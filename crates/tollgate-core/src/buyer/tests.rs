//! The buyer: adds back what has drained, renews before its budget or deadline
//! runs out, raises its reservation at once when demand outgrows it, keeps to
//! the provider's terms, acts on a refusal by its reason, and carries a
//! purchase across a channel boundary rather than stalling at one.
//!
//! The worked examples are the ones in `docs/design/core/tollgate-vouchers.md`
//! ("Time at a Speed", "Pay for What You Use", "How a Buyer Buys").

use tollgate_protocol::{ChannelId, ReasonCode};

use super::*;
use crate::time::Millis;

const CAPACITY: u64 = 1_000_000_000;
const SECOND: u64 = 1_000;
const DAY: u64 = 86_400 * SECOND;
const MB: u64 = 1_000_000;

fn channel(seed: u8) -> ChannelId {
    ChannelId([seed; 32])
}

fn terms() -> Terms {
    Terms {
        min_window_ms: 200,
        max_window_ms: 30_000,
        min_reserved_rate: 0,
        min_topup_gap_ms: 200,
        from_payer_weight: 1,
    }
}

/// Reserve 125% of demand, hold two seconds of it, renew half a second early.
fn policy() -> BuyerPolicy {
    BuyerPolicy {
        renew_lead_ms: 500,
        window_ms: 2_000,
        ..BuyerPolicy::default()
    }
}

fn demand(rate: u64) -> Demand {
    Demand {
        observed_rate: rate,
        terms: terms(),
    }
}

/// A buyer with one confirmed channel, which is where every test starts.
fn opened(capacity: u64) -> Buyer {
    let mut buyer = Buyer::new();
    buyer.funded(channel(1), capacity, None);
    buyer.confirmed(channel(1));
    buyer
}

/// Decide and commit, as the session does after the TopUp is sent.
fn buy(buyer: &mut Buyer, policy: &BuyerPolicy, demand: Demand, now: Millis) -> Purchase {
    let p = poll(buyer, policy, demand, now).expect("should buy");
    buyer.record(p, now);
    p
}

/// Let time pass with nothing moving: our own count drains at the reserved
/// rate, as the provider's does.
fn idle_until(buyer: &mut Buyer, now: Millis) {
    buyer.draw(0, now);
}

// ---------------------------------------------------------------------------
// Time at a speed
// ---------------------------------------------------------------------------

/// "Time at a Speed": 5 Mbit/s is 625,000 bytes a second, in 10 s windows,
/// bought again 2 s before the budget would run out.
fn time_at_a_speed() -> (BuyerPolicy, Demand) {
    let policy = BuyerPolicy {
        window_ms: 10_000,
        renew_lead_ms: 2_000,
        // Pinned to the one speed, whatever the demand.
        min_rate: 625_000,
        max_rate: 625_000,
        ..BuyerPolicy::default()
    };
    let demand = Demand {
        observed_rate: 625_000,
        terms: Terms {
            min_window_ms: 1_000,
            max_window_ms: 30 * DAY,
            min_topup_gap_ms: 1_000,
            ..terms()
        },
    };
    (policy, demand)
}

#[test]
fn a_buyer_adds_back_only_what_was_drawn() {
    let (policy, demand) = time_at_a_speed();
    let mut buyer = opened(CAPACITY);

    let p = buy(&mut buyer, &policy, demand, Millis(0));
    assert_eq!(p.trigger, Trigger::First);
    assert_eq!(p.grant, 6_250_000, "the reserved rate times the window");
    assert_eq!(p.reserved_rate, 625_000);
    assert_eq!(p.window_ms, 10_000);

    // Drawn at the reserved rate, used or not, and nothing to buy before t=8.
    idle_until(&mut buyer, Millis(7_999));
    assert!(poll(&buyer, &policy, demand, Millis(7_999)).is_none());

    idle_until(&mut buyer, Millis(8_000));
    assert_eq!(buyer.remaining_at(Millis(8_000)), 1_250_000);
    let p = buy(&mut buyer, &policy, demand, Millis(8_000));
    assert_eq!(p.trigger, Trigger::Renewal);
    assert_eq!(p.grant, 5_000_000, "only what was drawn since the last");
    assert_eq!(buyer.remaining_at(Millis(8_000)), 6_250_000);
    assert_eq!(buyer.deadline(), Millis(18_000));

    idle_until(&mut buyer, Millis(16_000));
    let p = buy(&mut buyer, &policy, demand, Millis(16_000));
    assert_eq!(p.grant, 5_000_000, "5 M every 8 s: each second paid once");
    assert_eq!(
        buyer.remaining_at(Millis(16_000)),
        6_250_000,
        "and no bigger"
    );
    assert_eq!(buyer.deadline(), Millis(26_000));
}

#[test]
fn the_budget_never_grows_from_one_purchase_to_the_next() {
    let (policy, demand) = time_at_a_speed();
    let mut buyer = opened(CAPACITY);
    buy(&mut buyer, &policy, demand, Millis(0));
    for second in 1..120 {
        let now = Millis(second * SECOND);
        idle_until(&mut buyer, now);
        if let Some(p) = poll(&buyer, &policy, demand, now) {
            buyer.record(p, now);
        }
        assert!(buyer.remaining_at(now) <= 6_250_000);
        assert!(buyer.remaining_at(now) > 0, "never runs dry at t={second}");
    }
}

