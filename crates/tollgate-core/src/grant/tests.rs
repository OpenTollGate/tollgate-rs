//! The worked examples from `docs/design/core/tollgate-vouchers.md` and
//! `tollgate-protocol.md`, run against the implementation.
//!
//! These are the numbers the design argues from, so if any of them moves, the
//! documented accounting no longer describes what the code does.

use alloc::vec;
use alloc::vec::Vec;

use tollgate_protocol::{ChannelId, ChannelUpdate, ReasonCode, Signature};

use super::*;
use crate::config::{BurstPolicy, GrantPolicy};
use crate::meter::Counters;
use crate::time::Millis;

/// Big enough that no test trips the channel ceiling by accident — the tests
/// that mean to check it set their own.
const ROOMY: u64 = 1_000_000_000_000;

const SECOND: u64 = 1_000;
const HOUR: u64 = 3_600 * SECOND;
const DAY: u64 = 24 * HOUR;
const MB: u64 = 1_000_000;

fn channel(seed: u8) -> ChannelId {
    ChannelId([seed; 32])
}

fn policy() -> GrantPolicy {
    GrantPolicy::default()
}

fn admission(policy: &GrantPolicy) -> Admission<'_> {
    Admission {
        policy,
        reserved_elsewhere: 0,
    }
}

/// A provider that recognises one channel from this payer.
fn opened(capacity: u64) -> GrantState {
    let mut state = GrantState::new();
    state.open_channel(channel(1), capacity, None);
    state
}

/// The signature is checked by the host before core ever sees the message, so
/// its contents are irrelevant here.
fn update(ch: u8, cumulative: u64) -> ChannelUpdate {
    ChannelUpdate {
        channel_id: channel(ch),
        cumulative,
        signature: Signature([0; 64]),
    }
}

/// Evaluate and apply, the way the session does. Panics if refused, so a test
/// that expects a refusal calls [`evaluate_topup`] directly.
fn buy(
    state: &mut GrantState,
    updates: &[ChannelUpdate],
    window_ms: u64,
    reserved_rate: u64,
    now: Millis,
) -> u64 {
    let policy = policy();
    match evaluate_topup(state, admission(&policy), updates, window_ms, reserved_rate) {
        Verdict::Accept { ratchets, grant } => {
            state.apply(&ratchets, grant, window_ms, reserved_rate, now);
            grant
        }
        Verdict::Reject { reason, .. } => panic!("refused: {reason:?}"),
    }
}

/// Read the meter once a second from second `from` to second `to`, moving
/// `moved` each second, as a provider carrying the payer all that time would.
fn carry(state: &mut GrantState, from: u64, to: u64, moved: u64) {
    for t in from..to {
        state.draw(moved, Millis((t + 1) * SECOND));
    }
}

/// Read the meter every 100 ms through one second starting at `from_ms`,
/// moving `moved[i]` in the i-th tenth. Returns what was drawn.
fn tenths(state: &mut GrantState, from_ms: u64, moved: [u64; 10]) -> u64 {
    let mut drawn = 0;
    for (i, m) in moved.into_iter().enumerate() {
        drawn += state.draw(m, Millis(from_ms + (i as u64 + 1) * 100));
    }
    drawn
}

// ---------------------------------------------------------------------------
// Time at a speed
// ---------------------------------------------------------------------------

#[test]
fn time_at_a_speed_draws_the_reserved_rate_used_or_not() {
    // tollgate-vouchers.md, "Time at a Speed": 5 Mbit/s is 625,000 bytes a
    // second, bought in 10 s windows and renewed 2 s before the budget would
    // run out.
    const RATE: u64 = 625_000;
    let mut state = opened(ROOMY);

    buy(&mut state, &[update(1, 6_250_000)], 10_000, RATE, Millis(0));
    assert_eq!(state.remaining(), 6_250_000);
    assert_eq!(state.deadline(), Millis(10_000));

    // t=0–3 busy at exactly the rate, t=3–5 idle, t=5–8 busy again: every
    // second costs the reserved rate all the same.
    carry(&mut state, 0, 3, RATE);
    carry(&mut state, 3, 5, 0);
    carry(&mut state, 5, 8, RATE);
    assert_eq!(state.remaining(), 1_250_000, "left over at t=8, and kept");

    // The buyer adds back what was drawn.
    let grant = buy(
        &mut state,
        &[update(1, 11_250_000)],
        10_000,
        RATE,
        Millis(8_000),
    );
    assert_eq!(grant, 5_000_000);
    assert_eq!(
        state.remaining(),
        6_250_000,
        "1.25 M + 5 M: nothing forfeit"
    );
    assert_eq!(state.deadline(), Millis(18_000));

    carry(&mut state, 8, 16, RATE);
    buy(
        &mut state,
        &[update(1, 16_250_000)],
        10_000,
        RATE,
        Millis(16_000),
    );
    assert_eq!(state.remaining(), 6_250_000);
    assert_eq!(state.deadline(), Millis(26_000));
    assert_eq!(
        state.authorized(),
        16_250_000,
        "5 M every 8 s after the first: 625,000 a second, each second paid once"
    );
}

