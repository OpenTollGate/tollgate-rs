//! The buyer algorithm: buys when demand justifies it, holds when the forfeit
//! would not be worth it, and never asks for more than the provider said it
//! would take.

use super::*;
use crate::grant::grant_for;
use crate::time::Millis;

fn bounds() -> WindowBounds {
    WindowBounds {
        min_ms: 200,
        max_ms: 30_000,
    }
}

fn policy() -> BuyerPolicy {
    BuyerPolicy {
        headroom_pct: 125,
        raise_threshold_pct: 150,
        renew_lead_ms: 500,
        window_ms: 2_000,
        min_rate: 0,
        max_rate: u64::MAX,
    }
}

fn demand(rate: u64) -> Demand {
    Demand {
        observed_rate: rate,
        bounds: bounds(),
    }
}

#[test]
fn the_first_purchase_covers_demand_plus_headroom() {
    let buyer = Buyer::new();
    let p = poll(&buyer, &policy(), demand(100_000), Millis(0)).expect("should buy");

    assert_eq!(p.trigger, Trigger::First);
    assert_eq!(p.rate, 125_000, "100 k/s demand at 125% headroom");
    assert_eq!(p.window_ms, 2_000);
    assert_eq!(p.grant, grant_for(125_000, 2_000));
    assert_eq!(p.cumulative, p.grant, "first purchase starts the ratchet");
    assert_eq!(p.forfeited, 0, "nothing to give up");
}

#[test]
fn steady_demand_mid_window_holds() {
    // Re-buying the same rate would forfeit the remainder for nothing.
    let mut buyer = Buyer::new();
    let p = poll(&buyer, &policy(), demand(100_000), Millis(0)).expect("should buy");
    buyer.record(p, Millis(0));

    for t in [100, 500, 1_000, 1_400] {
        assert!(
            poll(&buyer, &policy(), demand(100_000), Millis(t)).is_none(),
            "should hold at t={t}"
        );
    }
}

#[test]
fn the_grant_is_renewed_before_it_lapses() {
    // Renewing early is what keeps the peer off the minimum flow allowance.
    let mut buyer = Buyer::new();
    let p = poll(&buyer, &policy(), demand(100_000), Millis(0)).expect("should buy");
    buyer.record(p, Millis(0));
    assert_eq!(buyer.deadline(), Millis(2_000));

    assert!(poll(&buyer, &policy(), demand(100_000), Millis(1_499)).is_none());

    let p = poll(&buyer, &policy(), demand(100_000), Millis(1_500)).expect("should renew");
    assert_eq!(p.trigger, Trigger::Renewal);
    assert_eq!(p.cumulative, buyer.cumulative() + p.grant, "monotonic");
}

#[test]
fn a_large_demand_spike_is_worth_the_forfeit() {
    let mut buyer = Buyer::new();
    let p = poll(&buyer, &policy(), demand(1_000_000), Millis(0)).expect("should buy");
    buyer.record(p, Millis(0));
    assert_eq!(buyer.rate(), 1_250_000);

    // Demand jumps 16×. The remainder burns, but the design's own arithmetic
    // says a large jump costs only a few percent of the new grant.
    let p = poll(&buyer, &policy(), demand(16_000_000), Millis(500)).expect("should buy");
    assert_eq!(p.trigger, Trigger::DemandRose);
    assert_eq!(p.rate, 20_000_000);
    assert!(p.forfeited > 0, "the remainder is given up");
    assert!(
        p.forfeited * 20 < p.grant,
        "forfeit {} should be small against the new grant {}",
        p.forfeited,
        p.grant
    );
}

#[test]
fn a_small_demand_rise_is_not() {
    // This is the hysteresis. A 20% rise mid-window would forfeit most of the
    // grant in force to buy barely more than it — the punitive case in the
    // design's forfeiture table.
    let mut buyer = Buyer::new();
    let p = poll(&buyer, &policy(), demand(1_000_000), Millis(0)).expect("should buy");
    buyer.record(p, Millis(0));

    assert!(
        poll(&buyer, &policy(), demand(1_200_000), Millis(500)).is_none(),
        "a 20% rise should not trigger an early re-buy"
    );
}