#[test]
fn demand_outgrowing_the_reservation_raises_it_at_once() {
    // Nothing is forfeit, so there is no reason to wait for the renewal: the
    // same purchase raises the rate and brings the budget up to the new size.
    let mut buyer = opened(CAPACITY);
    buy(&mut buyer, &policy(), demand(400_000), Millis(0));
    assert_eq!(buyer.reserved(), 500_000);

    idle_until(&mut buyer, Millis(300));
    let p = poll(&buyer, &policy(), demand(600_000), Millis(300)).expect("should buy");
    assert_eq!(p.trigger, Trigger::RateRose);
    assert_eq!(p.reserved_rate, 750_000);
    assert_eq!(
        p.grant,
        1_500_000 - buyer.remaining_at(Millis(300)),
        "up to the new size, the new rate times the window"
    );
}

#[test]
fn a_rise_inside_the_headroom_buys_nothing_early() {
    // The headroom is there for this: demand creeping up within what was
    // reserved is not worth a purchase per gap.
    let mut buyer = opened(CAPACITY);
    buy(&mut buyer, &policy(), demand(400_000), Millis(0));
    assert!(poll(&buyer, &policy(), demand(480_000), Millis(300)).is_none());
}

#[test]
fn falling_demand_lowers_the_reservation_at_the_next_purchase() {
    let mut buyer = opened(CAPACITY);
    buy(&mut buyer, &policy(), demand(400_000), Millis(0));
    assert!(
        poll(&buyer, &policy(), demand(100_000), Millis(300)).is_none(),
        "not a reason to buy"
    );

    idle_until(&mut buyer, Millis(1_500));
    let p = buy(&mut buyer, &policy(), demand(100_000), Millis(1_500));
    assert_eq!(p.trigger, Trigger::Renewal);
    assert_eq!(p.reserved_rate, 125_000);
}

// ---------------------------------------------------------------------------
// Pay for what you use
// ---------------------------------------------------------------------------

fn pay_per_use() -> (BuyerPolicy, Demand) {
    let policy = BuyerPolicy {
        reserve: false,
        budget: 1_000 * MB,
        window_ms: 30 * DAY,
        ..BuyerPolicy::default()
    };
    let demand = Demand {
        observed_rate: 10 * MB,
        terms: Terms {
            min_window_ms: 1_000,
            max_window_ms: 30 * DAY,
            min_topup_gap_ms: 1_000,
            ..terms()
        },
    };
    (policy, demand)
}

#[test]
fn pay_per_use_holds_its_budget_and_reserves_nothing() {
    let (policy, demand) = pay_per_use();
    let mut buyer = opened(10 * CAPACITY);
    let p = buy(&mut buyer, &policy, demand, Millis(0));
    assert_eq!(p.grant, 1_000 * MB);
    assert_eq!(p.reserved_rate, 0);
    assert_eq!(p.window_ms, 30 * DAY);

    // An idle hour draws nothing.
    idle_until(&mut buyer, Millis(3_600 * SECOND));
    assert_eq!(buyer.remaining_at(Millis(3_600 * SECOND)), 1_000 * MB);
}

#[test]
fn pay_per_use_adds_back_what_it_used_before_it_runs_out() {
    let (policy, demand) = pay_per_use();
    let mut buyer = opened(10 * CAPACITY);
    buy(&mut buyer, &policy, demand, Millis(0));

    // 10 MB/s observed and a 1.2 s lead: it buys again with 12 MB to go.
    buyer.draw(980 * MB, Millis(DAY));
    assert!(poll(&buyer, &policy, demand, Millis(DAY)).is_none());
    buyer.draw(8 * MB, Millis(DAY + SECOND));
    let p = buy(&mut buyer, &policy, demand, Millis(DAY + SECOND));
    assert_eq!(p.trigger, Trigger::Renewal);
    assert_eq!(p.grant, 988 * MB, "what it used");
    assert_eq!(buyer.remaining_at(Millis(DAY + SECOND)), 1_000 * MB);
}

#[test]
fn a_budget_about_to_expire_is_kept_alive_while_something_wants_the_link() {
    // The deadline comes first: a purchase of a little keeps the budget,
    // which is accepted as a design decision.
    let (policy, demand) = pay_per_use();
    let mut buyer = opened(10 * CAPACITY);
    buy(&mut buyer, &policy, demand, Millis(0));

    let near = Millis(30 * DAY - 1_000);
    let p = buy(&mut buyer, &policy, demand, near);
    assert_eq!(p.trigger, Trigger::Renewal);
    assert_eq!(p.grant, 1, "nothing drained, so it buys the least it can");
    assert_eq!(buyer.deadline(), near + 30 * DAY);
}

