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

/// Resolve the host of a `base_url` like `http://peer:4747` to an IP, for
/// per-peer MAC metering of the upstream. Returns the first resolved address.
/// IPv4-oriented (the MAC counter reads `/proc/net/arp`); `None` on failure.
fn resolve_host_ip(base_url: &str) -> Option<std::net::IpAddr> {
    use std::net::ToSocketAddrs;
    let authority = base_url
        .trim_start_matches("http://")
        .trim_start_matches("https://")
        .split('/')
        .next()?;
    // ToSocketAddrs needs a port; default to the TollGate port if none is given.
    let with_port = if authority.contains(':') {
        authority.to_string()
    } else {
        format!("{authority}:4747")
    };
    with_port.to_socket_addrs().ok()?.next().map(|sa| sa.ip())
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
    /// Fault injection (testing): under-report the units we acknowledge receiving
    /// by this percentage, so the provider sees metering drift. 0 = report
    /// honestly. Used by the drift integration suite to exercise the cut-off path.
    pub understate_received_pct: u8,
    /// Uplink interface to meter for an *independent* receive-side count (e.g.
    /// `eth0`). When set, we report what we actually received over the link rather
    /// than echoing the provider's `delivered` — this is what surfaces real transit
    /// drift. `None` falls back to acknowledging the provider's count. Only correct
    /// when this upstream owns the interface; for shared links use `meter_upstream`.
    pub meter_iface: Option<String>,
    /// Meter this upstream by its next-hop MAC (a per-peer nftables counter),
    /// giving an independent receive-side count that stays correct even when
    /// several upstreams share one interface. Takes priority over `meter_iface`.
    /// See `docs/design/network-peering/peering-ip.md`.
    pub meter_upstream: bool,
}

/// The peer's latest MeteringReport, summarised. `delivered`/`received` are from
/// the *peer's* perspective: `delivered` is what the peer delivered to us.
pub struct ReportSummary {
    pub elapsed_ms: u64,
    pub delivered: u64,
    pub received: u64,
}

/// One observation from the consume loop, handed to its callback each poll.
pub struct ConsumeEvent {
    /// 0 for the initial "start" event, then 1, 2, … per poll.
    pub poll: u32,
    pub peer_pubkey: String,
    pub price: Price,
    pub paid_scaled: u64,
    /// Signed remaining balance: `+` we have prepaid credit left, `−` the
    /// provider owes us (it delivers at a negative price — it pays us).
    pub remaining_scaled: i64,
    /// The peer's latest report, if one arrived this poll.
    pub report: Option<ReportSummary>,
    /// The peer reported us balance-exhausted (cut off).
    pub cut_off: bool,
    /// We sent a top-up this poll.
    pub topped_up: bool,
}

