//! Payment channels, behind a trait.
//!
//! Channels exist to keep the issuer's spent-proof set bounded, not to prevent
//! theft: when the provider is the mint for its own vouchers, the only party
//! who could cheat is also the party who would have to honor the refund. What
//! bounds that is exposure and reputation, not cryptography. Without channels
//! the spent-proof set is roughly 700× larger, which ends a session within
//! hours on hardware with 16–128 MB of flash.
//!
//! The trait is where a real Cashu Spilman implementation plugs in. It is
//! deliberately synchronous: the reference implementation's own in-memory mint
//! path is synchronous too, and the node drives every call through
//! `spawn_blocking`, feeding the result back as an [`Event`] — which is exactly
//! the seam core's action/event split exists to provide.
//!
//! [`Event`]: tollgate_core::Event

use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use tollgate_core::Millis;
use tollgate_protocol::{ChannelId, PubKey, Signature};

mod local;
mod spilman;

pub use local::LocalChannels;
pub use spilman::{Money, SpilmanChannels, SpilmanConfig};

/// A channel we funded, ready to tell the peer about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FundedChannel {
    /// The channel.
    pub channel_id: ChannelId,
    /// Units it can carry before it must roll over.
    pub capacity: u64,
    /// Unix time, in seconds, after which we can reclaim it through the refund
    /// path. `None` for a backend with no refund path.
    pub expiry: Option<u64>,
    /// Opaque blob for the peer's Accept or RolloverInit. Only the backend on
    /// the other side has to understand it.
    pub funding: Vec<u8>,
}

/// A peer's funding, once we have checked it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedChannel {
    /// The channel they will pay us on.
    pub channel_id: ChannelId,
    /// Units it can carry.
    pub capacity: u64,
    /// The mint it is funded in, as the funding names it. Core refuses the
    /// channel unless this is one of our accepted mints.
    pub mint_url: String,
    /// Unix time, in seconds, after which the peer can reclaim it — and with
    /// it anything we earned on it and have not settled. `None` for a backend
    /// with no refund path.
    pub expiry: Option<u64>,
}

/// Put a channel's wall-clock expiry on the monotonic clock core runs on.
///
/// The refund timelock is an absolute timestamp, because the mint enforces it
/// and the mint knows nothing of this node's clock. Core never reads a clock,
/// so the host measures how far away the expiry is and hands core `now` plus
/// that. Measuring after the backend returned errs early, which is the safe
/// direction.
pub fn expires_at(expiry: u64, now: Millis) -> Millis {
    let wall_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    now + expiry.saturating_mul(1_000).saturating_sub(wall_ms)
}

/// A backend's refusal of funding in a mint we do not accept.
///
/// A backend that looks at the mint before it has verified anything else —
/// Spilman must, before it fetches a keyset from the URL the peer named —
/// returns this, so the peer is told [`MintNotAccepted`] rather than a generic
/// funding failure.
///
/// [`MintNotAccepted`]: tollgate_protocol::ReasonCode::MintNotAccepted
#[derive(Debug, thiserror::Error)]
#[error("{0} is not a mint we take payment in")]
pub struct MintNotAccepted(pub String);

/// A settlement that no retry can make succeed.
///
/// The node retries a failed [`ChannelBackend::settle`] until it works, which
/// is right for a mint that is briefly unreachable and pure noise for a
/// channel the backend has never seen or one its funder has already
/// reclaimed. A backend returns this for those, and the node stops.
#[derive(Debug, thiserror::Error)]
#[error("cannot settle: {0}")]
pub struct CannotSettle(pub String);

/// What a payment-channel implementation has to provide.
///
/// Signing lives here rather than beside the node's identity because **what a
/// channel update commits to is the channel scheme's business**. An ad-hoc
/// Schnorr over `(channel_id, cumulative)` and a Cashu Spilman balance-update
/// signature cover different preimages entirely, and core treats the bytes as
/// opaque precisely so that either can be dropped in.
pub trait ChannelBackend: Send + Sync + std::fmt::Debug {
    /// Fund a channel to pay `peer` on, against `mint_url`.
    fn fund(&self, peer: PubKey, mint_url: &str, capacity: u64) -> Result<FundedChannel>;

    /// Check funding a peer sent us and return the channel it opens.
    fn verify(&self, peer: PubKey, funding: &[u8]) -> Result<VerifiedChannel>;

    /// Sign a ratchet turn on a channel we fund.
    fn sign_update(&self, channel_id: ChannelId, cumulative: u64) -> Result<Signature>;

    /// Check a peer's ratchet turn on a channel it funds, **without keeping
    /// it**.
    ///
    /// Called on every update in a TopUp before the message reaches core, which
    /// trusts what it is handed. It must not change what the backend has
    /// recorded: a purchase is honored or refused as a whole, so an update that
    /// verifies can still belong to one that is refused — because another
    /// update in it did not verify, or because core declined it.
    fn verify_update(
        &self,
        peer: PubKey,
        channel_id: ChannelId,
        cumulative: u64,
        signature: Signature,
    ) -> bool;

    /// Keep a peer's ratchet turn as the channel's latest signed state — the
    /// one [`Self::settle`] submits.
    ///
    /// Called only for updates that passed [`Self::verify_update`] and belong
    /// to a purchase core accepted, when it emits [`Action::RecordUpdates`].
    ///
    /// [`Action::RecordUpdates`]: tollgate_core::Action::RecordUpdates
    fn record_update(
        &self,
        peer: PubKey,
        channel_id: ChannelId,
        cumulative: u64,
        signature: Signature,
    ) -> Result<()>;

    /// Settle a channel: submit the latest signed state, reclaim the change.
    ///
    /// The node retries this until it succeeds, so it must be **idempotent**:
    /// settling a channel that has already settled returns `Ok` and moves
    /// nothing. An error the node should not retry — a channel never seen,
    /// nothing left to claim — is a [`CannotSettle`]; any other error is taken
    /// to be transient.
    fn settle(&self, channel_id: ChannelId) -> Result<()>;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wall_seconds() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("after the epoch")
            .as_secs()
    }

    #[test]
    fn an_expiry_lands_as_far_ahead_on_the_node_clock_as_on_the_wall() {
        let now = Millis(5_000);
        let at = expires_at(wall_seconds() + 3_600, now);
        // Never late, and early by no more than the second the wall clock was
        // rounded down to plus the time the test took.
        assert!(at <= now + 3_600_000, "{at:?}");
        assert!(at >= now + 3_598_000, "{at:?}");
    }

    #[test]
    fn an_expiry_already_past_is_due_now_rather_than_wrapping() {
        let now = Millis(5_000);
        assert_eq!(expires_at(wall_seconds() - 60, now), now);
        assert_eq!(expires_at(0, now), now);
    }

    #[test]
    fn an_absurdly_distant_expiry_saturates_rather_than_overflowing() {
        // Far beyond any margin the channel could be settled in, and no panic
        // on the way there.
        assert!(expires_at(u64::MAX, Millis(5_000)) > Millis(u64::MAX / 2));
    }
}