#[test]
fn nothing_is_bought_while_nothing_wants_the_link() {
    let (ppu, mut idle) = pay_per_use();
    idle.observed_rate = 0;
    let buyer = opened(CAPACITY);
    assert!(poll(&buyer, &ppu, idle, Millis(0)).is_none());
    assert!(poll(&buyer, &policy(), demand(0), Millis(0)).is_none());
}

#[test]
fn a_standing_demand_buys_with_nothing_observed() {
    let policy = BuyerPolicy {
        demand: 100_000,
        ..policy()
    };
    let p = poll(&opened(CAPACITY), &policy, demand(0), Millis(0)).expect("should buy");
    assert_eq!(p.reserved_rate, 125_000);
}

// ---------------------------------------------------------------------------
// The provider's terms
// ---------------------------------------------------------------------------

#[test]
fn never_sooner_than_the_providers_gap() {
    let mut buyer = opened(CAPACITY);
    buy(&mut buyer, &policy(), demand(400_000), Millis(0));
    // Demand outgrew the reservation, but the gap is not up.
    assert!(poll(&buyer, &policy(), demand(800_000), Millis(199)).is_none());
    assert!(poll(&buyer, &policy(), demand(800_000), Millis(200)).is_some());
}

#[test]
fn the_window_is_clamped_to_what_the_provider_advertised() {
    let buyer = opened(CAPACITY);
    let narrow = Demand {
        observed_rate: 400_000,
        terms: Terms {
            min_window_ms: 200,
            max_window_ms: 1_000,
            ..terms()
        },
    };
    let p = poll(&buyer, &policy(), narrow, Millis(0)).expect("should buy");
    assert_eq!(p.window_ms, 1_000);
    assert_eq!(p.grant, 500_000, "sized to the window it actually got");
}

#[test]
fn the_reservation_is_raised_to_the_providers_smallest() {
    let floor = Demand {
        observed_rate: 1_000,
        terms: Terms {
            min_reserved_rate: 125_000,
            ..terms()
        },
    };
    let p = poll(&opened(CAPACITY), &policy(), floor, Millis(0)).expect("should buy");
    assert_eq!(p.reserved_rate, 125_000);

    // A pay-per-use buyer reserves it too, where the provider asks for it.
    let (ppu, _) = pay_per_use();
    let p = poll(&opened(CAPACITY), &ppu, floor, Millis(0)).expect("should buy");
    assert_eq!(p.reserved_rate, 125_000);
}

#[test]
fn a_smallest_reserved_rate_above_our_ceiling_buys_nothing() {
    let policy = BuyerPolicy {
        max_rate: 100_000,
        ..policy()
    };
    let floor = Demand {
        observed_rate: 50_000,
        terms: Terms {
            min_reserved_rate: 125_000,
            ..terms()
        },
    };
    assert!(poll(&opened(CAPACITY), &policy, floor, Millis(0)).is_none());
}

#[test]
fn the_operators_bounds_on_the_reservation_are_kept() {
    let policy = BuyerPolicy {
        min_rate: 50_000,
        max_rate: 300_000,
        ..policy()
    };
    let buyer = opened(CAPACITY);
    let p = poll(&buyer, &policy, demand(10), Millis(0)).expect("should buy");
    assert_eq!(p.reserved_rate, 50_000);
    let p = poll(&buyer, &policy, demand(u64::MAX), Millis(0)).expect("should buy");
    assert_eq!(
        p.reserved_rate, 300_000,
        "the headroom saturates, then caps"
    );
}

#[test]
fn a_weight_above_our_limit_is_refused() {
    let policy = BuyerPolicy {
        max_from_payer_weight: Some(2),
        ..policy()
    };
    assert!(policy.accepts_weight(0));
    assert!(policy.accepts_weight(2));
    assert!(!policy.accepts_weight(10));
    assert!(
        BuyerPolicy::default().accepts_weight(u16::MAX),
        "unset takes any"
    );
}

#[test]
fn a_buyer_with_nothing_to_spend_buys_nothing() {
    assert!(BuyerPolicy::default().buying());
    let zero = BuyerPolicy {
        max_rate: 0,
        ..BuyerPolicy::default()
    };
    assert!(!zero.buying());
    let no_budget = BuyerPolicy {
        reserve: false,
        ..BuyerPolicy::default()
    };
    assert!(!no_budget.buying(), "pay per use needs a budget");
}

#[test]
fn the_default_policy_is_the_documented_one() {
    // tollgate-configuration.md, Buying.
    let p = BuyerPolicy::default();
    assert_eq!(p.demand, 0);
    assert!(p.reserve);
    assert_eq!(p.headroom_pct, 125);
    assert_eq!((p.min_rate, p.max_rate), (0, u64::MAX));
    assert_eq!(p.max_from_payer_weight, None);
    assert_eq!(p.window_ms, 10_000);
    assert_eq!(p.budget, 0);
    assert_eq!(p.renew_lead_ms, 1_200);
    assert_eq!(p.cap_hold_ms, 10_000);
    assert!(!p.lead_is_thin());
}

