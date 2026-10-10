//! Pure decisions on an incoming purchase.
//!
//! Snapshot in, verdict out. No clock, no I/O, no state mutated — the caller
//! applies the verdict to a [`GrantState`] if it says to.
//!
//! Admission control is only possible at all because the payer states up front
//! the rate it wants set aside. The provider sums the rates it has already
//! reserved across payers and refuses **before** taking the money, rather than
//! accepting payment and quietly shaping below what was promised.
//!
//! The gap between purchases is not checked here. It bounds how many signature
//! checks a payer can cause, so it is checked before any are made, by the host
//! and by the session ([`Sessions`](crate::session::Sessions)), ahead of this.

use alloc::vec::Vec;

use tollgate_protocol::{ChannelId, ChannelUpdate, ReasonCode};

use crate::config::GrantPolicy;
use crate::grant::state::GrantState;

/// Everything outside the payer's own state that bears on whether a purchase
/// can be honored.
#[derive(Debug, Clone, Copy)]
pub struct Admission<'a> {
    /// The terms advertised in the Offer, and the node-wide capacity.
    pub policy: &'a GrantPolicy,
    /// The rate already reserved by *other* payers, in units per second.
    /// Summed by the caller across its connected payers.
    pub reserved_elsewhere: u64,
}

impl Admission<'_> {
    /// The highest reserved rate this payer could have now: the node's
    /// capacity less what the others have reserved. `None` in the policy means
    /// "whatever the link will bear".
    pub fn rate_available(&self) -> u64 {
        match self.policy.max_rate {
            Some(max) => max.saturating_sub(self.reserved_elsewhere),
            None => u64::MAX,
        }
    }
}

/// What to do with an incoming purchase.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Honor it: apply it to the payer's state.
    Accept {
        /// Each channel's new cumulative total, ready to apply.
        ratchets: Vec<(ChannelId, u64)>,
        /// Units bought by this purchase alone — the **combined increase**
        /// across every channel it ratchets — to add to the budget.
        grant: u64,
    },
    /// Refuse it, in full.
    ///
    /// Declining to ratchet is already enough to leave the payer's money
    /// untouched — an unclaimed channel state is worth nothing to us. The
    /// refusal is sent so the payer learns in one round trip, and acts on the
    /// reason.
    Reject {
        /// Machine-readable cause.
        reason: ReasonCode,
        /// The highest reserved rate we would accept from this payer now.
        max_reserved_rate: u64,
    },
}

/// Decide whether to honor a purchase.
///
/// **All or nothing.** Every update must be valid, or the whole message is
/// refused: applying some of them would leave the grant a different size from
/// the one the payer asked for and paid for.
///
/// The checks:
///
/// 1. every update names a channel we recognise. A settled or replaced channel
///    is not one, which is what stops the recognised set growing with every
///    rollover a long session performs.
/// 2. every `cumulative` increases on its own channel. This is the ratchet, and
///    it is also what makes a replayed or reordered purchase harmless.
/// 3. no channel is signed past its capacity — it could not be settled.
/// 4. the window falls inside what we advertised.
/// 5. the reserved rate is at least the smallest we advertised.
/// 6. the reserved rate, with every other payer's, fits what we can carry.
///    Lowering a reservation is always accepted, so this is checked only for
///    one that rises.
pub fn evaluate_topup(
    state: &GrantState,
    admission: Admission<'_>,
    updates: &[ChannelUpdate],
    window_ms: u64,
    reserved_rate: u64,
) -> Verdict {
    let available = admission.rate_available();
    let refuse = |reason| Verdict::Reject {
        reason,
        max_reserved_rate: available,
    };

    if updates.is_empty() {
        return refuse(ReasonCode::GrantInvalid);
    }

    let mut ratchets = Vec::with_capacity(updates.len());
    let mut grant: u64 = 0;

    for update in updates {
        let Some(channel) = state.channel(update.channel_id) else {
            // Either never funded, or already settled. Neither is something we
            // can ratchet.
            return refuse(ReasonCode::FundingInvalid);
        };

        // A cumulative that does not increase is a replay or a reorder. It
        // buys nothing; the session answers it as a failed verification.
        if update.cumulative <= channel.signed {
            return refuse(ReasonCode::GrantInvalid);
        }
        if update.cumulative > channel.capacity {
            return refuse(ReasonCode::GrantExceedsChannel);
        }
        // The same channel twice would let the second reading of `signed` be
        // stale and the delta be counted from the wrong base.
        if ratchets.iter().any(|(id, _)| *id == update.channel_id) {
            return refuse(ReasonCode::GrantInvalid);
        }

        grant = grant.saturating_add(update.cumulative - channel.signed);
        ratchets.push((update.channel_id, update.cumulative));
    }

    if !admission.policy.window_acceptable(window_ms)
        || reserved_rate < admission.policy.min_reserved_rate
    {
        return refuse(ReasonCode::OutOfRange);
    }

    if reserved_rate > state.reserved_rate() && reserved_rate > available {
        return refuse(ReasonCode::RateExceedsCapacity);
    }

    Verdict::Accept { ratchets, grant }
}