#[test]
fn a_stopped_buyer_drains_to_zero_at_the_deadline_with_nothing_left_to_expire() {
    // Sized at the reserved rate times the window, the budget is used up
    // exactly when the deadline comes.
    const RATE: u64 = 625_000;
    let mut state = opened(ROOMY);
    buy(&mut state, &[update(1, 6_250_000)], 10_000, RATE, Millis(0));

    carry(&mut state, 0, 10, 0);
    assert_eq!(state.remaining(), 0);
    assert!(
        !state.expire_if_due(Millis(10_000)),
        "nothing left to forfeit"
    );
}

#[test]
fn a_busy_second_costs_what_it_moved() {
    // The reserved rate is a floor, not a cap on what is drawn.
    let mut state = opened(ROOMY);
    buy(
        &mut state,
        &[update(1, 6_250_000)],
        10_000,
        625_000,
        Millis(0),
    );

    assert_eq!(state.draw(2_000_000, Millis(SECOND)), 2_000_000);
    assert_eq!(state.remaining(), 4_250_000);
}

#[test]
fn a_burst_inside_a_second_at_or_under_the_reserved_rate_costs_the_reserved_rate() {
    // The rule is per second, not per meter reading. Read ten times a second,
    // a second whose traffic all came in one tenth is drawn the reserved
    // rate, not the reserved rate for nine idle tenths plus the burst.
    const RATE: u64 = 625_000;
    let mut state = opened(ROOMY);
    buy(&mut state, &[update(1, 6_250_000)], 10_000, RATE, Millis(0));

    let mut burst = [0; 10];
    burst[4] = 400_000;
    assert_eq!(tenths(&mut state, 0, burst), RATE, "under the rate");

    burst[4] = RATE;
    assert_eq!(tenths(&mut state, 1_000, burst), RATE, "at the rate");

    // What moved is drawn as it is read, so the budget the shaper sees near
    // zero is current; only the floor waits for the second to end.
    let before = state.remaining();
    for t in [100, 200, 300, 400] {
        state.draw(0, Millis(2_000 + t));
    }
    state.draw(400_000, Millis(2_500));
    assert_eq!(before - state.remaining(), 400_000, "drawn when it moved");
    for t in [600, 700, 800, 900, 1_000] {
        state.draw(0, Millis(2_000 + t));
    }
    assert_eq!(before - state.remaining(), RATE, "topped up to the floor");
}

#[test]
fn a_burst_above_the_reserved_rate_costs_what_it_moved() {
    const RATE: u64 = 625_000;
    let mut state = opened(ROOMY);
    buy(&mut state, &[update(1, 6_250_000)], 10_000, RATE, Millis(0));

    let mut burst = [0; 10];
    burst[2] = 500_000;
    burst[7] = 400_000;
    assert_eq!(tenths(&mut state, 0, burst), 900_000);
    assert_eq!(state.remaining(), 6_250_000 - 900_000);
}

#[test]
fn a_reserved_rate_changed_mid_second_splits_it() {
    // Half a second at 625,000 a second, then a purchase that doubles it: the
    // first half is drawn at the old rate, and a new second starts at the new
    // one.
    let mut state = opened(ROOMY);
    buy(
        &mut state,
        &[update(1, 10_000_000)],
        10_000,
        625_000,
        Millis(0),
    );
    for t in [100, 200, 300, 400, 500] {
        state.draw(0, Millis(t));
    }
    buy(
        &mut state,
        &[update(1, 10_000_001)],
        10_000,
        1_250_000,
        Millis(500),
    );
    assert_eq!(state.remaining(), 10_000_001 - 312_500, "half at the old");

    let drawn = tenths(&mut state, 500, [0; 10]);
    assert_eq!(drawn, 1_250_000, "a whole second at the new");
}

#[test]
fn a_purchase_at_the_same_rate_does_not_split_the_second() {
    // Renewing mid-second changes nothing about the second being drawn: a
    // burst on either side of the purchase still counts against one floor.
    const RATE: u64 = 625_000;
    let mut state = opened(ROOMY);
    buy(&mut state, &[update(1, 6_250_000)], 10_000, RATE, Millis(0));
    state.draw(300_000, Millis(400));
    buy(
        &mut state,
        &[update(1, 6_550_000)],
        10_000,
        RATE,
        Millis(500),
    );
    state.draw(300_000, Millis(900));
    state.draw(0, Millis(1_000));
    assert_eq!(state.remaining(), 6_550_000 - RATE);
}