#[test]
fn a_lead_longer_than_the_window_does_not_buy_forever() {
    // A provider that bounds the window shorter than the configured lead
    // would otherwise have every purchase start inside its own lead.
    let policy = BuyerPolicy {
        renew_lead_ms: 5_000,
        ..policy()
    };
    assert_eq!(policy.lead_within(2_000), 1_000);
    let mut buyer = opened(CAPACITY);
    buy(&mut buyer, &policy, demand(400_000), Millis(0));
    idle_until(&mut buyer, Millis(500));
    assert!(poll(&buyer, &policy, demand(400_000), Millis(500)).is_none());
}

// ---------------------------------------------------------------------------
// Refusals
// ---------------------------------------------------------------------------

fn refused(p: &Purchase) -> [(ChannelId, u64); 1] {
    [(p.first.channel_id, p.first.cumulative)]
}

#[test]
fn a_capacity_refusal_is_answered_at_the_rate_the_provider_named() {
    let mut buyer = opened(CAPACITY);
    let p = buy(&mut buyer, &policy(), demand(4_000_000), Millis(0));
    assert_eq!(p.reserved_rate, 5_000_000);

    buyer.record_reject(
        &refused(&p),
        ReasonCode::RateExceedsCapacity,
        1_000_000,
        Millis(10),
        10_000,
    );
    assert_eq!(buyer.cumulative(), 0, "the ratchet is undone");
    assert_eq!(buyer.remaining_at(Millis(10)), 0, "and the budget with it");
    assert_eq!(buyer.reserved(), 0);

    // Not before the gap from the refused one, then at the rate named.
    assert!(poll(&buyer, &policy(), demand(4_000_000), Millis(100)).is_none());
    let p = poll(&buyer, &policy(), demand(4_000_000), Millis(200)).expect("should buy");
    assert_eq!(p.reserved_rate, 1_000_000);
    assert_eq!(
        p.first.cumulative, 2_000_000,
        "built on the last total taken"
    );
    buyer.record(p, Millis(200));

    // It keeps to it for the hold, then tries higher again.
    idle_until(&mut buyer, Millis(1_800));
    let p = buy(&mut buyer, &policy(), demand(4_000_000), Millis(1_800));
    assert_eq!(p.reserved_rate, 1_000_000, "still held at t=1.8 s");
    let p = poll(&buyer, &policy(), demand(4_000_000), Millis(10_010)).expect("should buy");
    assert_eq!(p.reserved_rate, 5_000_000);
    assert_eq!(p.trigger, Trigger::RateRose);
}

#[test]
fn a_rebuy_under_a_named_cap_says_so() {
    let mut buyer = opened(CAPACITY);
    buy(&mut buyer, &policy(), demand(400_000), Millis(0));
    let p = buy(&mut buyer, &policy(), demand(4_000_000), Millis(200));
    buyer.record_reject(
        &refused(&p),
        ReasonCode::RateExceedsCapacity,
        1_000_000,
        Millis(210),
        10_000,
    );
    assert_eq!(buyer.reserved(), 500_000, "back to what was taken");
    let p = poll(&buyer, &policy(), demand(4_000_000), Millis(400)).expect("should buy");
    assert_eq!(p.trigger, Trigger::Rebuy);
    assert_eq!(p.reserved_rate, 1_000_000);
}

#[test]
fn a_too_soon_refusal_waits_out_the_gap_and_sends_again() {
    let mut buyer = opened(CAPACITY);
    let p = buy(&mut buyer, &policy(), demand(400_000), Millis(0));
    buyer.record_reject(&refused(&p), ReasonCode::TooSoon, 0, Millis(5), 10_000);
    assert_eq!(buyer.cumulative(), 0);

    assert!(poll(&buyer, &policy(), demand(400_000), Millis(199)).is_none());
    let again = poll(&buyer, &policy(), demand(400_000), Millis(200)).expect("should buy");
    assert_eq!(
        again.first.cumulative, p.first.cumulative,
        "the same purchase"
    );
    assert_eq!(again.reserved_rate, p.reserved_rate, "and no cap from it");
}

#[test]
fn an_out_of_range_refusal_is_undone_and_bought_again_on_the_terms() {
    let mut buyer = opened(CAPACITY);
    let p = buy(&mut buyer, &policy(), demand(400_000), Millis(0));
    buyer.record_reject(&refused(&p), ReasonCode::OutOfRange, 0, Millis(5), 10_000);
    assert_eq!(buyer.cumulative(), 0);

    // The provider has revised its terms in the meantime.
    let revised = Demand {
        observed_rate: 400_000,
        terms: Terms {
            min_window_ms: 5_000,
            ..terms()
        },
    };
    let p = poll(&buyer, &policy(), revised, Millis(200)).expect("should buy");
    assert_eq!(p.window_ms, 5_000);
}

#[test]
fn a_stale_refusal_does_not_rewind_anything() {
    // A refusal for a purchase we have already moved past only names a cap.
    let mut buyer = opened(CAPACITY);
    let first = buy(&mut buyer, &policy(), demand(400_000), Millis(0));
    idle_until(&mut buyer, Millis(1_600));
    buy(&mut buyer, &policy(), demand(400_000), Millis(1_600));
    let cumulative = buyer.cumulative();

    buyer.record_reject(
        &refused(&first),
        ReasonCode::RateExceedsCapacity,
        1_000,
        Millis(1_700),
        10_000,
    );
    assert_eq!(buyer.cumulative(), cumulative);
}

