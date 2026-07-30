//! The pure buy/hold decision.
//!
//! Snapshot in, decision out. The host measures demand and supplies the clock;
//! this decides only whether to buy and how much.
//!
//! The rule is deliberately rudimentary — three cases, no control theory:
//!
//! 1. **Nothing bought yet** — buy for the demand we see.
//! 2. **The grant is about to lapse** — renew. Costs nothing; the remainder was
//!    nearly gone anyway.
//! 3. **Demand climbed a lot** — buy now and eat the forfeit. Only past a
//!    threshold, because raising the rate early burns whatever is left of the
//!    grant in force. The design's own table is the argument for hysteresis:
//!    a large jump costs ~2.5% of the new grant, a small one ~67%.
//!
//! Anything else holds.

use crate::buyer::state::{Buyer, BuyerPolicy, Purchase, Trigger, WindowBounds};
use crate::grant::grant_for;
use crate::time::Millis;

/// What the host observed since the last decision.
#[derive(Debug, Clone, Copy)]
pub struct Demand {
    /// Units per second the host currently wants to move over this link.
    ///
    /// How it is measured is the host's business — offered load, recent
    /// throughput, queue depth. The demo measures what the traffic generator is
    /// asking for.
    pub observed_rate: u64,
    /// Window bounds the provider advertised in its Offer.
    pub bounds: WindowBounds,
}

/// Decide whether to buy.
///
/// Returns `None` to hold. The caller sends the returned purchase as a TopUp
/// and then calls [`Buyer::record`] — the two are separate so a host that fails
/// to send does not advance its own ratchet past what the provider saw.
pub fn poll(
    buyer: &Buyer,
    policy: &BuyerPolicy,
    demand: Demand,
    now: Millis,
) -> Option<Purchase> {
    let target = target_rate(buyer, policy, demand.observed_rate);
    let window_ms = demand.bounds.clamp(policy.window_ms);

    let trigger = if !buyer.started {
        Trigger::First
    } else if buyer.capped_at.is_some() && target != buyer.rate {
        // The provider named a rate it would take; go straight back with it
        // rather than waiting out a grant we never got.
        Trigger::Rebuy
    } else if now + policy.renew_lead_ms as u64 >= buyer.deadline {
        Trigger::Renewal
    } else if worth_the_forfeit(buyer.rate, target, policy) {
        Trigger::DemandRose
    } else {
        return None;
    };

    // Buying nothing is not a purchase. This also stops an idle link with
    // `min_rate: 0` from emitting a TopUp every renewal interval forever.
    if target == 0 {
        return None;
    }

    let grant = grant_for(target, window_ms);
    Some(Purchase {
        cumulative: buyer.cumulative.saturating_add(grant),
        window_ms,
        rate: target,
        grant,
        forfeited: buyer.unspent_at(now),
        trigger,
    })
}

/// The rate we would like, given demand, headroom, and any ceiling the provider
/// has told us about.
fn target_rate(buyer: &Buyer, policy: &BuyerPolicy, observed: u64) -> u64 {
    let with_headroom = (observed as u128) * (policy.headroom_pct as u128) / 100;
    let mut target = with_headroom.min(u64::MAX as u128) as u64;

    target = target.clamp(policy.min_rate, policy.max_rate);

    // A provider that refused us has already said what it will take. Asking for
    // more again would just be refused again.
    if let Some(cap) = buyer.capped_at {
        target = target.min(cap);
    }
    target
}

/// Whether the jump from `current` to `target` clears the hysteresis threshold.
///
/// Only upward: demand falling is not a reason to buy at all, since the cheaper
/// grant would still forfeit the expensive one's remainder. We simply let the
/// current grant run out and renew lower.
fn worth_the_forfeit(current: u64, target: u64, policy: &BuyerPolicy) -> bool {
    if target <= current {
        return false;
    }
    let scaled = (current as u128) * (policy.raise_threshold_pct as u128) / 100;
    (target as u128) >= scaled
}
