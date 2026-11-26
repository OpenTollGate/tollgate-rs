//! A v1 device admitted, shaped, and refused — proven by the bytes the enforcer
//! actually allows, not by the rate it was told.
//!
//! `admit` decides whether to admit, `admit_enforcer::apply` puts it into force, and
//! the enforcer's own bucket decides what the device may send. A test that only read
//! back `shaping_rate` would prove the number was stored. This drives the bucket, so
//! what is proven is that a paid device is carried, an unpaid one is not, and the
//! rate it bought is the rate it gets.
//!
//! Loopback is used deliberately: it needs no kernel, no mesh and no FIPS, so this
//! runs anywhere. Its bucket is the same shape the `ip` enforcer's `tc` classes get
//! and the FIPS enforcer's transit rate gets — the mechanism under it differs, the
//! arithmetic does not.

use std::net::IpAddr;
use tollgate_core::AccessLevel;
use tollgate_net::bridge::Credit;
use tollgate_net::bridge::admit::admit;
use tollgate_net::bridge::admit_enforcer::{apply, close};
use tollgate_net::enforcer::{Enforcer, Loopback};

fn ip(s: &str) -> IpAddr {
    s.parse().expect("an address")
}

/// A paid device is admitted and gets the rate it bought; an unpaid one gets zero
/// bytes and never appears to the enforcer at all.
#[test]
fn a_paid_device_is_shaped_and_an_unpaid_one_is_not() {
    let enforcer = Loopback::new();
    let credit = Credit::new();
    credit
        .deposit("https://m", "byte", 100_000)
        .expect("a buyer paid");

    // The paid device: 1_000 bytes a second for 10 seconds.
    let paid = admit(&credit, "https://m", "byte", ip("192.0.2.7"), 1_000, 10)
        .expect("a paid device is admitted");
    let paid_handle = apply(&enforcer, &paid);
    assert_eq!(paid.spent, 10_000);

    // The unpaid device: ask for more than the credit that is left (90_000), so a
    // refusal is genuinely about money and not about my arithmetic. 100_000 a second
    // for 10 seconds is 1_000_000, which the buyer never paid.
    let refused = admit(&credit, "https://m", "byte", ip("192.0.2.8"), 100_000, 10);
    assert!(
        refused.is_err(),
        "an unpaid device was admitted with credit it never had"
    );

    // Now the part that matters: let a second pass and ask what each may send.
    let allowed = enforcer.take_allowance(paid_handle.peer, 1_000);
    assert!(
        allowed > 0,
        "a paid device was admitted but allowed to send nothing"
    );
    // One second at 1_000 B/s, with a burst cap, must be in that neighbourhood —
    // not the 10_000 it bought over ten seconds, and not unbounded.
    assert!(
        allowed <= 1_000 * 2,
        "a 1_000 B/s device was allowed {allowed} bytes in a second"
    );

    // The unpaid address was never registered, so its peer was not tracked.
    use tollgate_net::bridge::admit_enforcer::peer_for;
    assert!(
        !enforcer.peers().contains(&peer_for(ip("192.0.2.8"))),
        "an unpaid device reached the enforcer"
    );
}

/// What a device bought is what it is shaped to: two devices at different rates get
/// proportionally different allowances over the same interval.
#[test]
fn the_rate_a_device_bought_is_the_rate_it_gets() {
    let enforcer = Loopback::new();
    let credit = Credit::new();
    credit
        .deposit("https://m", "byte", 1_000_000)
        .expect("a buyer paid");

    let slow = admit(&credit, "https://m", "byte", ip("192.0.2.10"), 1_000, 5).expect("slow");
    let fast = admit(&credit, "https://m", "byte", ip("192.0.2.11"), 4_000, 5).expect("fast");
    let slow_handle = apply(&enforcer, &slow);
    let fast_handle = apply(&enforcer, &fast);

    assert_eq!(slow.spent, 5_000);
    assert_eq!(fast.spent, 20_000);

    // Same elapsed time, four times the rate.
    let slow_bytes = enforcer.take_allowance(slow_handle.peer, 1_000);
    let fast_bytes = enforcer.take_allowance(fast_handle.peer, 1_000);
    assert!(
        fast_bytes > slow_bytes,
        "a 4_000 B/s device ({fast_bytes}) was not allowed more than a 1_000 B/s one ({slow_bytes})"
    );
    let ratio = fast_bytes as f64 / slow_bytes as f64;
    assert!(
        (3.0..=5.0).contains(&ratio),
        "the allowance ratio {ratio} does not match the four-fold rate difference"
    );
}

/// Closing a device stops it: the next allowance is zero, and the enforcer no longer
/// tracks it. A closed admission that still carries traffic is a free ride.
#[test]
fn a_closed_admission_carries_nothing() {
    let enforcer = Loopback::new();
    let credit = Credit::new();
    credit
        .deposit("https://m", "byte", 100_000)
        .expect("a buyer paid");

    let admission =
        admit(&credit, "https://m", "byte", ip("192.0.2.20"), 2_000, 5).expect("admitted");
    let handle = apply(&enforcer, &admission);

    // It works first, so the test is not passing on a peer that was never admitted.
    assert!(enforcer.take_allowance(handle.peer, 1_000) > 0);

    close(&enforcer, &handle);
    assert_eq!(
        enforcer.take_allowance(handle.peer, 1_000),
        0,
        "a closed admission was still allowed to send"
    );
    // `take_allowance` recreates an entry for an unknown peer, so the tracking check
    // is that nothing carries an access level anymore — not that no entry exists.
    // Note: `carried(rate)` asks "carried at this rate", and AccessLevel::None is
    // carried whenever the rate is above zero, so the level itself is the thing to
    // assert on.
    assert!(
        enforcer
            .recorded_access(handle.peer)
            .is_none_or(|a| a == AccessLevel::None),
        "a closed admission is still carried: {:?}",
        enforcer.recorded_access(handle.peer)
    );
}

/// An allowance is bounded by the bucket, so a device cannot bank a minute of silence
/// and then send it all at once. That is the burst cap, and without it a shaped peer
/// could defeat the shape by idling.
#[test]
fn an_idle_device_cannot_bank_and_burst() {
    let enforcer = Loopback::new();
    let credit = Credit::new();
    credit
        .deposit("https://m", "byte", 1_000_000)
        .expect("a buyer paid");

    let admission =
        admit(&credit, "https://m", "byte", ip("192.0.2.30"), 1_000, 60).expect("admitted");
    let handle = apply(&enforcer, &admission);

    // A minute of silence must not become a minute of traffic.
    let after_idle = enforcer.take_allowance(handle.peer, 60_000);
    assert!(
        after_idle <= 1_000 * 2,
        "an idle 1_000 B/s device banked {after_idle} bytes"
    );
}