/// Pay a peer, then poll for MeteringReports and auto-top-up before the balance
/// runs out — the bootstrap-only "stay online" loop. Remaining balance is tracked
/// by applying the peer's advertised price to the usage it reports. `on_event` is
/// called once per poll (and once at start) so callers can print or record state.
pub async fn run_consume(
    base_url: &str,
    identity: &Identity,
    unit: &str,
    mint_url: &str,
    opts: ConsumeOpts,
    mut on_event: impl FnMut(&ConsumeEvent),
) -> anyhow::Result<()> {
    let paid = pay(base_url, identity, unit, mint_url, opts.amount_sat).await?;
    if !paid.accepted {
        anyhow::bail!(
            "initial bootstrap rejected: {}",
            paid.reason.as_deref().unwrap_or("unknown")
        );
    }
    let price = price_from_sheet(paid.price_sheet.as_ref());
    let peer_pubkey = paid.peer_pubkey_hex;
    let topup_scaled = opts.topup_sat.saturating_mul(PRICING_SCALE);
    let mut paid_scaled = opts.amount_sat.saturating_mul(PRICING_SCALE);

    on_event(&ConsumeEvent {
        poll: 0,
        peer_pubkey: peer_pubkey.clone(),
        price,
        paid_scaled,
        remaining_scaled: paid_scaled as i64,
        report: None,
        cut_off: false,
        topped_up: false,
    });

    let mut poll = 0u32;
    // Cumulative units we acknowledge having received from the peer — the
    // consumer's half of the bidirectional metering exchange. We report it back so
    // the provider can reconcile it against its own `delivered` and detect drift.
    let mut acked_received: u64 = 0;
    // Independent receive-side measurement source, in priority order: a per-peer
    // next-hop MAC counter (correct even when upstreams share an interface), else a
    // dedicated uplink interface, else none (echo the provider's count). See
    // docs/design/network-peering/peering-ip.md.
    let upstream_ip = opts
        .meter_upstream
        .then(|| resolve_host_ip(base_url))
        .flatten();
    let raw_received = |upstream_ip: Option<std::net::IpAddr>| -> u64 {
        if let Some(ip) = upstream_ip {
            crate::adapter::read_upstream_received(ip)
        } else if let Some(iface) = opts.meter_iface.as_deref() {
            crate::adapter::read_iface_counters(iface).received
        } else {
            0
        }
    };
    // Counters are cumulative since boot/install; subtract this baseline to get
    // bytes since session start (re-captured on re-admit below).
    let mut meter_baseline: u64 = raw_received(upstream_ip);
    while opts.max_polls.is_none_or(|max| poll < max) {
        poll += 1;
        tokio::time::sleep(opts.interval).await;

        // Poll: re-announce to receive any queued frames (MeteringReport, Reject),
        // and send back our own MeteringReport with what we've received so far — the
        // consumer's half of the bidirectional exchange. With an uplink meter that's
        // an independent measurement (surfacing real transit drift); otherwise we
        // acknowledge the provider's own count. Either way it closes the loop so the
        // provider's drift/settlement path runs end to end.
        let mut body = Vec::new();
        encode_frame(&our_announce(identity, unit), &mut body)
            .map_err(|e| anyhow::anyhow!("framing our Announce: {e:?}"))?;
        if acked_received > 0 {
            let ack = MeteringReport::new(0, 0, acked_received).encode();
            encode_frame(&ack, &mut body)
                .map_err(|e| anyhow::anyhow!("framing our MeteringReport: {e:?}"))?;
        }
        let messages = exchange(base_url, body).await?;
        let cut_off = balance_exhausted(&messages);
        if cut_off {
            // Re-admit resets the provider's metering baseline, so restart ours too
            // — otherwise we'd report a stale (large) count against the provider's
            // fresh (small) one and trip a false drift alarm.
            acked_received = 0;
            meter_baseline = raw_received(upstream_ip);
        }

        let report = find_metering_report(&messages);
        // Update what we'll acknowledge next poll. Fault injection deflates it to
        // simulate a peer that lies or under-counts.
        let keep = 100u64.saturating_sub(opts.understate_received_pct.min(100) as u64);
        if upstream_ip.is_some() || opts.meter_iface.is_some() {
            // Independent receive-side measurement (per-peer MAC counter or uplink
            // interface) since the session baseline.
            if let Some(ip) = upstream_ip {
                crate::adapter::ensure_upstream_counter(ip);
            }
            let measured = raw_received(upstream_ip).saturating_sub(meter_baseline);
            acked_received = measured.saturating_mul(keep) / 100;
        } else if let Some(r) = report.as_ref() {
            // Fallback: acknowledge the provider's cumulative delivered (self-healing
            // — the latest total wins).
            acked_received = r.delivered.saturating_mul(keep) / 100;
        }
        let cost = report
            .as_ref()
            .map_or(0, |r| price.cost_scaled(r.elapsed_ms, r.delivered));

        // Top up if cut off, or proactively before the balance dips below a top-up
        // — the watermark policy described under "Proactive Top-Up" in
        // docs/design/core/tollgate-bootstrap.md. Signed: under negative pricing
        // `cost` is negative (the provider pays us), so remaining only grows and we
        // never top up.
        let mut topped_up = false;
        if cut_off || (paid_scaled as i64).saturating_sub(cost) < topup_scaled as i64 {
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
                topped_up = true;
            }
        }

        on_event(&ConsumeEvent {
            poll,
            peer_pubkey: peer_pubkey.clone(),
            price,
            paid_scaled,
            remaining_scaled: (paid_scaled as i64).saturating_sub(cost),
            report: report.map(|r| ReportSummary {
                elapsed_ms: r.elapsed_ms,
                delivered: r.delivered,
                received: r.received,
            }),
            cut_off,
            topped_up,
        });
    }
    Ok(())
}

/// CLI `consume`: run the loop, printing greppable `CONSUME …` lines.
pub async fn consume(
    base_url: &str,
    identity: &Identity,
    unit: &str,
    mint_url: &str,
    opts: ConsumeOpts,
) -> anyhow::Result<()> {
    let topup_sat = opts.topup_sat;
    run_consume(base_url, identity, unit, mint_url, opts, move |ev| {
        if ev.poll == 0 {
            println!(
                "CONSUME start peer={} paid={} sat per_second={} per_unit={}",
                ev.peer_pubkey,
                ev.paid_scaled / PRICING_SCALE,
                ev.price.per_second,
                ev.price.per_unit
            );
            return;
        }
        match &ev.report {
            Some(r) => println!(
                "CONSUME poll={} delivered={} elapsed_ms={} remaining={}",
                ev.poll, r.delivered, r.elapsed_ms, ev.remaining_scaled
            ),
            None => println!(
                "CONSUME poll={} (no report yet){}",
                ev.poll,
                if ev.cut_off { " cut-off" } else { "" }
            ),
        }
        if ev.topped_up {
            println!(
                "CONSUME topup amount={topup_sat} sat cut_off={} paid_scaled={}",
                ev.cut_off, ev.paid_scaled
            );
        }
    })
    .await
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
