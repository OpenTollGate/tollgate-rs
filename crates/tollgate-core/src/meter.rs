//! Cumulative counters, per peer, link-local.
//!
//! Counts are **not exchanged, not signed, and not an input to any payment**.
//! The payer bought its budget in advance; these are only how the provider
//! knows how much of it to draw, and how the payer keeps its own count of the
//! same budget.
//!
//! The directions are named by the roles in a sale, from the side of the peer
//! as payer: `to_payer` is what this node sent it, `from_payer` what it sent
//! this node. In peering each node is also the other's payer, and then the same
//! two counts are read the other way round ([`Counters::swapped`]).
//!
//! The counters stay **raw**. The from-payer weight is applied when the budget
//! is drawn ([`Counters::weighted`]), not when counting, which keeps what the
//! meter reports separable from what the shaper charges.

/// Cumulative units across a link with one peer, since session start.
///
/// Cumulative rather than deltas because a running total survives a lost
/// reading.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Counters {
    /// `units_to_payer`: units this node sent to the peer — its download.
    pub to_payer: u64,
    /// `units_from_payer`: units this node got from the peer — its upload.
    pub from_payer: u64,
}

impl Counters {
    /// Counters at session start.
    pub const ZERO: Self = Self {
        to_payer: 0,
        from_payer: 0,
    };

    /// What grew since `earlier`.
    ///
    /// Saturating, so a counter that resets (an enforcer restart, a
    /// re-registered peer) reports no growth for one sample instead of
    /// underflowing into an enormous one.
    pub fn delta_since(self, earlier: Self) -> Self {
        Self {
            to_payer: self.to_payer.saturating_sub(earlier.to_payer),
            from_payer: self.from_payer.saturating_sub(earlier.from_payer),
        }
    }

    /// The same counts with this node as the payer: what it got from the peer
    /// is then what went to the payer.
    pub fn swapped(self) -> Self {
        Self {
            to_payer: self.from_payer,
            from_payer: self.to_payer,
        }
    }

    /// Units moved, as the budget is drawn by them:
    ///
    /// ```text
    /// moved = to_payer + from_payer × from_payer_weight
    /// ```
    ///
    /// A unit to the payer draws one. A unit from it draws the weight — `1`
    /// the same, `10` ten times as much, `0` nothing.
    ///
    /// Computed in `u128` and saturated: the counts are what the payer moved,
    /// and wrapping here would hand it free capacity.
    pub fn weighted(self, from_payer_weight: u16) -> u64 {
        let moved = self.to_payer as u128
            + (self.from_payer as u128).saturating_mul(from_payer_weight as u128);
        moved.min(u64::MAX as u128) as u64
    }
}

/// Tracks a peer's counters and turns each reading into what grew since the
/// last.
///
/// The host reports cumulative totals whenever it likes; this holds the last
/// reading so core can work in deltas without the host having to.
#[derive(Debug, Clone, Copy, Default)]
pub struct Meter {
    last: Counters,
}

impl Meter {
    /// A meter at session start.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a new cumulative reading and return what grew since the last.
    pub fn observe(&mut self, now: Counters) -> Counters {
        let delta = now.delta_since(self.last);
        self.last = now;
        delta
    }

    /// The most recent reading.
    pub fn totals(&self) -> Counters {
        self.last
    }
}