// ---------------------------------------------------------------------------
// Our own count, and the provider's Balance
// ---------------------------------------------------------------------------

#[test]
fn a_balance_below_our_own_count_never_makes_us_buy() {
    let mut buyer = opened(CAPACITY);
    buy(&mut buyer, &policy(), demand(400_000), Millis(0));
    buyer.note_balance(1, 1_000, Millis(10));
    assert_eq!(buyer.remaining_at(Millis(10)), 1_000_000);
    assert!(poll(&buyer, &policy(), demand(400_000), Millis(300)).is_none());
}

#[test]
fn a_buyer_that_came_back_learns_what_it_left_behind() {
    // Without a count of its own, the Balance at the session start is how it
    // knows; it then buys only what is missing.
    let (policy, demand) = pay_per_use();
    let mut buyer = opened(10 * CAPACITY);
    buyer.note_balance(700 * MB, 29 * DAY, Millis(0));
    assert_eq!(buyer.remaining_at(Millis(0)), 700 * MB);
    assert_eq!(buyer.deadline(), Millis(29 * DAY));
    assert!(
        poll(&buyer, &policy, demand, Millis(0)).is_none(),
        "700 MB is plenty to be going on with"
    );
}

#[test]
fn a_new_session_keeps_our_count_and_reserves_again() {
    let mut buyer = opened(CAPACITY);
    buy(&mut buyer, &policy(), demand(400_000), Millis(0));
    idle_until(&mut buyer, Millis(400));

    buyer.restart();
    assert_eq!(
        buyer.remaining_at(Millis(400)),
        800_000,
        "the budget is ours"
    );
    assert_eq!(
        buyer.reserved(),
        0,
        "the reservation ended with the session"
    );

    // No need to wait out a gap: the provider starts it over too.
    let p = poll(&buyer, &policy(), demand(400_000), Millis(401)).expect("should buy");
    assert_eq!(p.trigger, Trigger::First);
    assert_eq!(p.reserved_rate, 500_000);
    assert_eq!(p.grant, 200_000, "only what is missing");
}

#[test]
fn our_count_ends_the_reservation_at_zero_and_at_the_deadline() {
    let mut buyer = opened(CAPACITY);
    buy(&mut buyer, &policy(), demand(400_000), Millis(0));
    buyer.draw(u64::MAX, Millis(100));
    assert_eq!(buyer.remaining_at(Millis(100)), 0);
    assert_eq!(buyer.reserved(), 0);

    let mut buyer = opened(CAPACITY);
    let (ppu, d) = pay_per_use();
    buy(&mut buyer, &ppu, d, Millis(0));
    assert_eq!(buyer.remaining_at(Millis(30 * DAY)), 0, "expired");
}

#[test]
fn what_we_send_draws_at_the_providers_weight() {
    // The session reads our upload as `from_payer` and weights it; the buyer
    // only ever sees the weighted total.
    let mut buyer = opened(CAPACITY);
    buy(&mut buyer, &policy(), demand(400_000), Millis(0));
    let moved = crate::meter::Counters {
        to_payer: 50_000,
        from_payer: 1_000,
    }
    .weighted(10);
    buyer.draw(moved, Millis(1));
    assert_eq!(buyer.remaining_at(Millis(1)), 1_000_000 - 60_000);
}

// ---------------------------------------------------------------------------
// Channels
// ---------------------------------------------------------------------------

#[test]
fn nothing_is_bought_before_a_channel_is_confirmed() {
    // A grant signed against a channel the peer has not verified could be
    // against one that never opens.
    let mut buyer = Buyer::new();
    assert!(poll(&buyer, &policy(), demand(100_000), Millis(0)).is_none());

    buyer.funded(channel(1), CAPACITY, None);
    assert!(
        poll(&buyer, &policy(), demand(100_000), Millis(0)).is_none(),
        "funded is not the same as confirmed"
    );

    buyer.confirmed(channel(1));
    assert!(poll(&buyer, &policy(), demand(100_000), Millis(0)).is_some());
}

#[test]
fn cumulative_only_ever_increases_on_a_given_channel() {
    let mut buyer = opened(CAPACITY);
    let mut last = 0;
    for second in 0..20 {
        let now = Millis(second * 500);
        idle_until(&mut buyer, now);
        if let Some(p) = poll(&buyer, &policy(), demand(400_000), now) {
            assert!(p.first.cumulative > last);
            last = p.first.cumulative;
            buyer.record(p, now);
        }
    }
}

#[test]
fn a_replacement_is_opened_at_the_threshold_and_only_once() {
    // Funding a channel on every check would open one per tick.
    let mut buyer = opened(1_000_000);
    assert!(!buyer.needs_rollover(80));

    // 320 k/s with 125% headroom for 2 s is 800 k: 80% of the channel.
    buy(&mut buyer, &policy(), demand(320_000), Millis(0));
    assert_eq!(buyer.cumulative(), 800_000);
    assert!(buyer.needs_rollover(80), "80% reached");

    // Once funding is under way it must stay quiet until the peer confirms.
    buyer.funded(channel(2), 1_000_000, None);
    assert!(!buyer.needs_rollover(80), "one is already on the way");

    buyer.confirmed(channel(2));
    assert!(
        !buyer.needs_rollover(80),
        "a replacement is ready and waiting"
    );
}

