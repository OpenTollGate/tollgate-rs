//! What a payer has bought and what has been drawn from it.
//!
//! Two layers, and keeping them apart is the whole of it:
//!
//! - **Per channel**, a ratchet: the cumulative total signed on that channel.
//!   It only means anything against that channel, so each one is tracked
//!   separately and a total from one is meaningless against another.
//! - **Per payer**, the budget: how much has been authorized in total, how
//!   much has been drawn, when what is left expires, and the rate reserved.
//!
//! ```text
//! authorized     cumulative units the payer may draw, ever
//! consumed       cumulative units drawn against them
//! deadline       when what is left stops being spendable
//! reserved_rate  units per second drawn while the payer is carried, used or not
//! last_topup     when the payer's last TopUp had its signatures checked
//! ```
//!
//! `authorized − consumed` is the payer's **budget**. A purchase adds to it;
//! nothing is forfeit. The budget belongs to the payer rather than to a session
//! or a channel, so it outlives both ([`GrantState::restart`],
//! [`GrantState::restore`]); the reservation lasts only for the session.

use alloc::vec::Vec;

use tollgate_protocol::ChannelId;

use crate::config::BurstPolicy;
use crate::grant::limits;
use crate::time::Millis;

/// Consecutive purchases on one channel that may fail verification before we
/// stop honoring it.
///
/// One failure could be transient — a reordered message, a payer that lost
/// track of its own total — so it earns a Reject and nothing more. A channel
/// that keeps failing is either broken or being probed, and each attempt costs
/// us a signature verification, so past this many in a row it is closed and
/// settled at the last state that did verify.
pub const MAX_VERIFICATION_FAILURES: u32 = 3;

/// One channel a peer pays us on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IncomingChannel {
    /// The channel.
    pub id: ChannelId,
    /// Units it can carry in total.
    pub capacity: u64,
    /// Cumulative total signed on it. Monotonic — this is the ratchet.
    pub signed: u64,
    /// Purchases on it that failed verification since the last one that
    /// passed. See [`MAX_VERIFICATION_FAILURES`].
    pub failures: u32,
    /// When the peer can reclaim it through the refund path, or `None` if it
    /// never expires. Everything earned on it that has not been settled by
    /// then goes back to the peer.
    pub expires_at: Option<Millis>,
}

impl IncomingChannel {
    /// A freshly verified channel, nothing signed on it yet.
    pub fn new(id: ChannelId, capacity: u64, expires_at: Option<Millis>) -> Self {
        Self {
            id,
            capacity,
            signed: 0,
            failures: 0,
            expires_at,
        }
    }

    /// Units that can still be signed onto it.
    pub fn headroom(&self) -> u64 {
        self.capacity.saturating_sub(self.signed)
    }

    /// Whether it has been drained to its capacity.
    pub fn exhausted(&self) -> bool {
        self.signed >= self.capacity
    }
}

/// A payer's budget as it is kept between sessions: what is left, and when it
/// expires. The host writes it to disk and hands it back when the payer returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Budget {
    /// Units left.
    pub remaining: u64,
    /// When they expire, on the host's clock.
    pub deadline: Millis,
}

impl Budget {
    /// Nothing left.
    pub const NONE: Self = Self {
        remaining: 0,
        deadline: Millis::ZERO,
    };

    /// What is left at `now`: nothing once the deadline has passed.
    pub fn at(self, now: Millis) -> Self {
        if self.remaining == 0 || now >= self.deadline {
            Self::NONE
        } else {
            self
        }
    }

    /// Milliseconds to the deadline at `now`, or `0` if there is no budget.
    pub fn expires_in_ms(self, now: Millis) -> u64 {
        let live = self.at(now);
        if live.remaining == 0 {
            0
        } else {
            live.deadline.saturating_since(now)
        }
    }
}

/// The provider's view of one payer.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GrantState {
    /// Channels this peer currently pays us on. Only these are recognised: an
    /// update naming anything else is refused, which is what keeps this list
    /// from growing with every rollover a long session performs.
    channels: Vec<IncomingChannel>,
    /// Cumulative units the payer may draw, ever. At a session start this is
    /// the budget it brought with it.
    authorized: u64,
    /// Cumulative units drawn against them. Never exceeds `authorized`.
    consumed: u64,
    /// When what is left stops being spendable.
    deadline: Millis,
    /// Units per second drawn while the payer is carried, used or not. Set by
    /// each TopUp, ended with the session, at the deadline, or when the budget
    /// reaches zero.
    reserved_rate: u64,
    /// When the payer's last TopUp had its signatures checked, whatever came
    /// of it. A TopUp refused as too soon does not move it.
    last_topup: Option<Millis>,
}