#[test]
fn a_session_ending_mid_second_draws_that_part_of_it() {
    // Carried for 400 ms of a second, then not: 4/10 of the floor, or what
    // moved in that time if more.
    const RATE: u64 = 625_000;
    let mut state = opened(ROOMY);
    buy(&mut state, &[update(1, 6_250_000)], 10_000, RATE, Millis(0));
    for t in [100, 200, 300, 400] {
        state.draw(0, Millis(t));
    }
    assert_eq!(state.pause(), 250_000);
    assert_eq!(state.remaining(), 6_250_000 - 250_000);

    // Not carried: nothing is drawn, whatever the time.
    assert_eq!(state.pause(), 0);

    // Carried again from a reading that starts a second, which draws only
    // what it moved; then 400 ms more moving 300,000, and the session ends.
    let before = state.remaining();
    state.draw(0, Millis(5_000));
    state.draw(300_000, Millis(5_400));
    state.end_reservation();
    assert_eq!(before - state.remaining(), 300_000, "more than 4/10 of it");
    assert_eq!(state.reserved_rate(), 0);
}

// ---------------------------------------------------------------------------
// Pay for what you use
// ---------------------------------------------------------------------------

#[test]
fn pay_per_use_draws_only_what_moved_and_expires_at_the_deadline() {
    // tollgate-vouchers.md, "Pay for What You Use": 1 GB from a hotspot that
    // accepts windows up to 30 days and lets payers reserve nothing.
    let mut state = opened(ROOMY);

    buy(&mut state, &[update(1, 1_000 * MB)], 30 * DAY, 0, Millis(0));
    assert_eq!(state.deadline(), Millis(30 * DAY));

    state.draw(300 * MB, Millis(SECOND));
    assert_eq!(state.remaining(), 700 * MB);

    // An idle hour draws nothing.
    carry(&mut state, 0, 3_600, 0);
    assert_eq!(state.remaining(), 700 * MB);

    state.draw(250 * MB, Millis(SECOND));
    assert_eq!(state.remaining(), 450 * MB);

    // The phone adds back what it used.
    let grant = buy(
        &mut state,
        &[update(1, 1_550 * MB)],
        30 * DAY,
        0,
        Millis(3 * DAY),
    );
    assert_eq!(grant, 550 * MB);
    assert_eq!(state.remaining(), 1_000 * MB);
    assert_eq!(state.deadline(), Millis(33 * DAY));

    // Nothing bought since: what is left expires, and the payment is kept.
    assert!(!state.expire_if_due(Millis(33 * DAY - 1)));
    assert!(state.expire_if_due(Millis(33 * DAY)));
    assert_eq!(state.remaining(), 0);
    assert_eq!(state.authorized(), 1_550 * MB, "the payment is kept");
}

// ---------------------------------------------------------------------------
// A grant adds; the deadline never comes closer
// ---------------------------------------------------------------------------

#[test]
fn renewing_early_keeps_what_was_left() {
    // Nothing forfeit at a purchase: a payer renewing early pays for each unit
    // once.
    let mut state = opened(ROOMY);
    buy(&mut state, &[update(1, 1_000_000)], 10_000, 0, Millis(0));
    state.draw(100_000, Millis(SECOND));

    buy(
        &mut state,
        &[update(1, 1_100_000)],
        10_000,
        0,
        Millis(1_000),
    );
    assert_eq!(state.remaining(), 1_000_000);
}

#[test]
fn a_short_window_on_a_later_purchase_does_not_shorten_the_budget() {
    // The deadline is the later of the old one and now plus the window.
    let mut state = opened(ROOMY);
    buy(&mut state, &[update(1, 1_000)], 30 * DAY, 0, Millis(0));
    buy(&mut state, &[update(1, 2_000)], 10_000, 0, Millis(5_000));
    assert_eq!(state.deadline(), Millis(30 * DAY));

    // A later one does move it.
    buy(&mut state, &[update(1, 3_000)], 30 * DAY, 0, Millis(DAY));
    assert_eq!(state.deadline(), Millis(31 * DAY));
}

#[test]
fn a_purchase_after_the_deadline_starts_a_fresh_budget() {
    // What was left expired at the deadline, even if no tick noticed.
    let mut state = opened(ROOMY);
    buy(&mut state, &[update(1, 1_000)], 10_000, 0, Millis(0));
    buy(&mut state, &[update(1, 1_500)], 10_000, 0, Millis(20_000));
    assert_eq!(state.remaining(), 500);
    assert_eq!(state.deadline(), Millis(30_000));
}

#[test]
fn the_reserved_rate_replaces_the_one_before_up_or_down() {
    let mut state = opened(ROOMY);
    buy(&mut state, &[update(1, 1_000)], 10_000, 625_000, Millis(0));
    assert_eq!(state.reserved_rate(), 625_000);
    buy(&mut state, &[update(1, 2_000)], 10_000, 100, Millis(1_000));
    assert_eq!(state.reserved_rate(), 100);
    buy(&mut state, &[update(1, 3_000)], 10_000, 0, Millis(2_000));
    assert_eq!(state.reserved_rate(), 0);
}

// ---------------------------------------------------------------------------
// Running out
// ---------------------------------------------------------------------------

