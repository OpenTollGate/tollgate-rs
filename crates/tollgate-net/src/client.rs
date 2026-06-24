//! Client side of the protocol, for integration testing and manual play:
//! - [`detect`] — send our Announce and report the peer's Announce back.
//! - [`pay`] — detect, then send a bootstrap Cashu token and report the ack.
//! - [`consume`] — pay, then poll for MeteringReports and auto-top-up before the
//!   balance runs out, tracking remaining balance against the discovered price.

use anyhow::Context;

use std::str::FromStr;
use std::time::Duration;

use cashu::Amount;
use cashu::mint_url::MintUrl;
use cashu::nuts::nut00::TokenV3;
use cashu::nuts::{CurrencyUnit, Id, Proof, PublicKey as CashuPublicKey};
use cashu::secret::Secret;

use tollgate_core::Price;
use tollgate_protocol::{
    Announce, BootstrapAck, BootstrapToken, MessageType, MeteringReport, PROTOCOL_VERSION,
    PriceSheet, PublicKey, Reject, decode_frames, encode_frame, frame, peek_type,
};

use crate::config::Identity;

/// Scale factor between sats and the node's internal milli-unit balance.
const PRICING_SCALE: u64 = 1000;

/// Outcome of a detection probe: the peer's identity as we learned it, plus any
/// PriceSheet it advertised.
pub struct Detected {
    pub pubkey_hex: String,
    pub unit: String,
    pub version: u8,
    pub price_sheet: Option<PriceSheet>,
}

/// Result of a bootstrap payment attempt.
pub struct Paid {
    pub peer_pubkey_hex: String,
    pub accepted: bool,
    pub reason: Option<String>,
    pub price_sheet: Option<PriceSheet>,
}

/// POST `request` (already framed) to a peer's exchange endpoint and return the
/// decoded response message bodies.
async fn exchange(base_url: &str, request: Vec<u8>) -> anyhow::Result<Vec<Vec<u8>>> {
    let endpoint = format!("{}/tollgate/v1/exchange", base_url.trim_end_matches('/'));
    let resp = reqwest::Client::new()
        .post(&endpoint)
        .header("content-type", "application/cbor")
        .body(request)
        .send()
        .await
        .with_context(|| format!("posting to {endpoint}"))?
        .error_for_status()
        .context("peer returned an error status")?;
    let bytes = resp.bytes().await.context("reading peer response")?;
    let frames = decode_frames(&bytes).map_err(|e| anyhow::anyhow!("bad framing: {e:?}"))?;
    Ok(frames.into_iter().map(|f| f.to_vec()).collect())
}

/// Our own Announce, encoded.
fn our_announce(identity: &Identity, unit: &str) -> Vec<u8> {
    let pubkey = PublicKey::from_bytes(identity.public_key.serialize());
    Announce::new(PROTOCOL_VERSION, pubkey, unit, 0).encode()
}

/// Find and decode the first peer Announce in a set of message bodies.
fn find_announce(messages: &[Vec<u8>]) -> Option<Announce> {
    messages
        .iter()
        .filter(|m| matches!(peek_type(m), Some(MessageType::Announce)))
        .find_map(|m| Announce::decode(m).ok())
}

/// Find and decode the first PriceSheet in a set of message bodies.
fn find_price_sheet(messages: &[Vec<u8>]) -> Option<PriceSheet> {
    messages
        .iter()
        .filter(|m| matches!(peek_type(m), Some(MessageType::PriceSheet)))
        .find_map(|m| PriceSheet::decode(m).ok())
}

/// Find and decode the first MeteringReport in a set of message bodies.
fn find_metering_report(messages: &[Vec<u8>]) -> Option<MeteringReport> {
    messages
        .iter()
        .filter(|m| matches!(peek_type(m), Some(MessageType::MeteringReport)))
        .find_map(|m| MeteringReport::decode(m).ok())
}

/// Whether a balance-exhausted Reject is present in a set of message bodies.
fn balance_exhausted(messages: &[Vec<u8>]) -> bool {
    messages
        .iter()
        .filter(|m| matches!(peek_type(m), Some(MessageType::Reject)))
        .filter_map(|m| Reject::decode(m).ok())
        .any(|r| r.is_balance_exhausted())
}

/// The price the peer charges, taken from the first mint option of its first
/// product (v1 advertises a single rate). Zero if it offered no priced mints.
fn price_from_sheet(sheet: Option<&PriceSheet>) -> Price {
    sheet
        .and_then(|s| s.products.first())
        .and_then(|p| p.mints.first())
        .map(|m| Price {
            per_second: m.price_per_second,
            per_unit: m.price_per_unit,
        })
        .unwrap_or_default()
}

