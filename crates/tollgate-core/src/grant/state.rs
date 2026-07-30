//! What a peer has bought and what it has drawn down.
//!
//! Three numbers, zeroed when the session starts, and none of them exchanged:
//! the payer knows what it signed, the provider knows what it delivered, and
//! neither has to tell the other, because the money moved before the traffic
//! did.
//!
//! ```text
//! authorized   cumulative units the payer has signed for, ever
//! consumed     cumulative units drawn against them
//! deadline     when the current grant stops being spendable
//! ```

use crate::grant::limits;
use crate::time::Millis;

/// The provider's view of one peer's payment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct GrantState {
    /// Cumulative units the payer has signed for. Monotonic — this is the
    /// Spilman ratchet, and it is what makes a TopUp idempotent.
    authorized: u64,
    /// Cumulative units drawn against them. Never exceeds `authorized`.
    consumed: u64,
    /// When the grant in force stops being spendable.
    deadline: Millis,
    /// Units per second, fixed for the life of the grant at `grant / window`.
    /// Capacity left unused early is not banked for later, or a payer that
    /// waited would be owed an unbounded burst just before the deadline.
    rate: u64,
    /// Whether any grant has ever been applied.
    started: bool,
}

impl GrantState {
    /// The zeroed state both sides adopt when a session starts.
    pub const fn new() -> Self {
        Self {
            authorized: 0,
            consumed: 0,
            deadline: Millis::ZERO,
            rate: 0,
            started: false,
        }
    }

    /// Cumulative units signed for.
    pub fn authorized(&self) -> u64 {
        self.authorized
    }

    /// Cumulative units drawn.
    pub fn consumed(&self) -> u64 {
        self.consumed
    }

    /// Units the peer may still spend. Never negative by construction.
    pub fn remaining(&self) -> u64 {
        self.authorized.saturating_sub(self.consumed)
    }

    /// The rate the grant in force bought.
    pub fn rate(&self) -> u64 {
        self.rate
    }

    /// When the grant in force expires.
    pub fn deadline(&self) -> Millis {
        self.deadline
    }

    /// Whether any grant has been applied yet.
    pub fn started(&self) -> bool {
        self.started
    }

    /// Apply a validated TopUp, replacing the grant in force.
    ///
    /// **A grant replaces the previous one, it does not add to it.** Buying
    /// again before the old window runs out forfeits whatever was left of it —
    /// that is the payer's risk, and it is what makes the product bandwidth
    /// rather than a stored quantity of units. Without forfeiture a buyer could
    /// accumulate claims off-peak and present them all at peak, which is
    /// selling volume, not bandwidth.
    ///
    /// Returns the size of this purchase alone.
    ///
    /// Caller must have validated the update first — see
    /// [`evaluate_topup`](super::core::evaluate_topup).
    pub fn apply(&mut self, cumulative: u64, window_ms: u32, now: Millis) -> u64 {
        let grant = cumulative.saturating_sub(self.authorized);

        // The old grant's remainder burns at this moment, which is exactly what
        // setting `consumed` to the old `authorized` expresses.
        self.consumed = self.authorized;
        self.authorized = cumulative;
        self.deadline = now + window_ms as u64;
        self.rate = limits::rate_from(grant, window_ms);
        self.started = true;

        grant
    }

    /// Draw units against the grant, clamped so `consumed` never passes
    /// `authorized`.
    pub fn draw(&mut self, units: u64) {
        self.consumed = self.consumed.saturating_add(units).min(self.authorized);
    }

    /// Expire the grant if its deadline has passed.
    ///
    /// Unspent capacity is forfeit and the payment is kept — a second of
    /// capacity that went unsold is gone for the provider too. Returns whether
    /// anything was forfeited, which is worth logging but changes no decision.
    pub fn expire_if_due(&mut self, now: Millis) -> bool {
        if self.started && now >= self.deadline && self.consumed < self.authorized {
            self.consumed = self.authorized;
            return true;
        }
        false
    }

    /// Whether there is live capacity right now.
    pub fn is_live(&self, now: Millis) -> bool {
        self.started && now < self.deadline && self.remaining() > 0
    }

    /// The rate to shape this peer at.
    ///
    /// The minimum flow allowance is the **floor**, not a separate mechanism:
    /// a peer whose grant has expired or run out falls back to it rather than
    /// to silence, which is what leaves it able to send the TopUp that buys the
    /// next grant.
    pub fn shaping_rate(&self, now: Millis, minimum_flow: u64) -> u64 {
        if self.is_live(now) {
            self.rate.max(minimum_flow)
        } else {
            minimum_flow
        }
    }
}