impl GrantState {
    /// Nothing bought and no channel.
    pub const fn new() -> Self {
        Self {
            channels: Vec::new(),
            authorized: 0,
            consumed: 0,
            deadline: Millis::ZERO,
            reserved_rate: 0,
            last_topup: None,
        }
    }

    /// Start a new session over the channels and budget kept from the last
    /// one.
    ///
    /// The budget carries over until its deadline, as it does into any new
    /// session: `authorized` becomes what was left and `consumed` zero. The
    /// reservation does not — a payer that comes back reserves again with its
    /// next TopUp — and nor does the time of its last one. The channels and the
    /// totals signed on them stay, so the payer carries on ratcheting where it
    /// left off instead of funding again.
    ///
    /// Verification failures are not carried over: they count failures in a
    /// row on one connection, and a payer that lost track of its total is
    /// exactly the one that reconnects and starts clean.
    pub fn restart(&mut self, now: Millis) {
        let mut channels = core::mem::take(&mut self.channels);
        for channel in &mut channels {
            channel.failures = 0;
        }
        let budget = self.budget(now);
        *self = Self {
            channels,
            ..Self::new()
        };
        self.restore(budget, now);
    }

    /// Take up a budget the payer left behind in an earlier session, as the
    /// host kept it. One past its deadline is nothing.
    pub fn restore(&mut self, budget: Budget, now: Millis) {
        let budget = budget.at(now);
        self.authorized = budget.remaining;
        self.consumed = 0;
        self.deadline = budget.deadline;
        self.reserved_rate = 0;
    }

    /// The budget as it stands at `now`, as the host keeps it between
    /// sessions.
    pub fn budget(&self, now: Millis) -> Budget {
        Budget {
            remaining: self.remaining(),
            deadline: self.deadline,
        }
        .at(now)
    }

    /// End the reservation, as the session ending does. The budget stays.
    pub fn end_reservation(&mut self) {
        self.reserved_rate = 0;
    }

    /// Recognise a channel the peer has funded and we have verified.
    ///
    /// Re-verifying one we already hold refreshes its capacity and expiry
    /// without disturbing the ratchet, so a repeated ChannelReady is harmless.
    pub fn open_channel(&mut self, id: ChannelId, capacity: u64, expires_at: Option<Millis>) {
        if let Some(existing) = self.channels.iter_mut().find(|c| c.id == id) {
            existing.capacity = capacity;
            existing.expires_at = expires_at;
            return;
        }
        self.channels
            .push(IncomingChannel::new(id, capacity, expires_at));
    }

    /// Stop recognising a channel — it has been settled, or the peer replaced
    /// it. Updates naming it are refused from here on. The budget it paid for
    /// is not touched: settling collects the money that bought it.
    ///
    /// Returns the channel as it stood, or `None` if we did not recognise it.
    pub fn close_channel(&mut self, id: ChannelId) -> Option<IncomingChannel> {
        let at = self.channels.iter().position(|c| c.id == id)?;
        Some(self.channels.remove(at))
    }

    /// Count a purchase on this channel that failed verification — a bad
    /// signature, or a total that did not increase.
    ///
    /// Returns the failures now in a row, or `None` if we do not recognise the
    /// channel, in which case there is nothing to count against.
    pub fn record_failure(&mut self, id: ChannelId) -> Option<u32> {
        let channel = self.channels.iter_mut().find(|c| c.id == id)?;
        channel.failures = channel.failures.saturating_add(1);
        Some(channel.failures)
    }

    /// Channels this peer currently pays us on.
    pub fn channels(&self) -> &[IncomingChannel] {
        &self.channels
    }

    /// A channel by id, if we recognise it.
    pub fn channel(&self, id: ChannelId) -> Option<IncomingChannel> {
        self.channels.iter().copied().find(|c| c.id == id)
    }