/// Send our Announce to `base_url` and return the peer's Announce.
pub async fn detect(base_url: &str, identity: &Identity, unit: &str) -> anyhow::Result<Detected> {
    let body = frame(&our_announce(identity, unit))
        .map_err(|e| anyhow::anyhow!("framing our Announce: {e:?}"))?;
    let messages = exchange(base_url, body).await?;
    let announce = find_announce(&messages).context("peer did not return an Announce")?;
    Ok(Detected {
        pubkey_hex: hex::encode(announce.public_key().as_bytes()),
        unit: announce.unit.clone(),
        version: announce.version,
        price_sheet: find_price_sheet(&messages),
    })
}

/// Detect the peer and pay a bootstrap token of `amount_sat`, drawn on `mint_url`.
/// Returns the peer identity and whether the bootstrap was accepted.
pub async fn pay(
    base_url: &str,
    identity: &Identity,
    unit: &str,
    mint_url: &str,
    amount_sat: u64,
) -> anyhow::Result<Paid> {
    let token = build_test_token(mint_url, amount_sat).context("building bootstrap token")?;

    // One exchange carrying both our Announce (establishes identity) and the
    // bootstrap token. The response should carry the peer's Announce and a
    // BootstrapAck.
    let mut body = Vec::new();
    let frame_err = |e| anyhow::anyhow!("framing: {e:?}");
    encode_frame(&our_announce(identity, unit), &mut body).map_err(frame_err)?;
    encode_frame(&BootstrapToken::new(token.into_bytes()).encode(), &mut body)
        .map_err(frame_err)?;

    let messages = exchange(base_url, body).await?;

    let peer_pubkey_hex = find_announce(&messages)
        .map(|a| hex::encode(a.public_key().as_bytes()))
        .context("peer did not return an Announce")?;

    let ack = messages
        .iter()
        .filter(|m| matches!(peek_type(m), Some(MessageType::BootstrapAck)))
        .find_map(|m| BootstrapAck::decode(m).ok())
        .context("peer did not return a BootstrapAck")?;

    Ok(Paid {
        peer_pubkey_hex,
        accepted: ack.is_accepted(),
        reason: ack.reason,
        price_sheet: find_price_sheet(&messages),
    })
}

/// Options for [`consume`].
pub struct ConsumeOpts {
    /// Sats in the initial bootstrap token.
    pub amount_sat: u64,
    /// Sats per top-up (also the low-balance watermark: top up when less than
    /// this much is left).
    pub topup_sat: u64,
    /// How often to poll the peer for MeteringReports.
    pub interval: Duration,
    /// Stop after this many polls (`None` = until the process is killed).
    pub max_polls: Option<u32>,
}

/// Pay a peer, then poll for MeteringReports and auto-top-up before the balance
/// runs out — the bootstrap-only "stay online" loop. Remaining balance is tracked
/// by applying the peer's advertised price to the usage it reports.
///
/// Emits greppable `CONSUME …` lines for each stage.
pub async fn consume(
    base_url: &str,
    identity: &Identity,
    unit: &str,
    mint_url: &str,
    opts: ConsumeOpts,
) -> anyhow::Result<()> {
    let paid = pay(base_url, identity, unit, mint_url, opts.amount_sat).await?;
    if !paid.accepted {
        anyhow::bail!(
            "initial bootstrap rejected: {}",
            paid.reason.as_deref().unwrap_or("unknown")
        );
    }
    let price = price_from_sheet(paid.price_sheet.as_ref());
    println!(
        "CONSUME start peer={} paid={} sat per_second={} per_unit={}",
        paid.peer_pubkey_hex, opts.amount_sat, price.per_second, price.per_unit
    );

    let topup_scaled = opts.topup_sat.saturating_mul(PRICING_SCALE);
    let mut paid_scaled = opts.amount_sat.saturating_mul(PRICING_SCALE);

    let mut poll = 0u32;
    while opts.max_polls.is_none_or(|max| poll < max) {
        poll += 1;
        tokio::time::sleep(opts.interval).await;

        // Poll: re-announce to receive any queued frames (MeteringReport, Reject).
        let body = frame(&our_announce(identity, unit))
            .map_err(|e| anyhow::anyhow!("framing our Announce: {e:?}"))?;
        let messages = exchange(base_url, body).await?;
        let cut_off = balance_exhausted(&messages);

        let remaining = match find_metering_report(&messages) {
            Some(report) => {
                let cost = price.cost_scaled(report.elapsed_ms, report.delivered);
                let remaining = paid_scaled.saturating_sub(cost);
                println!(
                    "CONSUME poll={poll} delivered={} elapsed_ms={} cost={} remaining={}",
                    report.delivered, report.elapsed_ms, cost, remaining
                );
                remaining
            }
            None => {
                println!(
                    "CONSUME poll={poll} (no report yet){}",
                    if cut_off { " cut-off" } else { "" }
                );
                paid_scaled // unknown usage; only top up if cut off
            }
        };

        if cut_off || remaining < topup_scaled {
            let top = pay(base_url, identity, unit, mint_url, opts.topup_sat).await?;
            if top.accepted {
                // A cut-off means the peer suspended us and reset its metering
                // baseline on re-admit, so reset our accounting too; a proactive
                // top-up just adds to the running total.
                paid_scaled = if cut_off {
                    topup_scaled
                } else {
                    paid_scaled.saturating_add(topup_scaled)
                };
                println!(
                    "CONSUME topup amount={} sat cut_off={cut_off} paid_scaled={paid_scaled}",
                    opts.topup_sat
                );
            } else {
                println!(
                    "CONSUME topup amount={} sat rejected={:?}",
                    opts.topup_sat, top.reason
                );
            }
        }
    }
    Ok(())
}