#[test]
fn an_overrun_is_held_at_zero_and_not_carried_as_debt() {
    let mut state = opened(ROOMY);
    buy(&mut state, &[update(1, 1_000)], 10_000, 0, Millis(0));

    assert_eq!(
        state.draw(5_000, Millis(SECOND)),
        1_000,
        "only what was there"
    );
    assert_eq!(state.consumed(), state.authorized());

    // The next purchase is all there, not eaten by the overrun.
    buy(&mut state, &[update(1, 3_000)], 10_000, 0, Millis(1_000));
    assert_eq!(state.remaining(), 2_000);
}

#[test]
fn the_reservation_ends_when_the_budget_reaches_zero() {
    let mut state = opened(ROOMY);
    buy(
        &mut state,
        &[update(1, 1_000_000)],
        10_000,
        500_000,
        Millis(0),
    );
    carry(&mut state, 0, 2, 0);
    assert_eq!(state.remaining(), 0);
    assert_eq!(state.reserved_rate(), 0);
}

#[test]
fn the_reservation_ends_at_the_deadline() {
    let mut state = opened(ROOMY);
    buy(
        &mut state,
        &[update(1, ROOMY / 2)],
        10_000,
        500_000,
        Millis(0),
    );
    assert!(state.expire_if_due(Millis(10_000)));
    assert_eq!(state.reserved_rate(), 0);
    assert_eq!(state.remaining(), 0);
}

// ---------------------------------------------------------------------------
// Shaping
// ---------------------------------------------------------------------------

const NO_BURST: BurstPolicy = BurstPolicy {
    rate: 0,
    unreserved_rate: u64::MAX,
};

#[test]
fn a_reserved_payer_is_carried_at_exactly_its_rate_by_default() {
    let mut state = opened(ROOMY);
    buy(
        &mut state,
        &[update(1, ROOMY / 2)],
        10_000,
        625_000,
        Millis(0),
    );
    assert_eq!(
        state.shaping_rate(Millis(1), NO_BURST, 4_096, SECOND),
        625_000
    );
}

#[test]
fn a_burst_carries_a_reserved_payer_above_its_rate_but_never_below() {
    let mut state = opened(ROOMY);
    buy(
        &mut state,
        &[update(1, ROOMY / 2)],
        10_000,
        625_000,
        Millis(0),
    );
    let burst = |rate| BurstPolicy {
        rate,
        unreserved_rate: u64::MAX,
    };
    assert_eq!(
        state.shaping_rate(Millis(1), burst(2_500_000), 0, SECOND),
        2_500_000
    );
    assert_eq!(
        state.shaping_rate(Millis(1), burst(100_000), 0, SECOND),
        625_000,
        "a burst below the reservation does not slow it"
    );
}

#[test]
fn a_payer_that_reserved_nothing_is_carried_at_the_unreserved_rate() {
    let mut state = opened(ROOMY);
    buy(&mut state, &[update(1, ROOMY / 2)], 10_000, 0, Millis(0));
    let unreserved = |rate| BurstPolicy {
        rate: 0,
        unreserved_rate: rate,
    };
    assert_eq!(
        state.shaping_rate(Millis(1), unreserved(2_500_000), 4_096, SECOND),
        2_500_000
    );
    assert_eq!(
        state.shaping_rate(Millis(1), unreserved(0), 4_096, SECOND),
        4_096,
        "with none, only the allowance until it reserves"
    );
}

#[test]
fn near_the_end_a_payer_cannot_move_more_in_a_tick_than_is_left() {
    // tollgate-protocol.md, Grant State: with a one-second tick and 4 MB left,
    // the payer is carried at no more than 4 MB/s.
    let mut state = opened(ROOMY);
    buy(&mut state, &[update(1, 4 << 20)], 10_000, 0, Millis(0));
    assert_eq!(
        state.shaping_rate(Millis(1), NO_BURST, 0, SECOND),
        4 << 20,
        "4 MiB/s"
    );
    // Rounded down to a power of two, so it changes only as the budget
    // halves, and never above what is left.
    state.draw(1 << 20, Millis(SECOND));
    assert_eq!(
        state.shaping_rate(Millis(1), NO_BURST, 0, SECOND),
        2 << 20,
        "3 MiB left: 2 MiB/s"
    );
    // A shorter tick allows more per second.
    assert_eq!(state.shaping_rate(Millis(1), NO_BURST, 0, 100), 16 << 20);
}

#[test]
fn the_clip_never_takes_a_payer_below_the_allowance() {
    let mut state = opened(ROOMY);
    buy(&mut state, &[update(1, 10)], 10_000, 0, Millis(0));
    assert_eq!(
        state.shaping_rate(Millis(1), NO_BURST, 4_096, SECOND),
        4_096
    );
}