#[test]
fn a_replacement_is_quiet_from_the_moment_its_funding_is_asked_for() {
    // Funding is a mint round trip, many ticks long. Marked only once the
    // channel came back, every tick in between would ask for another.
    let mut buyer = opened(1_000_000);
    buy(&mut buyer, &policy(), demand(320_000), Millis(0));
    let asked = Millis(1_000);
    assert_eq!(
        buyer.rollover_due(80, asked, 0),
        Some(RolloverReason::Capacity)
    );

    buyer.funding_requested(1, asked);
    assert_eq!(buyer.rollover_due(80, asked + 1, 0), None);
    assert_eq!(
        buyer.rollover_due(80, asked + FUNDING_TIMEOUT_MS - 1, 0),
        None,
        "quiet while the host is still working on it"
    );

    // An answer that never comes is given up on, and the rollover asked again.
    assert_eq!(
        buyer.rollover_due(80, asked + FUNDING_TIMEOUT_MS, 0),
        Some(RolloverReason::Capacity)
    );

    // A failure clears it at once.
    buyer.funding_requested(2, asked);
    buyer.funding_failed(2);
    assert_eq!(
        buyer.rollover_due(80, asked + 1, 0),
        Some(RolloverReason::Capacity)
    );

    // And the channel coming back hands over to `pending`, which stays quiet
    // until the peer confirms, however long that takes.
    buyer.funding_requested(3, asked);
    assert!(buyer.answers(3));
    buyer.funded(channel(2), 1_000_000, None);
    assert!(!buyer.funding_in_flight(asked));
    assert_eq!(
        buyer.rollover_due(80, asked + FUNDING_TIMEOUT_MS * 10, 0),
        None
    );
}

#[test]
fn the_first_channel_back_is_taken_and_every_other_request_is_superseded() {
    // A request given up on is not cancelled — the host may still be working
    // on it — so after a timeout two can be out at once. One channel was
    // wanted, whichever of them produces it.
    let mut buyer = Buyer::new();
    buyer.funding_requested(7, Millis(0));
    buyer.funding_requested(9, Millis(FUNDING_TIMEOUT_MS));
    assert!(buyer.answers(7), "the one given up on still counts");
    assert!(buyer.answers(9));
    assert!(!buyer.answers(6), "asked before either, of somebody else");
    assert!(!buyer.answers(10), "not asked yet");

    // The one given up on comes back first, and is taken.
    buyer.funded(channel(1), 1_000, None);
    assert!(
        !buyer.answers(9),
        "the later one is superseded: its channel is not wanted"
    );

    // Nor is anything taken over a channel still awaiting confirmation, even
    // for a request asked since.
    buyer.funding_requested(10, Millis(FUNDING_TIMEOUT_MS * 2));
    assert!(!buyer.answers(10), "never over `pending`");
    buyer.confirmed(channel(1));
    assert!(buyer.answers(10));

    // A failure of a request given up on leaves the later one waited on.
    buyer.funding_requested(11, Millis(FUNDING_TIMEOUT_MS * 4));
    buyer.funding_failed(10);
    assert!(buyer.funding_in_flight(Millis(FUNDING_TIMEOUT_MS * 4 + 1)));
    buyer.funding_failed(11);
    assert!(!buyer.funding_in_flight(Millis(FUNDING_TIMEOUT_MS * 4 + 1)));
    assert!(
        buyer.answers(10) && buyer.answers(11),
        "either may still land"
    );

    // A buyer started over has asked for nothing.
    assert!(!Buyer::new().answers(11));
}

#[test]
fn a_confirmed_replacement_waits_behind_the_channel_in_use() {
    // It does not displace the one being drained — that one runs to its
    // capacity first, which is what keeps the old channel's remaining capacity
    // from being thrown away.
    let mut buyer = opened(1_000_000);
    buyer.funded(channel(2), 1_000_000, None);
    buyer.confirmed(channel(2));

    assert_eq!(buyer.active().expect("active").id, channel(1));
    assert_eq!(buyer.next_channel().expect("next").id, channel(2));
}

#[test]
fn a_purchase_that_overflows_is_signed_across_both_channels() {
    // The old channel is topped to its capacity and the remainder is signed
    // onto the new one, in one purchase.
    let mut buyer = opened(1_000_000);
    buy(&mut buyer, &policy(), demand(320_000), Millis(0));
    assert_eq!(buyer.cumulative(), 800_000, "200 k of headroom left");

    buyer.funded(channel(2), 1_000_000, None);
    buyer.confirmed(channel(2));

    // 1.6 s at 400 k/s drew 640 k, which is more than the 200 k the channel
    // in use can carry.
    idle_until(&mut buyer, Millis(1_600));
    let p = poll(&buyer, &policy(), demand(320_000), Millis(1_600)).expect("should buy");
    assert_eq!(p.grant, 640_000);

    let first = p.first;
    let second = p.second.expect("the overflow needs its own channel");
    assert_eq!(first.channel_id, channel(1));
    assert_eq!(
        first.cumulative, 1_000_000,
        "topped to exactly its capacity"
    );
    assert_eq!(second.channel_id, channel(2));
    assert_eq!(
        second.cumulative, 440_000,
        "the remainder starts the new one"
    );
}