/// Build a syntactically valid cashuA token of `amount_sat` drawn on `mint_url`.
///
/// For the integration stub the proof's keyset/secret/signature are filler — the
/// fake mint accepts any proof as UNSPENT. What matters is that the token parses
/// and embeds `mint_url`, so the provider posts its NUT-07 check there.
fn build_test_token(mint_url: &str, amount_sat: u64) -> anyhow::Result<String> {
    let mint = MintUrl::from_str(mint_url).map_err(|e| anyhow::anyhow!("bad mint url: {e}"))?;
    // A valid v1 keyset id is "00" + 14 hex chars.
    let keyset_id =
        Id::from_str("009a1f293253e41e").map_err(|e| anyhow::anyhow!("keyset id: {e}"))?;
    // Filler unblinded signature point. Derive a guaranteed-valid compressed
    // secp256k1 point at runtime (the public key of secret scalar = 1, i.e. the
    // generator G) rather than trusting a literal through curve validation. The
    // fake mint ignores C's value; only that the token parses matters.
    let secp = secp256k1::Secp256k1::new();
    let one = secp256k1::SecretKey::from_slice(&{
        let mut b = [0u8; 32];
        b[31] = 1;
        b
    })
    .expect("scalar 1 is a valid secret key");
    let g = secp256k1::PublicKey::from_secret_key(&secp, &one);
    let c = CashuPublicKey::from_slice(&g.serialize())
        .map_err(|e| anyhow::anyhow!("filler C point: {e}"))?;

    let proof = Proof {
        amount: Amount::from(amount_sat),
        keyset_id,
        secret: Secret::generate(),
        c,
        witness: None,
        dleq: None,
        p2pk_e: None,
    };

    let token = TokenV3::new(mint, vec![proof], None, Some(CurrencyUnit::Sat))
        .map_err(|e| anyhow::anyhow!("building token: {e}"))?;
    Ok(token.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tollgate_protocol::{MintPrice, ProductOffer};

    fn sample_sheet(per_unit: i64) -> Vec<u8> {
        let prices = vec![MintPrice {
            mint_url: "http://m".to_string(),
            price_per_second: 0,
            price_per_unit: per_unit,
            mint_unit: "sat".to_string(),
        }];
        PriceSheet::new(vec![ProductOffer::new(1000, &prices, vec![])], 5000, 60000).encode()
    }

    #[test]
    fn find_price_sheet_picks_the_sheet_among_other_frames() {
        // A realistic response: Announce, PriceSheet, BootstrapAck.
        let messages = vec![
            Announce::new(1, PublicKey::from_bytes([1u8; 33]), "bytes", 0).encode(),
            sample_sheet(3),
            BootstrapAck::accepted().encode(),
        ];
        let found = find_price_sheet(&messages).expect("a PriceSheet among the frames");
        assert_eq!(found.products[0].mints[0].price_per_unit, 3);
    }

    #[test]
    fn find_price_sheet_returns_none_when_absent() {
        let messages = vec![
            Announce::new(1, PublicKey::from_bytes([1u8; 33]), "bytes", 0).encode(),
            BootstrapAck::accepted().encode(),
        ];
        assert!(find_price_sheet(&messages).is_none());
    }
}