#[test]
fn an_expired_budget_falls_back_to_the_allowance_not_to_silence() {
    // This is what keeps a link alive between purchases: without it the payer
    // could never send the TopUp that revives it.
    const ALLOWANCE: u64 = 4_096;
    let mut state = opened(ROOMY);
    buy(
        &mut state,
        &[update(1, ROOMY / 2)],
        1_000,
        100_000,
        Millis(0),
    );

    assert_eq!(
        state.shaping_rate(Millis(500), NO_BURST, ALLOWANCE, SECOND),
        100_000
    );
    assert_eq!(
        state.shaping_rate(Millis(1_500), NO_BURST, ALLOWANCE, SECOND),
        ALLOWANCE
    );
}

#[test]
fn a_peer_that_has_never_paid_still_gets_the_allowance() {
    let state = GrantState::new();
    assert_eq!(
        state.shaping_rate(Millis(0), NO_BURST, 4_096, SECOND),
        4_096
    );
    assert_eq!(
        state.shaping_rate(Millis(0), NO_BURST, 0, SECOND),
        0,
        "allowance off by default"
    );
}

#[test]
fn the_allowance_is_a_floor_not_an_addition() {
    // A payer reserving more than the allowance does not also get the
    // allowance on top.
    let mut state = opened(ROOMY);
    buy(
        &mut state,
        &[update(1, ROOMY / 2)],
        10_000,
        100_000,
        Millis(0),
    );
    assert_eq!(
        state.shaping_rate(Millis(1), NO_BURST, 4_096, SECOND),
        100_000
    );
}

// ---------------------------------------------------------------------------
// The from-payer weight
// ---------------------------------------------------------------------------

#[test]
fn the_from_payer_weight_examples_hold() {
    // tollgate-vouchers.md, "The From-Payer Weight".
    let phone = Counters {
        to_payer: 300 * MB,
        from_payer: 20 * MB,
    };
    assert_eq!(phone.weighted(1), 320 * MB, "Wi-Fi at weight 1");

    let relay = Counters {
        to_payer: 1_000 * MB,
        from_payer: 50 * MB,
    };
    assert_eq!(relay.weighted(10), 1_500 * MB, "a 100/10 line at weight 10");

    let tap = Counters {
        to_payer: 500,
        from_payer: 0,
    };
    assert_eq!(tap.weighted(10), 500, "nothing flows back from a glass");
}

#[test]
fn peering_at_weight_zero_has_each_payer_pay_for_what_flows_to_it() {
    // B sends A 1,000 MB, A sends B 200 MB, both weights 0. Counted at B:
    let at_b = Counters {
        to_payer: 1_000 * MB,
        from_payer: 200 * MB,
    };
    // Sale 1, B sells to A: A's budget with B.
    assert_eq!(at_b.weighted(0), 1_000 * MB);
    // Sale 2, A sells to B: B's own count of its budget with A reads the
    // same counts the other way round.
    assert_eq!(at_b.swapped().weighted(0), 200 * MB);
}

#[test]
fn the_weighted_draw_saturates() {
    // Both counts are what the payer moved; wrapping would hand it capacity.
    let huge = Counters {
        to_payer: u64::MAX,
        from_payer: u64::MAX,
    };
    assert_eq!(huge.weighted(u16::MAX), u64::MAX);
}

#[test]
fn a_counter_that_resets_reports_no_growth_rather_than_underflowing() {
    let earlier = Counters {
        to_payer: 5_000,
        from_payer: 5_000,
    };
    let after_reset = Counters::ZERO;
    assert_eq!(after_reset.delta_since(earlier), Counters::ZERO);
}

// ---------------------------------------------------------------------------
// Admission
// ---------------------------------------------------------------------------

#[test]
fn a_reserved_rate_beyond_capacity_is_refused_with_the_rate_still_free() {
    // tollgate-protocol.md, "Changing the Reserved Rate". A holds 4.375 M at
    // t=11, signed 11.25 M; other payers have reserved enough that 8 M/s is
    // free.
    const RATE: u64 = 625_000;
    let policy = GrantPolicy {
        max_rate: Some(18_000_000),
        ..GrantPolicy::default()
    };
    let elsewhere = Admission {
        policy: &policy,
        reserved_elsewhere: 10_000_000,
    };
    let mut state = opened(ROOMY);
    let apply = |state: &mut GrantState, cumulative, reserve, now| match evaluate_topup(
        state,
        elsewhere,
        &[update(1, cumulative)],
        10_000,
        reserve,
    ) {
        Verdict::Accept { ratchets, grant } => {
            state.apply(&ratchets, grant, 10_000, reserve, Millis(now))
        }
        Verdict::Reject { reason, .. } => panic!("refused: {reason:?}"),
    };
    apply(&mut state, 6_250_000, RATE, 0);
    carry(&mut state, 0, 8, RATE);
    apply(&mut state, 11_250_000, RATE, 8_000);
    carry(&mut state, 8, 11, RATE);
    assert_eq!(state.remaining(), 4_375_000);

    // t=11: 20 M/s does not fit.
    assert_eq!(
        evaluate_topup(
            &state,
            elsewhere,
            &[update(1, 21_250_000)],
            10_000,
            20_000_000
        ),
        Verdict::Reject {
            reason: ReasonCode::RateExceedsCapacity,
            max_reserved_rate: 8_000_000,
        }
    );
    assert_eq!(state.reserved_rate(), RATE, "reservation unchanged");

    // t=12: one gap later, the same purchase at 8 M/s.
    carry(&mut state, 11, 12, RATE);
    assert_eq!(state.remaining(), 3_750_000);
    apply(&mut state, 21_250_000, 8_000_000, 12_000);
    assert_eq!(state.remaining(), 13_750_000);
    assert_eq!(state.reserved_rate(), 8_000_000);
    assert_eq!(state.deadline(), Millis(22_000), "max(t=18, t=22)");
}