    /// Channels drained to their capacity, which can be settled.
    pub fn exhausted_channels(&self) -> impl Iterator<Item = ChannelId> + '_ {
        self.channels.iter().filter(|c| c.exhausted()).map(|c| c.id)
    }

    /// Channels within `lead_ms` of their expiry, which have to be settled now.
    ///
    /// Settling is the receiver's job alone, and past expiry the funder can
    /// reclaim the whole channel through the refund path — including what it
    /// already paid us on it. See
    /// [`NodePolicy::settle_lead_ms`](crate::config::NodePolicy::settle_lead_ms).
    pub fn expiring_channels(
        &self,
        now: Millis,
        lead_ms: u64,
    ) -> impl Iterator<Item = ChannelId> + '_ {
        self.channels
            .iter()
            .filter(move |c| c.expires_at.is_some_and(|e| now + lead_ms >= e))
            .map(|c| c.id)
    }

    /// Cumulative units the payer may draw.
    pub fn authorized(&self) -> u64 {
        self.authorized
    }

    /// Cumulative units drawn.
    pub fn consumed(&self) -> u64 {
        self.consumed
    }

    /// Units the payer may still spend. Never negative by construction.
    pub fn remaining(&self) -> u64 {
        self.authorized.saturating_sub(self.consumed)
    }

    /// The rate reserved now.
    pub fn reserved_rate(&self) -> u64 {
        self.reserved_rate
    }

    /// When what is left expires.
    pub fn deadline(&self) -> Millis {
        self.deadline
    }

    /// When the payer's last TopUp had its signatures checked.
    pub fn last_topup(&self) -> Option<Millis> {
        self.last_topup
    }

    /// Whether a TopUp arriving at `now` comes sooner after the last than
    /// `gap_ms` allows.
    pub fn too_soon(&self, now: Millis, gap_ms: u64) -> bool {
        self.last_topup
            .is_some_and(|last| now.saturating_since(last) < gap_ms)
    }

    /// Note that a TopUp had its signatures checked at `now`, whatever came of
    /// it after that.
    pub fn topup_checked(&mut self, now: Millis) {
        self.last_topup = Some(now);
    }

    /// Apply a validated purchase.
    ///
    /// `ratchets` is each `(channel, new cumulative)` the purchase turns, and
    /// `grant` their combined increase — both computed by
    /// [`evaluate_topup`](super::core::evaluate_topup), which is also where the
    /// validation lives.
    ///
    /// **A grant adds to the budget; nothing is forfeit.** A payer renewing
    /// early pays for each second once. The deadline becomes the later of the
    /// old one and now plus the window, so a purchase never brings it closer,
    /// and the reserved rate replaces the one before it, up or down.
    pub fn apply(
        &mut self,
        ratchets: &[(ChannelId, u64)],
        grant: u64,
        window_ms: u64,
        reserved_rate: u64,
        now: Millis,
    ) {
        for (id, cumulative) in ratchets {
            if let Some(channel) = self.channels.iter_mut().find(|c| c.id == *id) {
                channel.signed = *cumulative;
                // Only failures *in a row* count against a channel.
                channel.failures = 0;
            }
        }

        // A budget whose deadline has passed is gone, even if the tick that
        // would have expired it has not come round yet.
        self.expire_if_due(now);
        self.authorized = self.authorized.saturating_add(grant);
        self.deadline = self.deadline.max(now + window_ms);
        self.reserved_rate = reserved_rate;
    }

    /// Draw one tick's worth from the budget, by the one rule:
    ///
    /// ```text
    /// drawn = max(moved, reserved_rate × tick)
    /// ```
    ///
    /// `moved` is already weighted by the from-payer weight, and `tick_ms` is
    /// how long the payer was carried since the last draw. A payer that
    /// overran its budget between two readings has `consumed` held at
    /// `authorized`: the overrun is not carried as a debt. When the budget
    /// reaches zero the reservation ends with it.
    ///
    /// Returns the units drawn.
    pub fn draw(&mut self, moved: u64, tick_ms: u64) -> u64 {
        let reserved = limits::units_in(self.reserved_rate, tick_ms);
        let drawn = moved.max(reserved).min(self.remaining());
        self.consumed = self.consumed.saturating_add(drawn);
        if self.remaining() == 0 {
            self.reserved_rate = 0;
        }
        drawn
    }

    /// Expire the budget if its deadline has passed.
    ///
    /// What is left is forfeit and the payment is kept, and the reservation
    /// ends. Returns whether anything was forfeit, which is worth logging but
    /// changes no decision.
    pub fn expire_if_due(&mut self, now: Millis) -> bool {
        if now < self.deadline {
            return false;
        }
        self.reserved_rate = 0;
        if self.consumed < self.authorized {
            self.consumed = self.authorized;
            return true;
        }
        false
    }

    /// Whether the payer has budget to spend right now.
    pub fn is_live(&self, now: Millis) -> bool {
        now < self.deadline && self.remaining() > 0
    }

    /// The rate to shape this payer at:
    ///
    /// ```text
    /// speed = the burst policy's choice, at least reserved_rate
    /// speed = min(speed, remaining / tick)      // near zero
    /// rate  = max(speed, minimum flow allowance)
    /// ```
    ///
    /// The minimum flow allowance is the **floor**, not a separate mechanism:
    /// a payer whose budget has run out or expired falls back to it rather
    /// than to silence, which is what leaves it able to send the TopUp that
    /// buys more.
    pub fn shaping_rate(
        &self,
        now: Millis,
        burst: BurstPolicy,
        minimum_flow: u64,
        tick_ms: u64,
    ) -> u64 {
        if !self.is_live(now) {
            return minimum_flow;
        }
        let speed = if self.reserved_rate > 0 {
            self.reserved_rate.max(burst.rate)
        } else {
            burst.unreserved_rate
        };
        let speed = speed.min(limits::per_tick_ceiling(self.remaining(), tick_ms));
        speed.max(minimum_flow)
    }
}
