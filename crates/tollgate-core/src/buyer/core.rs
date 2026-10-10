//! The pure buy/hold decision.
//!
//! Snapshot in, decision out. The host measures demand and supplies the clock;
//! this decides only whether to buy, at what reserved rate, and how much.
//!
//! Nothing is forfeit at a purchase, so there is nothing to time and no reason
//! to hold one back. The rule:
//!
//! - **What to reserve.** Following demand, with headroom, within the
//!   operator's bounds and never below the provider's smallest — or nothing,
//!   for a buyer that pays per use.
//! - **How much.** Whatever has drained since the last purchase: each one
//!   brings the budget back up to the size wanted — the reserved rate times
//!   the window, or the fixed budget of a pay-per-use buyer — and never
//!   further.
//! - **When.** Before the budget, at the rate it is drawn, or the deadline
//!   runs out; at once when demand outgrows the rate reserved, or when a
//!   session resumed with a budget has to reserve again; and never sooner
//!   after the last purchase than the provider's gap. Only while something
//!   wants the link, or a reservation is being made again.

use crate::buyer::state::{Buyer, BuyerPolicy, Leg, Purchase, Terms, Trigger};
use crate::grant::{budget_for, units_in};
use crate::time::Millis;

/// What the host observed, and the terms of the provider it would buy from.
#[derive(Debug, Clone, Copy)]
pub struct Demand {
    /// Units per second this node wants to move over this link, as the
    /// provider will draw them: what it pulls, plus what it pushes weighted by
    /// the provider's from-payer weight.
    ///
    /// How it is measured is the host's business — offered load, recent
    /// throughput, queue depth.
    pub observed_rate: u64,
    /// The provider's terms, from its Offer.
    pub terms: Terms,
}

/// Decide whether to buy.
///
/// Returns `None` to hold. The caller sends the TopUp and then calls
/// [`Buyer::record`] — the two are separate so a host that fails to send does
/// not advance its own ratchet past what the provider saw.
pub fn poll(buyer: &Buyer, policy: &BuyerPolicy, demand: Demand, now: Millis) -> Option<Purchase> {
    let active = buyer.active()?;
    let terms = demand.terms;

    // Only while something wants the link: observed demand, or the standing
    // demand the operator set — or a reservation the last session held, which
    // a session resumed with a budget makes again at once. Until it does, the
    // provider carries it as a payer that reserved nothing, and demand on the
    // new link may not have been observed yet.
    let wanted_rate = demand.observed_rate.max(policy.demand);
    let resume = if policy.reserve {
        buyer.resume_rate(now)
    } else {
        0
    };
    if wanted_rate == 0 && resume == 0 {
        return None;
    }

    // Never sooner than the provider accepts. It would be refused unread.
    if buyer
        .last_topup()
        .is_some_and(|last| now.saturating_since(last) < terms.min_topup_gap_ms)
    {
        return None;
    }

    let reserve = if wanted_rate > 0 {
        reserved_rate(buyer, policy, terms, wanted_rate, now)?
    } else {
        fit_rate(buyer, policy, terms, resume, now)?
    };
    let window_ms = terms.clamp_window(policy.window_ms);
    let size = if policy.reserve {
        budget_for(reserve, window_ms)
    } else {
        policy.budget
    };

    let remaining = buyer.remaining_at(now);
    // Against the window actually in force, not the one asked for: a provider
    // that bounds the window shorter than the configured lead would otherwise
    // put the buyer in a loop.
    let lead = policy.lead_within(window_ms);
    let draining = buyer.reserved().max(demand.observed_rate);
    let running_out = remaining <= units_in(draining, lead) || now + lead >= buyer.deadline();

    let trigger = if reserve > buyer.reserved() && wanted_rate.max(resume) > buyer.reserved() {
        if !buyer.started {
            Trigger::First
        } else if buyer.cap(now).is_some() {
            Trigger::Rebuy
        } else {
            Trigger::RateRose
        }
    } else if running_out {
        if buyer.started {
            Trigger::Renewal
        } else {
            Trigger::First
        }
    } else {
        return None;
    };

    // What has drained since the last purchase. A TopUp has to add at least
    // one unit, since a cumulative total must rise, so one that only changes
    // the reserved rate, or only keeps the deadline, buys a little.
    let wanted = size.saturating_sub(remaining).max(1);

    // A grant that overflows the channel in use is signed across both: the
    // first is topped to exactly its capacity, and the remainder starts the
    // replacement. A cumulative total only means anything against the channel
    // it was signed on, so this cannot be one update.
    let (first, second, grant) = if wanted <= active.headroom() {
        (
            Leg {
                channel_id: active.id,
                cumulative: active.cumulative + wanted,
            },
            None,
            wanted,
        )
    } else if let Some(next) = buyer.next_channel() {
        let overflow = (wanted - active.headroom()).min(next.headroom());
        (
            Leg {
                channel_id: active.id,
                cumulative: active.capacity,
            },
            Some(Leg {
                channel_id: next.id,
                cumulative: next.cumulative + overflow,
            }),
            active.headroom() + overflow,
        )
    } else {
        // No replacement confirmed yet. Buy what the channel in use will still
        // take; the rollover that is already under way opens the rest.
        (
            Leg {
                channel_id: active.id,
                cumulative: active.capacity,
            },
            None,
            active.headroom(),
        )
    };

    // Buying nothing is not a purchase: a channel with no headroom left and
    // no replacement.
    if grant == 0 {
        return None;
    }

    Some(Purchase {
        first,
        second,
        window_ms,
        reserved_rate: reserve,
        grant,
        trigger,
    })
}

/// The rate to reserve, given what is wanted, the operator's bounds, the
/// provider's smallest, and any ceiling the provider has told us about.
///
/// `None` when nothing fits: the provider's smallest is above what the
/// operator will reserve, or above the ceiling the provider named.
fn reserved_rate(
    buyer: &Buyer,
    policy: &BuyerPolicy,
    terms: Terms,
    wanted: u64,
    now: Millis,
) -> Option<u64> {
    let rate = if policy.reserve {
        let with_headroom = (wanted as u128) * (policy.headroom_pct as u128) / 100;
        with_headroom.min(u64::MAX as u128) as u64
    } else {
        0
    };
    fit_rate(buyer, policy, terms, rate, now)
}

/// Fit a reserved rate to the operator's bounds, the provider's smallest, and
/// any ceiling the provider has told us about.
///
/// `None` when nothing fits.
fn fit_rate(
    buyer: &Buyer,
    policy: &BuyerPolicy,
    terms: Terms,
    rate: u64,
    now: Millis,
) -> Option<u64> {
    let rate = if policy.reserve {
        rate.max(policy.min_rate)
    } else {
        0
    };
    let mut rate = rate.max(terms.min_reserved_rate).min(policy.max_rate);

    // A provider that refused us has already said what it will take. Asking
    // for more again would just be refused again.
    if let Some(cap) = buyer.cap(now) {
        rate = rate.min(cap);
    }
    // A buyer of time at a speed that is told nothing is free holds there
    // rather than buying a budget of nothing, until the cap lapses.
    if policy.reserve && rate == 0 {
        return None;
    }
    (rate >= terms.min_reserved_rate).then_some(rate)
}
