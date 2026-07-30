//! Pure admission decisions on an incoming TopUp.
//!
//! Snapshot in, verdict out. No clock, no I/O, no state mutated — the caller
//! applies the verdict to a [`GrantState`] if it says to.
//!
//! Admission control is only possible at all because the payer states the rate
//! it wants up front, for a bounded horizon. The provider can sum the rates it
//! has already committed across peers and refuse **before** taking the money,
//! rather than accepting payment and quietly shaping below what was sold.

use tollgate_protocol::ReasonCode;

use crate::config::GrantPolicy;
use crate::grant::limits;
use crate::grant::state::GrantState;

/// Everything outside the peer's own grant state that bears on whether a TopUp
/// can be honored.
#[derive(Debug, Clone, Copy)]
pub struct Admission<'a> {
    /// Window bounds and the per-peer rate ceiling.
    pub policy: &'a GrantPolicy,
    /// Rate already committed to *other* peers, in units per second. Summed by
    /// the host across its live grants.
    pub committed_elsewhere: u64,
    /// Units still spendable on the channel this update ratchets. A grant that
    /// runs past it cannot be settled, so it is refused rather than accepted
    /// and then dishonored.
    pub channel_capacity: u64,
}

impl Admission<'_> {
    /// Rate we could still commit to this peer, given what is committed
    /// elsewhere. `None` in the policy means "whatever the link will bear".
    pub fn rate_available(&self) -> u64 {
        match self.policy.max_rate {
            Some(max) => max.saturating_sub(self.committed_elsewhere),
            None => u64::MAX,
        }
    }
}

/// What to do with an incoming TopUp.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Honor it. Apply to the grant state and start shaping at `rate`.
    Accept {
        /// Units bought by this purchase alone, `cumulative - authorized`.
        grant: u64,
        /// Units per second it buys, fixed for the life of the grant.
        rate: u64,
    },
    /// Refuse it.
    ///
    /// Declining to ratchet is already enough to leave the payer's money
    /// untouched — an unclaimed channel state is worth nothing to us. The
    /// refusal is sent so the payer learns in one round trip instead of
    /// inferring it from throughput that never arrived.
    Reject {
        /// Machine-readable cause.
        reason: ReasonCode,
        /// Rate we *would* accept, so the payer can re-purchase immediately
        /// instead of guessing.
        max_rate_available: u64,
    },
}

/// Decide whether to honor a TopUp.
///
/// The checks, in the order the protocol specifies them:
///
/// 1. `cumulative > authorized` — the ratchet only turns forwards. This is also
///    what makes a replayed or reordered TopUp harmless.
/// 2. the window falls inside what we advertised.
/// 3. the rate fits under what we have left to commit.
/// 4. the new total fits inside the channel's capacity.
pub fn evaluate_topup(
    state: &GrantState,
    admission: Admission<'_>,
    cumulative: u64,
    window_ms: u32,
) -> Verdict {
    let available = admission.rate_available();

    // A cumulative that does not increase is a replay or a reorder. Discarding
    // it is what lets TopUp be fire-and-forget in the first place.
    if cumulative <= state.authorized() {
        return Verdict::Reject {
            reason: ReasonCode::GrantInvalid,
            max_rate_available: available,
        };
    }

    if !admission.policy.window_acceptable(window_ms) {
        return Verdict::Reject {
            reason: ReasonCode::WindowOutOfRange,
            max_rate_available: available,
        };
    }

    let grant = cumulative - state.authorized();
    let rate = limits::rate_from(grant, window_ms);

    if rate > available {
        return Verdict::Reject {
            reason: ReasonCode::RateExceedsCapacity,
            max_rate_available: available,
        };
    }

    if cumulative > admission.channel_capacity {
        return Verdict::Reject {
            reason: ReasonCode::GrantExceedsChannel,
            max_rate_available: available,
        };
    }

    Verdict::Accept { grant, rate }
}
