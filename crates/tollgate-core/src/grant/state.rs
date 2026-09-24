//! What a peer has bought and what it has drawn down.
//!
//! Two layers, and keeping them apart is the whole of it:
//!
//! - **Per channel**, a ratchet: the cumulative total signed on that channel.
//!   It only means anything against that channel, so each one is tracked
//!   separately and a total from one is meaningless against another.
//! - **Per peer**, the grant: how much has been authorized in total, how much
//!   has been drawn, and when the window closes.
//!
//! A purchase may ratchet several channels at once, and the grant it buys is
//! the **combined increase** across them. That is what lets one purchase span a
//! channel that is filling up and its replacement, and what lets a payer spend
//! from several accepted mints at once.
//!
//! ```text
//! authorized   cumulative units the payer has bought, ever, across all channels
//! consumed     cumulative units drawn against them
//! deadline     when the grant in force stops being spendable
//! ```
//!
//! Nothing here is exchanged: the payer knows what it signed, the provider
//! knows what it delivered, and neither has to tell the other, because the
//! money moved before the traffic did.

use alloc::vec::Vec;

use tollgate_protocol::ChannelId;

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

/// The provider's view of one peer's payment.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GrantState {
    /// Channels this peer currently pays us on. Only these are recognised: an
    /// update naming anything else is refused, which is what keeps this list
    /// from growing with every rollover a long session performs.
    channels: Vec<IncomingChannel>,
    /// Cumulative units bought, ever, across every channel.
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
            channels: Vec::new(),
            authorized: 0,
            consumed: 0,
            deadline: Millis::ZERO,
            rate: 0,
            started: false,
        }
    }

    /// Start a new session over the channels kept from the last one.
    ///
    /// The grant is zeroed exactly as it is for a session that starts from
    /// nothing — a new connection is a new session — but the channels and the
    /// totals signed on them stay, so the payer carries on ratcheting where it
    /// left off instead of funding again.
    ///
    /// Verification failures are not carried over: they count failures in a
    /// row on one connection, and a payer that lost track of its total is
    /// exactly the one that reconnects and starts clean.
    pub fn restart(&mut self) {
        let mut channels = core::mem::take(&mut self.channels);
        for channel in &mut channels {
            channel.failures = 0;
        }
        *self = Self {
            channels,
            ..Self::new()
        };
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
    /// it. Updates naming it are refused from here on.
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

    /// Cumulative units bought across every channel.
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

    /// Apply a validated purchase, replacing the grant in force.
    ///
    /// `ratchets` is each `(channel, new cumulative)` the purchase turns, and
    /// `grant` their combined increase — both computed by
    /// [`evaluate_topup`](super::core::evaluate_topup), which is also where the
    /// validation lives.
    ///
    /// **A grant replaces the previous one, it does not add to it.** Buying
    /// again before the old window runs out forfeits whatever was left of it —
    /// that is the payer's risk, and it is what makes the product bandwidth
    /// rather than a stored quantity of units. Without forfeiture a buyer could
    /// accumulate claims off-peak and present them all at peak, which is
    /// selling volume, not bandwidth.
    pub fn apply(
        &mut self,
        ratchets: &[(ChannelId, u64)],
        grant: u64,
        window_ms: u32,
        now: Millis,
    ) {
        for (id, cumulative) in ratchets {
            if let Some(channel) = self.channels.iter_mut().find(|c| c.id == *id) {
                channel.signed = *cumulative;
                // Only failures *in a row* count against a channel.
                channel.failures = 0;
            }
        }

        // The old grant's remainder burns at this moment, which is exactly what
        // setting `consumed` to the old `authorized` expresses.
        self.consumed = self.authorized;
        self.authorized = self.authorized.saturating_add(grant);
        self.deadline = now + window_ms as u64;
        self.rate = limits::rate_from(grant, window_ms);
        self.started = true;
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
