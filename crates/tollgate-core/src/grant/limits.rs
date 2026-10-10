//! Rate arithmetic.
//!
//! A proof holds a quantity and nothing else, so a rate only exists as a
//! quantity over a time. Everything that converts between a rate and a number
//! of units goes through these.
//!
//! All of it is done in `u128` and saturated back to `u64`. The inputs are
//! attacker-influenced — a payer chooses its grant, window and reserved rate —
//! and wrapping arithmetic here would hand it free capacity.

use crate::time::Millis;

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

/// How long the one rule's floor is applied over: a second.
pub const SECOND_MS: u64 = 1_000;

/// The second the one rule is being applied over, while a payer is carried.
///
/// The rule is `max(moved, reserved rate × 1 s)` for each second, not for each
/// meter reading. Readings come more often than that, and traffic is bursty
/// within a second: drawn per reading, a second whose traffic all came in one
/// reading would pay the reserved rate for every other reading as well, on top
/// of what it moved.
///
/// So what moved is drawn as it is read, which keeps the budget current for
/// the shaper near zero, and the floor is settled when a second is complete:
/// whatever the reserved rate came to beyond what moved in it. A reading that
/// spans several seconds completes them together.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Second {
    /// When it started.
    start: Millis,
    /// When it was last read.
    at: Millis,
    /// Units moved in it so far, already drawn.
    moved: u64,
}

impl Second {
    /// A second starting at `now`, with nothing moved in it yet.
    pub fn starting(now: Millis) -> Self {
        Self {
            start: now,
            at: now,
            moved: 0,
        }
    }

    /// When it was last read: as far as the payer is known to have been
    /// carried.
    pub fn last_read(&self) -> Millis {
        self.at
    }

    /// Take a reading at `now`: `moved` units since the last, at a reserved
    /// `rate`. Returns the units owed for it: what moved, plus the floor of
    /// every second it completes, beyond what moved in them.
    pub fn read(&mut self, moved: u64, now: Millis, rate: u64) -> u64 {
        self.moved = self.moved.saturating_add(moved);
        self.at = self.at.max(now);
        let whole = self.at.saturating_since(self.start) / SECOND_MS;
        if whole == 0 {
            return moved;
        }
        let floor = units_in(rate, whole.saturating_mul(SECOND_MS));
        let owed = moved.saturating_add(floor.saturating_sub(self.moved));
        self.start = self.start + whole.saturating_mul(SECOND_MS);
        self.moved = 0;
        owed
    }

    /// End it at `until`, part of the way through, at a reserved `rate`.
    /// Returns what the floor for that part came to beyond what moved in it.
    pub fn close(self, until: Millis, rate: u64) -> u64 {
        let floor = units_in(rate, until.saturating_since(self.start));
        floor.saturating_sub(self.moved)
    }
}