#[test]
fn lowering_a_reservation_is_always_accepted() {
    // Even when capacity has since been promised to others past the ceiling.
    let policy = GrantPolicy {
        max_rate: Some(1_000_000),
        ..GrantPolicy::default()
    };
    let mut state = opened(ROOMY);
    buy(&mut state, &[update(1, 1_000)], 10_000, 900_000, Millis(0));
    let crowded = Admission {
        policy: &policy,
        reserved_elsewhere: 1_000_000,
    };
    assert!(matches!(
        evaluate_topup(&state, crowded, &[update(1, 2_000)], 10_000, 500_000),
        Verdict::Accept { .. }
    ));
    assert!(matches!(
        evaluate_topup(&state, crowded, &[update(1, 2_000)], 10_000, 900_000),
        Verdict::Accept { .. }
    ));
    assert!(matches!(
        evaluate_topup(&state, crowded, &[update(1, 2_000)], 10_000, 900_001),
        Verdict::Reject {
            reason: ReasonCode::RateExceedsCapacity,
            max_reserved_rate: 0,
        }
    ));
}

#[test]
fn a_reserved_rate_below_the_smallest_is_out_of_range() {
    // A provider that sells only time at a speed.
    let policy = GrantPolicy {
        min_reserved_rate: 125_000,
        ..GrantPolicy::default()
    };
    let state = opened(ROOMY);
    assert!(matches!(
        evaluate_topup(&state, admission(&policy), &[update(1, 1_000)], 10_000, 0),
        Verdict::Reject {
            reason: ReasonCode::OutOfRange,
            ..
        }
    ));
    assert!(matches!(
        evaluate_topup(
            &state,
            admission(&policy),
            &[update(1, 1_000)],
            10_000,
            125_000
        ),
        Verdict::Accept { .. }
    ));
}

#[test]
fn a_window_outside_the_advertised_range_is_refused() {
    let policy = policy();
    let state = opened(ROOMY);

    for window in [0, 1, policy.min_window_ms - 1, policy.max_window_ms + 1] {
        assert!(
            matches!(
                evaluate_topup(&state, admission(&policy), &[update(1, 1_000)], window, 0),
                Verdict::Reject {
                    reason: ReasonCode::OutOfRange,
                    ..
                }
            ),
            "window {window} ms should be refused"
        );
    }

    for window in [policy.min_window_ms, 5_000, policy.max_window_ms] {
        assert!(
            matches!(
                evaluate_topup(&state, admission(&policy), &[update(1, 1_000)], window, 0),
                Verdict::Accept { .. }
            ),
            "window {window} ms should be accepted"
        );
    }
}

#[test]
fn the_default_terms_are_the_documented_ones() {
    // tollgate-configuration.md, Grants.
    let policy = GrantPolicy::default();
    assert_eq!(
        (policy.min_window_ms, policy.max_window_ms),
        (1_000, 2_592_000_000)
    );
    assert_eq!(policy.min_reserved_rate, 0);
    assert_eq!(policy.min_topup_gap_ms, 1_000);
    assert_eq!(policy.max_rate, None);
    assert!(policy.window_range_valid());
}

#[test]
fn a_window_shorter_than_the_gap_cannot_be_advertised() {
    // A budget could expire before its payer is allowed to renew it.
    let policy = GrantPolicy {
        min_window_ms: 500,
        min_topup_gap_ms: 1_000,
        ..GrantPolicy::default()
    };
    assert!(!policy.window_range_valid());
    let inverted = GrantPolicy {
        min_window_ms: 20_000,
        max_window_ms: 10_000,
        ..GrantPolicy::default()
    };
    assert!(!inverted.window_range_valid());
}

#[test]
fn an_unset_rate_ceiling_means_the_link_is_the_limit() {
    let policy = policy();
    assert_eq!(policy.max_rate, None);
    let admission = Admission {
        policy: &policy,
        reserved_elsewhere: u64::MAX,
    };
    assert_eq!(admission.rate_available(), u64::MAX);
}