#[test]
fn an_exhausted_channel_is_retired_and_offered_for_settlement() {
    let mut buyer = opened(1_000_000);
    let p = poll(&buyer, &policy(), demand(320_000), Millis(0)).expect("should buy");
    assert_eq!(buyer.record(p, Millis(0)), None, "nothing retired yet");

    buyer.funded(channel(2), 1_000_000, None);
    buyer.confirmed(channel(2));

    idle_until(&mut buyer, Millis(1_600));
    let p = poll(&buyer, &policy(), demand(320_000), Millis(1_600)).expect("should buy");
    let retired = buyer.record(p, Millis(1_600));

    assert_eq!(
        retired,
        Some(channel(1)),
        "the drained channel can be settled"
    );
    assert_eq!(buyer.active().expect("active").id, channel(2));
    assert_eq!(buyer.next_channel(), None);
    assert_eq!(
        buyer.cumulative(),
        440_000,
        "the overflow carried onto the new channel"
    );
}

#[test]
fn without_a_replacement_the_buyer_takes_what_fits_and_then_stops() {
    // The channel really is full. Stopping is correct; what would be wrong is
    // signing a total the channel cannot settle.
    let mut buyer = opened(1_000_000);
    buy(&mut buyer, &policy(), demand(320_000), Millis(0));

    idle_until(&mut buyer, Millis(1_600));
    let p = buy(&mut buyer, &policy(), demand(320_000), Millis(1_600));
    assert_eq!(p.grant, 200_000, "only what the channel could still carry");
    assert_eq!(p.first.cumulative, 1_000_000);
    assert_eq!(p.second, None);

    idle_until(&mut buyer, Millis(2_000));
    assert!(
        poll(&buyer, &policy(), demand(320_000), Millis(2_000)).is_none(),
        "nothing left to sign against and nowhere to move to"
    );
    assert!(
        buyer.needs_rollover(80),
        "and it is asking for a replacement"
    );
}

#[test]
fn a_replacement_confirmed_after_the_channel_filled_takes_over_directly() {
    // The provider settled the full channel the moment it filled. Queuing the
    // replacement behind it would put a leg on a settled channel into every
    // purchase, and each would be refused in full.
    let mut buyer = opened(1_000_000);
    buy(&mut buyer, &policy(), demand(320_000), Millis(0));
    idle_until(&mut buyer, Millis(1_600));
    buy(&mut buyer, &policy(), demand(320_000), Millis(1_600));
    assert!(buyer.active().expect("active").exhausted());

    buyer.funded(channel(2), 1_000_000, None);
    buyer.confirmed(channel(2));
    assert_eq!(buyer.active().expect("active").id, channel(2));
    assert_eq!(buyer.next_channel(), None);

    idle_until(&mut buyer, Millis(2_000));
    let p = poll(&buyer, &policy(), demand(320_000), Millis(2_000)).expect("should buy");
    assert_eq!(p.first.channel_id, channel(2));
    assert_eq!(p.second, None, "nothing signed on the settled channel");
}

#[test]
fn cumulative_restarts_from_zero_on_the_replacement() {
    // A cumulative total only means anything against the channel it was signed
    // on, so carrying the old one forward would be meaningless — and would read
    // as a channel already near its capacity.
    let mut buyer = opened(1_000_000);
    buy(&mut buyer, &policy(), demand(320_000), Millis(0));

    buyer.funded(channel(2), 1_000_000, None);
    buyer.confirmed(channel(2));
    assert_eq!(
        buyer.next_channel().expect("next").cumulative,
        0,
        "the replacement starts empty"
    );
}

#[test]
fn a_replacement_is_opened_before_the_channel_is_too_small_for_another_purchase() {
    // A threshold alone is reactive, and a purchase is not gradual: one can
    // take a channel from empty to nearly full in one step. Waiting for 80%
    // *signed* would leave nothing to move onto, and the payer would fall to
    // the minimum flow allowance while a replacement is funded and verified.
    let mut buyer = opened(3_000_000);

    // One purchase takes a third of the channel.
    let p = buy(&mut buyer, &policy(), demand(400_000), Millis(0));
    assert_eq!(p.grant, 1_000_000);

    // Only 2 M left, which is two more purchases — already worth replacing,
    // well before the 80% the threshold would wait for.
    assert!(
        buyer.needs_rollover(80),
        "should ask for a replacement while there is still room to use it"
    );
}

