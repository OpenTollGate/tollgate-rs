//! Minimal client side of the handshake: send our Announce to a peer's
//! `/tollgate/v1/exchange` endpoint and report the peer's Announce back.
//!
//! This is the "can two nodes detect each other" path. It is intentionally
//! small — pricing, payment, and metering are layered on later.

use anyhow::{Context, bail};

use tollgate_protocol::{
    Announce, MessageType, PROTOCOL_VERSION, PublicKey, decode_frames, frame, peek_type,
};

use crate::config::Identity;

/// Outcome of a detection probe: the peer's identity as we learned it.
pub struct Detected {
    pub pubkey_hex: String,
    pub unit: String,
    pub version: u8,
}

/// Send our Announce to `base_url` and return the peer's Announce.
///
/// `base_url` is the peer's HTTP origin, e.g. `http://gateway:4747`.
pub async fn detect(base_url: &str, identity: &Identity, unit: &str) -> anyhow::Result<Detected> {
    let endpoint = format!("{}/tollgate/v1/exchange", base_url.trim_end_matches('/'));

    let pubkey = PublicKey::from_bytes(identity.public_key.serialize());
    let our_announce = Announce::new(PROTOCOL_VERSION, pubkey, unit, 0).encode();
    let body = frame(&our_announce).map_err(|e| anyhow::anyhow!("framing our Announce: {e:?}"))?;

    let client = reqwest::Client::new();
    let resp = client
        .post(&endpoint)
        .header("content-type", "application/cbor")
        .body(body)
        .send()
        .await
        .with_context(|| format!("posting to {endpoint}"))?
        .error_for_status()
        .context("peer returned an error status")?;

    let bytes = resp.bytes().await.context("reading peer response")?;
    let frames = decode_frames(&bytes).map_err(|e| anyhow::anyhow!("bad framing: {e:?}"))?;

    for f in frames {
        if let Some(MessageType::Announce) = peek_type(f) {
            let announce =
                Announce::decode(f).map_err(|e| anyhow::anyhow!("decoding peer Announce: {e}"))?;
            return Ok(Detected {
                pubkey_hex: hex::encode(announce.public_key().as_bytes()),
                unit: announce.unit.clone(),
                version: announce.version,
            });
        }
    }

    bail!("peer did not return an Announce");
}
