//! Minimal client side of the protocol, for integration testing:
//! - [`detect`] — send our Announce and report the peer's Announce back.
//! - [`pay`] — detect, then send a bootstrap Cashu token and report the ack.
//!
//! Intentionally small — pricing negotiation and metering are layered on later.

use anyhow::Context;

use std::str::FromStr;

use cashu::Amount;
use cashu::mint_url::MintUrl;
use cashu::nuts::nut00::TokenV3;
use cashu::nuts::{CurrencyUnit, Id, Proof, PublicKey as CashuPublicKey};
use cashu::secret::Secret;

use tollgate_protocol::{
    Announce, BootstrapAck, BootstrapToken, MessageType, PROTOCOL_VERSION, PublicKey,
    decode_frames, encode_frame, frame, peek_type,
};

use crate::config::Identity;

/// Outcome of a detection probe: the peer's identity as we learned it.
pub struct Detected {
    pub pubkey_hex: String,
    pub unit: String,
    pub version: u8,
}

/// Result of a bootstrap payment attempt.
pub struct Paid {
    pub peer_pubkey_hex: String,
    pub accepted: bool,
    pub reason: Option<String>,
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
    })
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