#[test]
fn falling_demand_never_triggers_an_early_buy() {
    // Buying cheaper mid-window still forfeits the expensive grant's remainder.
    // Letting it run out and renewing lower is strictly better.
    let mut buyer = Buyer::new();
    let p = poll(&buyer, &policy(), demand(10_000_000), Millis(0)).expect("should buy");
    buyer.record(p, Millis(0));

    assert!(poll(&buyer, &policy(), demand(1_000), Millis(500)).is_none());

    // ...but the renewal at the deadline does drop the rate.
    let p = poll(&buyer, &policy(), demand(1_000), Millis(1_500)).expect("should renew");
    assert_eq!(p.rate, 1_250);
}

#[test]
fn cumulative_only_ever_increases() {
    // The Spilman ratchet requires it, and it is what makes TopUp idempotent.
    let mut buyer = Buyer::new();
    let mut now;
    let mut last = 0;

    for (step, rate) in [500_000u64, 8_000_000, 100_000, 40_000_000, 1_000]
        .into_iter()
        .enumerate()
    {
        now = Millis(step as u64 * 2_000);
        let p = poll(&buyer, &policy(), demand(rate), now).expect("should buy");
        assert!(p.cumulative > last, "cumulative went backwards at step {step}");
        last = p.cumulative;
        buyer.record(p, now);
    }
    assert_eq!(buyer.cumulative(), last);
}

#[test]
fn the_window_is_clamped_to_what_the_provider_advertised() {
    let buyer = Buyer::new();

    // A provider on constrained hardware raising its floor.
    let constrained = Demand {
        observed_rate: 100_000,
        bounds: WindowBounds {
            min_ms: 5_000,
            max_ms: 30_000,
        },
    };
    let p = poll(&buyer, &policy(), constrained, Millis(0)).expect("should buy");
    assert_eq!(p.window_ms, 5_000, "raised to the provider's minimum");

    // A provider that will not sell far ahead.
    let short = Demand {
        observed_rate: 100_000,
        bounds: WindowBounds {
            min_ms: 200,
            max_ms: 1_000,
        },
    };
    let p = poll(&buyer, &policy(), short, Millis(0)).expect("should buy");
    assert_eq!(p.window_ms, 1_000, "lowered to the provider's maximum");
}

#[test]
fn a_rejection_is_answered_at_the_rate_the_provider_named() {
    let mut buyer = Buyer::new();
    let p = poll(&buyer, &policy(), demand(1_000_000), Millis(0)).expect("should buy");
    buyer.record(p, Millis(0));

    // We ask for 20 M/s; the provider says it can only do 4 M/s. Our own
    // ratchet rewinds, because the provider never turned its own.
    let asked = poll(&buyer, &policy(), demand(16_000_000), Millis(400)).expect("should buy");
    let before = buyer.cumulative();
    buyer.record(asked, Millis(400));
    buyer.record_reject(asked.cumulative, 4_000_000);
    assert_eq!(buyer.cumulative(), before, "the refused purchase was undone");

    let p = poll(&buyer, &policy(), demand(16_000_000), Millis(500)).expect("should re-buy");
    assert_eq!(p.trigger, Trigger::Rebuy);
    assert_eq!(p.rate, 4_000_000, "capped at what the provider will take");

    // Once a purchase lands, the cap is forgotten — capacity may have freed up.
    buyer.record(p, Millis(500));
    let p = poll(&buyer, &policy(), demand(16_000_000), Millis(600)).expect("should buy");
    assert_eq!(p.rate, 20_000_000);
}

#[test]
fn the_operators_ceiling_is_respected() {
    // Vouchers cost money to acquire, whatever the protocol thinks.
    let policy = BuyerPolicy {
        max_rate: 5_000_000,
        ..policy()
    };
    let buyer = Buyer::new();
    let p = poll(&buyer, &policy, demand(100_000_000), Millis(0)).expect("should buy");
    assert_eq!(p.rate, 5_000_000);
}

#[test]
fn the_operators_floor_keeps_an_idle_link_ready() {
    let policy = BuyerPolicy {
        min_rate: 10_000,
        ..policy()
    };
    let buyer = Buyer::new();
    let p = poll(&buyer, &policy, demand(0), Millis(0)).expect("should buy");
    assert_eq!(p.rate, 10_000);
}

#[test]
fn an_idle_link_with_no_floor_buys_nothing() {
    // Otherwise a silent link would emit a TopUp every renewal interval forever.
    let buyer = Buyer::new();
    assert!(poll(&buyer, &policy(), demand(0), Millis(0)).is_none());
}

#[test]
fn the_headroom_computation_saturates() {
    let buyer = Buyer::new();
    let p = poll(&buyer, &policy(), demand(u64::MAX), Millis(0)).expect("should buy");
    assert_eq!(p.rate, u64::MAX);
}
