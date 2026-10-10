//! Rate arithmetic.
//!
//! A proof holds a quantity and nothing else, so a rate only exists as a
//! quantity over a time. Everything that converts between a rate and a number
//! of units goes through these.
//!
//! All of it is done in `u128` and saturated back to `u64`. The inputs are
//! attacker-influenced — a payer chooses its grant, window and reserved rate —
//! and wrapping arithmetic here would hand it free capacity.

/// Units per second that `units` over `ms` milliseconds comes to.
///
/// Over no time at all it is an unbounded rate, and saturates rather than
/// dividing by zero.
pub fn rate_from(units: u64, ms: u64) -> u64 {
    if ms == 0 {
        return u64::MAX;
    }
    let rate = (units as u128) * 1_000 / (ms as u128);
    rate.min(u64::MAX as u128) as u64
}

/// Units drawn at `rate` over `ms` milliseconds.
///
/// The inverse of [`rate_from`], up to integer truncation.
pub fn units_in(rate: u64, ms: u64) -> u64 {
    let units = (rate as u128) * (ms as u128) / 1_000;
    units.min(u64::MAX as u128) as u64
}

/// The budget that lasts `window_ms` at a reserved `rate`: what a payer buying
/// time at a speed holds.
pub fn budget_for(rate: u64, window_ms: u64) -> u64 {
    units_in(rate, window_ms)
}

/// The fastest a payer with `remaining` units left can be carried so it cannot
/// move more than that in one tick of `tick_ms`, rounded down to a power of
/// two.
///
/// Rounded so the shaper is told something new only when the budget halves,
/// rather than on every tick as it drains — which, for a payer carried as fast
/// as the link allows, would be every tick of its life. Rounding down only
/// slows it, never lets it overrun.
pub fn per_tick_ceiling(remaining: u64, tick_ms: u64) -> u64 {
    let ceiling = rate_from(remaining, tick_ms.max(1));
    match ceiling {
        0 => 0,
        c => 1u64 << (63 - c.leading_zeros()),
    }
}