#[test]
fn a_topup_inside_the_gap_is_too_soon_and_does_not_move_the_time() {
    let mut state = opened(ROOMY);
    assert!(
        !state.too_soon(Millis(0), 1_000),
        "the first is never too soon"
    );
    state.topup_checked(Millis(0));
    assert!(state.too_soon(Millis(999), 1_000));
    // Asking did not move it.
    assert!(!state.too_soon(Millis(1_000), 1_000));
}

// ---------------------------------------------------------------------------
// A budget belongs to the payer
// ---------------------------------------------------------------------------

#[test]
fn a_new_session_keeps_the_budget_and_drops_the_reservation() {
    let mut state = opened(ROOMY);
    buy(
        &mut state,
        &[update(1, 1_000 * MB)],
        30 * DAY,
        125_000,
        Millis(0),
    );
    state.draw(300 * MB, Millis(SECOND));
    state.topup_checked(Millis(0));

    state.restart(Millis(HOUR));
    assert_eq!(
        state.remaining(),
        700 * MB,
        "the phone still has its 700 MB"
    );
    assert_eq!(state.deadline(), Millis(30 * DAY));
    assert_eq!(
        state.reserved_rate(),
        0,
        "it reserves again with its next TopUp"
    );
    assert_eq!(state.last_topup(), None);
    assert_eq!(
        state.channel(channel(1)).expect("kept").signed,
        1_000 * MB,
        "and the channel carries on where it was"
    );
}

#[test]
fn a_budget_kept_past_its_deadline_is_nothing() {
    let budget = Budget {
        remaining: 700 * MB,
        deadline: Millis(30 * DAY),
    };
    let mut state = GrantState::new();
    state.restore(budget, Millis(31 * DAY));
    assert_eq!(state.remaining(), 0);
    assert_eq!(budget.at(Millis(30 * DAY)), Budget::NONE);

    state.restore(budget, Millis(DAY));
    assert_eq!(state.remaining(), 700 * MB);
    assert_eq!(state.budget(Millis(DAY)), budget);
    assert_eq!(budget.expires_in_ms(Millis(DAY)), 29 * DAY);
    assert_eq!(Budget::NONE.expires_in_ms(Millis(DAY)), 0);
}

#[test]
fn settling_a_channel_does_not_touch_the_budget() {
    // The settlement collects the money that bought it, spent or not.
    let mut state = opened(ROOMY);
    buy(&mut state, &[update(1, 1_000_000)], 10_000, 0, Millis(0));
    state.close_channel(channel(1));
    assert_eq!(state.remaining(), 1_000_000);
    assert!(state.is_live(Millis(1)));
}

// ---------------------------------------------------------------------------
// A purchase spanning several channels
// ---------------------------------------------------------------------------

#[test]
fn the_grant_is_the_combined_increase_across_every_channel() {
    // The channel in use is topped to its capacity and the remainder starts the
    // replacement, in one purchase.
    let mut state = GrantState::new();
    state.open_channel(channel(1), 1_000_000, None);
    state.open_channel(channel(2), 1_000_000, None);

    buy(&mut state, &[update(1, 800_000)], 2_000, 0, Millis(0));
    assert_eq!(state.authorized(), 800_000);

    let grant = buy(
        &mut state,
        &[update(1, 1_000_000), update(2, 600_000)],
        2_000,
        0,
        Millis(1_000),
    );
    assert_eq!(
        grant, 800_000,
        "200 k from the old channel, 600 k from the new"
    );
    assert_eq!(state.remaining(), 1_600_000, "added to what was left");
}

#[test]
fn a_purchase_is_refused_in_full_if_any_update_is_bad() {
    // Applying some of them would leave the grant a different size from the one
    // the payer asked for and paid for.
    let mut state = GrantState::new();
    state.open_channel(channel(1), 1_000_000, None);
    state.open_channel(channel(2), 1_000_000, None);
    buy(&mut state, &[update(1, 500_000)], 2_000, 0, Millis(0));

    let policy = policy();
    let verdict = evaluate_topup(
        &state,
        admission(&policy),
        // The second does not increase.
        &[update(1, 600_000), update(2, 0)],
        2_000,
        0,
    );
    assert!(matches!(
        verdict,
        Verdict::Reject {
            reason: ReasonCode::GrantInvalid,
            ..
        }
    ));
    assert_eq!(
        state.channel(channel(1)).expect("channel").signed,
        500_000,
        "the good update was not applied either"
    );
}

#[test]
fn the_same_channel_twice_in_one_purchase_is_refused() {
    // The second reading of `signed` would be stale and its delta counted from
    // the wrong base, inflating the grant.
    let state = opened(1_000_000);
    let policy = policy();
    assert!(matches!(
        evaluate_topup(
            &state,
            admission(&policy),
            &[update(1, 100_000), update(1, 200_000)],
            2_000,
            0
        ),
        Verdict::Reject {
            reason: ReasonCode::GrantInvalid,
            ..
        }
    ));
}