#[test]
fn a_channel_with_plenty_of_room_is_left_alone() {
    let mut buyer = opened(CAPACITY);
    buy(&mut buyer, &policy(), demand(400_000), Millis(0));
    assert!(
        !buyer.needs_rollover(80),
        "1 M against a 1 GB channel is no reason to fund another"
    );
}

const TTL_MS: u64 = 3_600_000;
const MARGIN_MS: u64 = 60_000;
const SETTLE_LEAD_MS: u64 = 30_000;

/// A buyer with one confirmed channel that expires an hour in.
fn opened_until(capacity: u64, expires_at: Millis) -> Buyer {
    let mut buyer = Buyer::new();
    buyer.funded(channel(1), capacity, Some(expires_at));
    buyer.confirmed(channel(1));
    buyer
}

#[test]
fn a_slowly_drawn_channel_is_replaced_when_it_enters_the_safety_margin() {
    // 100 KB/s takes about three hours to get through 1 GB, and the funder can
    // reclaim the channel after one. Waiting for the threshold would let it
    // take back what it already paid.
    let mut buyer = opened_until(CAPACITY, Millis(TTL_MS));
    buy(&mut buyer, &policy(), demand(100_000), Millis(0));
    assert!(!buyer.needs_rollover(80), "nowhere near full");

    let entering = Millis(TTL_MS - MARGIN_MS);
    assert_eq!(buyer.rollover_due(80, entering - 1, MARGIN_MS), None);
    assert_eq!(
        buyer.rollover_due(80, entering, MARGIN_MS),
        Some(RolloverReason::Expiry),
        "inside the margin, however little has been used"
    );

    // And only once, exactly as for capacity.
    buyer.funded(channel(2), CAPACITY, Some(Millis(TTL_MS * 2)));
    assert_eq!(buyer.rollover_due(80, entering, MARGIN_MS), None);
}

#[test]
fn a_channel_that_never_expires_is_only_rolled_over_for_capacity() {
    let buyer = opened(CAPACITY);
    assert_eq!(buyer.rollover_due(80, Millis(u64::MAX), MARGIN_MS), None);
}

#[test]
fn a_filling_channel_is_a_capacity_rollover_even_near_expiry() {
    // The reason decides whether the replacement grows, and a channel that
    // filled up has earned it whatever the clock says.
    let mut buyer = opened_until(1_000_000, Millis(TTL_MS));
    buy(&mut buyer, &policy(), demand(320_000), Millis(0));

    assert_eq!(
        buyer.rollover_due(80, Millis(TTL_MS - MARGIN_MS), MARGIN_MS),
        Some(RolloverReason::Capacity)
    );
}

#[test]
fn inside_the_margin_a_confirmed_replacement_takes_over_at_once() {
    // The receiver is about to settle the old channel, so what is left on it is
    // given up rather than drained: anything signed on it after settlement
    // would be refused. The budget is not touched.
    let mut buyer = opened_until(CAPACITY, Millis(TTL_MS));
    buy(&mut buyer, &policy(), demand(100_000), Millis(0));

    buyer.funded(channel(2), CAPACITY, Some(Millis(2 * TTL_MS)));
    buyer.confirmed(channel(2));

    let early = Millis(TTL_MS - MARGIN_MS - 1);
    assert_eq!(
        buyer.retire_expiring(early, MARGIN_MS, SETTLE_LEAD_MS),
        None,
        "outside the margin the old channel drains first, as usual"
    );

    let inside = Millis(TTL_MS - MARGIN_MS);
    assert_eq!(
        buyer.retire_expiring(inside, MARGIN_MS, SETTLE_LEAD_MS),
        Some(channel(1))
    );
    assert_eq!(buyer.active().expect("active").id, channel(2));
    assert_eq!(buyer.next_channel(), None);

    let p = poll(&buyer, &policy(), demand(100_000), inside).expect("should buy");
    assert_eq!(p.first.channel_id, channel(2), "buying moved with it");
    assert_eq!(p.second, None);
}

#[test]
fn without_a_replacement_the_expiring_channel_is_used_until_it_is_settled() {
    // Late is better than nothing up to the point where the receiver settles —
    // and from there, nothing: a total signed on a settled channel is refused.
    let mut buyer = opened_until(CAPACITY, Millis(TTL_MS));
    let inside = Millis(TTL_MS - MARGIN_MS);
    assert_eq!(
        buyer.retire_expiring(inside, MARGIN_MS, SETTLE_LEAD_MS),
        None
    );
    assert!(
        poll(&buyer, &policy(), demand(100_000), inside).is_some(),
        "still worth buying on while the replacement is on its way"
    );

    let settling = Millis(TTL_MS - SETTLE_LEAD_MS);
    assert_eq!(
        buyer.retire_expiring(settling, MARGIN_MS, SETTLE_LEAD_MS),
        Some(channel(1))
    );
    assert_eq!(buyer.active(), None);
    assert!(poll(&buyer, &policy(), demand(100_000), settling).is_none());

    // The replacement lands and buying resumes on it.
    buyer.funded(channel(2), CAPACITY, Some(Millis(2 * TTL_MS)));
    buyer.confirmed(channel(2));
    assert_eq!(buyer.active().expect("active").id, channel(2));
}