#[test]
fn an_update_on_an_unrecognised_channel_is_refused() {
    // Either never funded, or already settled. Refusing is what stops the
    // recognised set growing with every rollover a long session performs.
    let state = opened(1_000_000);
    let policy = policy();
    assert!(matches!(
        evaluate_topup(&state, admission(&policy), &[update(9, 100_000)], 2_000, 0),
        Verdict::Reject {
            reason: ReasonCode::FundingInvalid,
            ..
        }
    ));
}

#[test]
fn a_settled_channel_stops_being_recognised() {
    let mut state = opened(1_000_000);
    buy(&mut state, &[update(1, 500_000)], 2_000, 0, Millis(0));
    state.close_channel(channel(1));

    let policy = policy();
    assert!(matches!(
        evaluate_topup(&state, admission(&policy), &[update(1, 600_000)], 2_000, 0),
        Verdict::Reject {
            reason: ReasonCode::FundingInvalid,
            ..
        }
    ));
}

#[test]
fn a_channel_drained_to_capacity_is_reported_for_settlement() {
    let mut state = opened(1_000_000);
    assert_eq!(state.exhausted_channels().count(), 0);

    buy(&mut state, &[update(1, 1_000_000)], 2_000, 0, Millis(0));
    assert_eq!(
        state.exhausted_channels().collect::<vec::Vec<_>>(),
        vec![channel(1)]
    );
}

#[test]
fn a_cumulative_that_does_not_increase_is_refused() {
    // Replays and reorders are what make fire-and-forget purchases safe.
    let policy = policy();
    let mut state = opened(ROOMY);
    buy(&mut state, &[update(1, 10_000)], 1_000, 0, Millis(0));

    for cumulative in [0, 5_000, 10_000] {
        assert!(
            matches!(
                evaluate_topup(
                    &state,
                    admission(&policy),
                    &[update(1, cumulative)],
                    1_000,
                    0
                ),
                Verdict::Reject {
                    reason: ReasonCode::GrantInvalid,
                    ..
                }
            ),
            "cumulative {cumulative} should not ratchet"
        );
    }

    assert!(matches!(
        evaluate_topup(&state, admission(&policy), &[update(1, 10_001)], 1_000, 0),
        Verdict::Accept { grant: 1, .. }
    ));
}

#[test]
fn a_grant_beyond_the_channel_is_refused() {
    let policy = policy();
    let state = opened(50_000);

    assert!(matches!(
        evaluate_topup(&state, admission(&policy), &[update(1, 50_001)], 1_000, 0),
        Verdict::Reject {
            reason: ReasonCode::GrantExceedsChannel,
            ..
        }
    ));
    assert!(matches!(
        evaluate_topup(&state, admission(&policy), &[update(1, 50_000)], 1_000, 0),
        Verdict::Accept { .. }
    ));
}

// ---------------------------------------------------------------------------
// Channel expiry
// ---------------------------------------------------------------------------

#[test]
fn a_channel_is_due_for_settlement_ahead_of_its_expiry() {
    // Past expiry the funder can reclaim the channel, and everything already
    // paid on it with it. The receiver settles with time to spare for a retry.
    let mut state = GrantState::new();
    state.open_channel(channel(1), 1_000_000, Some(Millis(3_600_000)));
    state.open_channel(channel(2), 1_000_000, Some(Millis(7_200_000)));
    state.open_channel(channel(3), 1_000_000, None);

    let lead = 30_000;
    assert_eq!(
        state.expiring_channels(Millis(3_570_000 - 1), lead).count(),
        0
    );
    let due: Vec<_> = state.expiring_channels(Millis(3_570_000), lead).collect();
    assert_eq!(due, [channel(1)], "only the one about to expire");

    assert_eq!(
        state.expiring_channels(Millis(u64::MAX), lead).count(),
        2,
        "a channel with no expiry is never due"
    );
}

#[test]
fn reverifying_a_channel_refreshes_its_expiry_without_touching_the_ratchet() {
    let mut state = GrantState::new();
    state.open_channel(channel(1), 1_000_000, Some(Millis(1_000)));
    buy(&mut state, &[update(1, 500_000)], 2_000, 0, Millis(0));

    state.open_channel(channel(1), 1_000_000, Some(Millis(9_000)));
    let c = state.channel(channel(1)).expect("still recognised");
    assert_eq!(c.expires_at, Some(Millis(9_000)));
    assert_eq!(c.signed, 500_000);
}

// ---------------------------------------------------------------------------
// Arithmetic
// ---------------------------------------------------------------------------

#[test]
fn rate_arithmetic_saturates_rather_than_wrapping() {
    assert_eq!(rate_from(u64::MAX, 1), u64::MAX);
    assert_eq!(rate_from(1, 0), u64::MAX, "no time at all is unbounded");
    assert_eq!(units_in(u64::MAX, u64::MAX), u64::MAX);
    assert_eq!(budget_for(625_000, 10_000), 6_250_000);
    assert_eq!(per_tick_ceiling(0, SECOND), 0);
    assert_eq!(per_tick_ceiling(u64::MAX, 1), 1 << 63);
}
