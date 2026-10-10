//! What this node has bought from one peer, and the policy it buys under.
//!
//! The Spilman ratchet is **per channel** — a cumulative total only means
//! anything against the channel it was signed on — so the payer tracks up to
//! three at once:
//!
//! ```text
//! active    being drained now
//! next      funded and confirmed, waiting for `active` to fill
//! pending   funded by us, not yet confirmed by the peer
//! ```
//!
//! Before `pending` there is a fourth, shorter stage with no channel yet: the
//! host has been asked to fund one and has not answered. Funding is a mint
//! round trip, far longer than a tick, so it is marked the moment it is asked
//! for — see [`Buyer::funding_requested`]. Each request carries an id, so an
//! answer that comes after its request was given up on and asked again can be
//! told apart from the answer to the one asked since — see [`Buyer::answers`].
//!
//! A channel is opened well before it is needed (default: at 80% of the one in
//! use) precisely so that `next` is ready by the time a purchase overflows
//! `active`, and the overflow can be signed across both rather than stalling.
//!
//! A channel also has an expiry, after which we can reclaim it through the
//! refund path. A slowly drawn channel reaches that long before it fills, so
//! it is replaced when it enters the safety margin before expiry as well, and
//! abandoned for its replacement rather than drained.
//!
//! What was bought is kept apart from the channels, in the buyer's own count
//! of its **budget** with the provider: what it signed for, less what it
//! measured crossing the link, drawn by the same rule the provider uses. The
//! budget is the payer's, not a channel's, so it outlives channels and
//! sessions.

use tollgate_protocol::{ChannelId, ReasonCode};

use crate::grant::Second;
use crate::time::Millis;

/// How this node buys. One workable policy rather than the policy: the design
/// leaves choosing a budget to the payer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuyerPolicy {
    /// Units per second to want from every peer whether or not anything asks:
    /// a standing order, which spends money. `0` buys only for what is
    /// observed.
    pub demand: u64,
    /// Reserve a rate that follows demand — time at a speed. `false` reserves
    /// nothing, or the provider's smallest reserved rate, and pays for what it
    /// uses.
    pub reserve: bool,
    /// Reserve this percentage of observed demand. Above 100 leaves headroom so
    /// a rising flow is not held back before the next purchase lands, and is
    /// what keeps the buyer from buying again for every small rise.
    pub headroom_pct: u32,
    /// Never reserve below this, units per second. Raised to the provider's
    /// smallest reserved rate.
    pub min_rate: u64,
    /// Never reserve above this. The operator's spending ceiling — vouchers
    /// cost money to acquire, whatever the protocol thinks.
    pub max_rate: u64,
    /// The highest from-payer weight this node buys at. A provider offering
    /// more is refused before any money moves. `None` takes any.
    pub max_from_payer_weight: Option<u16>,
    /// Window to ask for, clamped to the provider's range. With
    /// [`Self::reserve`] it also sets the budget: the reserved rate times the
    /// window.
    pub window_ms: u64,
    /// Without [`Self::reserve`], the units to hold with each provider.
    pub budget: u64,
    /// Buy again this long before the budget, at the rate it is drawn, or the
    /// deadline would run out. See [`BuyerPolicy::MIN_SAFE_LEAD_MS`].
    pub renew_lead_ms: u64,
    /// How long to keep to a reserved rate a provider named in a TopUpReject
    /// before trying higher again.
    pub cap_hold_ms: u64,
}

impl BuyerPolicy {
    /// A renewal lead below this is asking for a lapsed budget.
    ///
    /// The lead is how late a purchase may be and still land before the budget
    /// runs out — an absolute tolerance, in milliseconds, against everything
    /// between deciding to buy and the provider applying the result: a
    /// signature, a round trip, the provider's tick, a scheduler that had
    /// something else to do. A node under load misses a few hundred
    /// milliseconds without much trying.
    ///
    /// Missing it is expensive out of all proportion to the gap. The budget
    /// runs out, the shaper drops to the minimum flow allowance with a
    /// window's worth of packets in flight, and a TCP flow crossing that spends
    /// seconds in backoff recovering from a gap of a tenth of a second.
    /// Measured, in `testing/forwarding`: a 300 ms lead flaked under load;
    /// 1.2 s held.
    pub const MIN_SAFE_LEAD_MS: u64 = 1_000;

    /// The lead this policy can actually use inside a window of `window_ms`.
    ///
    /// The provider bounds the window, and a short bound can leave a
    /// configured lead longer than the window itself — taken literally, every
    /// purchase would be inside its own lead the moment it is made. Half the
    /// window is the ceiling.
    pub fn lead_within(&self, window_ms: u64) -> u64 {
        self.renew_lead_ms.min(window_ms / 2)
    }

    /// Whether the lead is too short to absorb ordinary scheduling jitter.
    pub fn lead_is_thin(&self) -> bool {
        self.renew_lead_ms < Self::MIN_SAFE_LEAD_MS
    }

    /// Whether this policy buys anything at all: a reserving buyer whose
    /// ceiling is zero does not, nor a pay-per-use one with no budget to hold.
    pub fn buying(&self) -> bool {
        if self.reserve {
            self.max_rate > 0
        } else {
            self.budget > 0
        }
    }

    /// Whether this policy buys at a provider's from-payer weight.
    pub fn accepts_weight(&self, weight: u16) -> bool {
        self.max_from_payer_weight.is_none_or(|max| weight <= max)
    }
}

impl Default for BuyerPolicy {
    /// Time at a speed that follows demand, a ten-second budget, and a lead
    /// that carries a real TCP flow.
    fn default() -> Self {
        Self {
            demand: 0,
            reserve: true,
            headroom_pct: 125,
            min_rate: 0,
            max_rate: u64::MAX,
            max_from_payer_weight: None,
            window_ms: 10_000,
            budget: 0,
            renew_lead_ms: 1_200,
            cap_hold_ms: 10_000,
        }
    }
}

/// The terms a provider advertised in its Offer, which every purchase from it
/// has to fit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Terms {
    /// Shortest window it accepts.
    pub min_window_ms: u64,
    /// Longest window it accepts.
    pub max_window_ms: u64,
    /// Smallest reserved rate it accepts.
    pub min_reserved_rate: u64,
    /// Shortest time it accepts between two TopUps.
    pub min_topup_gap_ms: u64,
    /// What a unit we send it draws from our budget. Fixed for the session.
    pub from_payer_weight: u16,
}

impl Terms {
    /// Fit a preferred window into what the provider will take.
    pub fn clamp_window(&self, want_ms: u64) -> u64 {
        want_ms.clamp(
            self.min_window_ms,
            self.max_window_ms.max(self.min_window_ms),
        )
    }
}

/// One channel, from the payer's side.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChannelBuyer {
    /// The channel.
    pub id: ChannelId,
    /// Units it can carry in total.
    pub capacity: u64,
    /// Cumulative units signed **on this channel**. Monotonic, and meaningless
    /// against any other channel.
    pub cumulative: u64,
    /// When we can reclaim it, or `None` if it never expires.
    pub expires_at: Option<Millis>,
}

impl ChannelBuyer {
    /// A freshly funded channel, nothing signed on it yet.
    pub fn new(id: ChannelId, capacity: u64, expires_at: Option<Millis>) -> Self {
        Self {
            id,
            capacity,
            cumulative: 0,
            expires_at,
        }
    }

    /// Whether `now` is within `margin_ms` of the channel's expiry.
    pub fn expiring(&self, now: Millis, margin_ms: u64) -> bool {
        self.expires_at
            .is_some_and(|expiry| now + margin_ms >= expiry)
    }

    /// Units that can still be signed onto this channel.
    pub fn headroom(&self) -> u64 {
        self.capacity.saturating_sub(self.cumulative)
    }

    /// Whether the channel has been drained to its capacity.
    pub fn exhausted(&self) -> bool {
        self.cumulative >= self.capacity
    }
}

/// One channel update a purchase requires.
///
/// A purchase that overflows the channel in use takes two, because a cumulative
/// total is only meaningful against the channel it was signed on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Leg {
    /// The channel to ratchet.
    pub channel_id: ChannelId,
    /// New cumulative total on that channel.
    pub cumulative: u64,
}

/// A purchase decided but not yet sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Purchase {
    /// The update on the channel currently in use.
    pub first: Leg,
    /// The update on the replacement channel, when the purchase overflowed the
    /// first. Present only when a rollover is already funded and confirmed.
    pub second: Option<Leg>,
    /// How long the provider is to keep the budget, already clamped to its
    /// range.
    pub window_ms: u64,
    /// The rate to reserve from now on.
    pub reserved_rate: u64,
    /// Units bought by this purchase alone, across both legs: what has drained
    /// since the last, so the budget is back to the size wanted.
    pub grant: u64,
    /// Why the buyer acted, for the operator's benefit.
    pub trigger: Trigger,
}

/// How long a funding request may go unanswered before a rollover is tried
/// again.
///
/// The host answers every request, with the channel or with a failure, so this
/// only matters when an answer never comes — a funding call that hangs. It is
/// long next to a mint round trip, so a slow funding is not doubled, and short
/// next to what a rollover has left: one starts with at least two purchases'
/// worth of headroom, or a whole safety margin, of a minute or more, before
/// expiry.
pub const FUNDING_TIMEOUT_MS: u64 = 30_000;

/// The funding requests still open for one peer: every one asked for since a
/// channel last came back, of which the latest is the one being waited on.
///
/// The ids come from one counter for the whole node, so they only ever grow,
/// and a request asked for later always has the larger one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct FundingRequests {
    /// The earliest request still open. Every request this buyer made from
    /// here to `latest` is still open: none has been answered with a channel.
    /// An id in between may have gone to another peer, but its answer names
    /// that peer, so it never reaches this buyer.
    first: u64,
    /// The one asked for last.
    latest: u64,
    /// When `latest` was asked for, while it is still being waited on. `None`
    /// once the host said it failed.
    since: Option<Millis>,
}

/// Why a channel is being replaced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RolloverReason {
    /// It is filling up. The replacement grows: a peer that fills one channel
    /// is likely to fill the next.
    Capacity,
    /// It entered the safety margin before its expiry without filling. The
    /// replacement stays the same size, since growing a channel that is not
    /// being used up would only lock more away.
    Expiry,
}

/// What prompted a purchase.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trigger {
    /// Nothing has been bought yet in this session.
    First,
    /// The budget or its deadline is about to run out.
    Renewal,
    /// Demand outgrew the rate reserved, so the reservation is raised at once.
    RateRose,
    /// The provider refused the last purchase and named the reserved rate it
    /// would take.
    Rebuy,
}

/// The buyer's state before a purchase, kept so a refusal can be undone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Prior {
    pub(super) active: Option<ChannelBuyer>,
    pub(super) next: Option<ChannelBuyer>,
    pub(super) deadline: Millis,
    pub(super) reserved: u64,
    pub(super) started: bool,
    pub(super) resumed: u64,
    pub(super) grant: u64,
}

/// The payer's side of one peering.
#[derive(Debug, Clone, Copy, Default)]
pub struct Buyer {
    /// The channel being drained.
    pub(super) active: Option<ChannelBuyer>,
    /// A confirmed replacement, waiting for `active` to fill.
    pub(super) next: Option<ChannelBuyer>,
    /// Funded by us, not yet confirmed by the peer. Nothing is signed on it —
    /// the peer has not said it verified the funding, so a grant signed here
    /// might be against a channel that never opens.
    pub(super) pending: Option<ChannelBuyer>,
    /// The requests we have made to the host to fund a channel for this peer
    /// and have had no channel back for. No channel exists yet, so there is
    /// nothing to put in `pending`, but a channel is already on the way.
    pub(super) funding: Option<FundingRequests>,
    /// Our own count of the budget we hold with the provider: what we signed
    /// for, less what we measured it draw.
    pub(super) remaining: u64,
    /// When that budget expires.
    pub(super) deadline: Millis,
    /// The rate we reserved with our last purchase. It ends with the session,
    /// at the deadline, and when the budget runs out, as it does at the
    /// provider.
    pub(super) reserved: u64,
    /// The second our own count is being drawn over, by the provider's rule.
    pub(super) second: Option<Second>,
    /// When we last sent a TopUp, so the next waits out the provider's gap.
    pub(super) last_topup: Option<Millis>,
    /// Whether anything has been bought in this session.
    pub(super) started: bool,
    /// The rate we had reserved when the last session ended, until we buy in
    /// this one.
    ///
    /// The provider ends a reservation with the session and carries a payer
    /// that comes back with a budget as one that reserved nothing, until its
    /// next TopUp. Our count may say that budget lasts a while yet, so a
    /// buyer of time at a speed buys at once to reserve again, rather than
    /// when the budget runs low.
    pub(super) resumed: u64,
    /// The reserved rate the provider last told us it would accept, from a
    /// TopUpReject, and until when we keep to it.
    ///
    /// Held for a while rather than forgotten on the next purchase: capacity
    /// may free up, but probing above it every gap means a buyer parked above
    /// the cap is refused once a gap forever.
    pub(super) capped_at: Option<(u64, Millis)>,
    /// Units bought by the most recent purchase, so a rollover can be started
    /// before a channel is too small to carry another one.
    pub(super) last_grant: u64,
    /// State as it stood before the most recent purchase.
    ///
    /// A TopUp is fire-and-forget, so we assume it landed and advance. If a
    /// TopUpReject comes back, the provider never turned its ratchet — and if
    /// we kept our own advanced, every later purchase would be computed from a
    /// total the provider does not recognise, and refused for the same reason
    /// as the first. So we keep exactly one step of history to undo.
    pub(super) prior: Option<Prior>,
}

impl Buyer {
    /// A buyer with no channel yet.
    pub const fn new() -> Self {
        Self {
            active: None,
            next: None,
            pending: None,
            funding: None,
            remaining: 0,
            deadline: Millis::ZERO,
            reserved: 0,
            second: None,
            last_topup: None,
            started: false,
            resumed: 0,
            capped_at: None,
            last_grant: 0,
            prior: None,
        }
    }

    /// The channel being drained.
    pub fn active(&self) -> Option<ChannelBuyer> {
        self.active
    }

    /// The confirmed replacement, if one is ready.
    pub fn next_channel(&self) -> Option<ChannelBuyer> {
        self.next
    }

    /// Whether a channel has been funded and is awaiting the peer's
    /// confirmation. While this is set, no further rollover is started.
    pub fn awaiting_confirmation(&self) -> bool {
        self.pending.is_some()
    }

    /// Cumulative units signed on the channel in use. Only meaningful against
    /// that channel.
    pub fn cumulative(&self) -> u64 {
        self.active.map(|c| c.cumulative).unwrap_or(0)
    }

    /// Our own count of the budget left at `now`: nothing past its deadline.
    pub fn remaining_at(&self, now: Millis) -> u64 {
        if now >= self.deadline {
            0
        } else {
            self.remaining
        }
    }

    /// When the budget expires.
    pub fn deadline(&self) -> Millis {
        self.deadline
    }

    /// The rate reserved now.
    pub fn reserved(&self) -> u64 {
        self.reserved
    }

    /// When we last sent a TopUp.
    pub fn last_topup(&self) -> Option<Millis> {
        self.last_topup
    }

    /// The rate to reserve again at once, at `now`: the one reserved when the
    /// last session ended, while nothing has been bought in this one and we
    /// carry a budget into it. `0` otherwise.
    pub fn resume_rate(&self, now: Millis) -> u64 {
        if self.started || self.remaining_at(now) == 0 {
            0
        } else {
            self.resumed
        }
    }

    /// Start over with no channel, as a peer that lost the ones we paid it on
    /// makes us, keeping only the rate we would reserve again with the budget
    /// it may still hold for us.
    pub fn start_over(&mut self) {
        *self = Self {
            resumed: self.resumed,
            ..Self::new()
        };
    }

    /// The reserved-rate ceiling in force, if a provider named one recently
    /// enough.
    pub(super) fn cap(&self, now: Millis) -> Option<u64> {
        self.capped_at
            .filter(|(_, until)| now < *until)
            .map(|(rate, _)| rate)
    }

    /// Whether `id` is the channel being drained.
    pub fn is_active(&self, id: ChannelId) -> bool {
        self.active.is_some_and(|c| c.id == id)
    }

    /// Start a new session over the channels and budget kept from the last
    /// one.
    ///
    /// The budget is ours, and the provider keeps it across sessions, so our
    /// count of it stays too. The reservation does not: it ends with the
    /// session at the provider, so the next purchase reserves again. It is
    /// made at once, at the rate reserved before if nothing is observed yet
    /// (see [`Self::resume_rate`]), since the provider starts the gap over as
    /// well.
    pub fn restart(&mut self) {
        // The part second since the last whole one, up to the last reading,
        // as the provider draws it when the session ends.
        if let Some(second) = self.second.take() {
            let owed = second.close(second.last_read(), self.reserved);
            self.take(owed);
        }
        let resumed = if self.started {
            self.reserved
        } else {
            self.resumed
        };
        *self = Self {
            resumed,
            active: self.active,
            next: self.next,
            pending: self.pending,
            funding: self.funding,
            last_grant: self.last_grant,
            remaining: self.remaining,
            deadline: self.deadline,
            ..Self::new()
        };
    }

    /// Draw our own count down for what crossed the link by `now`, by the
    /// provider's rule: `max(moved, reserved × 1 s)` for each second. `moved`
    /// is already weighted by the provider's from-payer weight.
    ///
    /// When it reaches zero the reservation ends with it, as it does at the
    /// provider.
    pub fn draw(&mut self, moved: u64, now: Millis) {
        if now >= self.deadline {
            self.remaining = 0;
            self.reserved = 0;
            self.second = None;
            return;
        }
        let rate = self.reserved;
        let second = self.second.get_or_insert(Second::starting(now));
        let drawn = second.read(moved, now, rate);
        self.take(drawn);
    }

    /// Take `drawn` off our count, and end the reservation if that empties
    /// it.
    fn take(&mut self, drawn: u64) {
        self.remaining = self.remaining.saturating_sub(drawn);
        if self.remaining == 0 {
            self.reserved = 0;
        }
    }

    /// Take in a Balance the provider sent: what it says is left, and for how
    /// long.
    ///
    /// Information, not an instruction: we decide from our own count. The one
    /// case it fills in is a buyer that has no count at all — it came back
    /// without one, and holds nothing by its own reckoning — which learns from
    /// it the budget it left behind, as a payer that reconnects has to. Even
    /// then it can only make us buy less, never more.
    pub fn note_balance(&mut self, remaining: u64, expires_in_ms: u64, now: Millis) {
        if remaining == 0 || expires_in_ms == 0 || self.started {
            return;
        }
        if self.remaining_at(now) == 0 {
            self.remaining = remaining;
            self.deadline = self.deadline.max(now + expires_in_ms);
        }
    }

    /// Forget a channel we funded that the peer never confirmed.
    ///
    /// Only for a peer that has come back without knowing it: nothing was
    /// signed on it, and while it is held no rollover can start.
    pub fn forget_pending(&mut self) {
        self.pending = None;
    }

    /// Record that we have asked the host to fund a channel for this peer,
    /// under the id `request`.
    ///
    /// From here no rollover is started until the host answers — with the
    /// channel, [`Self::funded`], or with a failure,
    /// [`Self::funding_failed`] — or [`FUNDING_TIMEOUT_MS`] passes without
    /// either. Marking it only once the channel came back would leave every
    /// tick of the mint round trip free to ask for another.
    ///
    /// A request asked for after an earlier one timed out does not close the
    /// earlier one: whichever of them comes back first is taken.
    pub fn funding_requested(&mut self, request: u64, now: Millis) {
        let first = self.funding.map_or(request, |open| open.first);
        self.funding = Some(FundingRequests {
            first,
            latest: request,
            since: Some(now),
        });
    }

    /// The host could not fund the channel we asked for under `request`. The
    /// next check may ask again.
    ///
    /// A failure of a request already given up on changes nothing: a later one
    /// is being waited on.
    pub fn funding_failed(&mut self, request: u64) {
        if let Some(open) = self.funding.as_mut()
            && open.latest == request
        {
            open.since = None;
        }
    }

    /// Whether a funding request is still out and not yet given up on.
    pub fn funding_in_flight(&self, now: Millis) -> bool {
        self.funding
            .and_then(|open| open.since)
            .is_some_and(|since| now.saturating_since(since) < FUNDING_TIMEOUT_MS)
    }

    /// Whether the channel the host funded under `request` is one to take.
    ///
    /// It is if we asked this buyer for it and no channel has come back since
    /// — neither another answer, nor one still waiting in `pending`. The first
    /// answer to arrive for any request still open wins, and closes the rest:
    /// one channel was wanted, whichever request produced it. Any other is
    /// superseded, and its funds have to be reclaimed rather than a second
    /// channel opened on top of the first.
    pub fn answers(&self, request: u64) -> bool {
        self.pending.is_none()
            && self
                .funding
                .is_some_and(|open| (open.first..=open.latest).contains(&request))
    }

    /// Record a channel we have funded but the peer has not yet confirmed.
    ///
    /// Closes every funding request still open; check [`Self::answers`] first.
    pub fn funded(&mut self, id: ChannelId, capacity: u64, expires_at: Option<Millis>) {
        self.funding = None;
        self.pending = Some(ChannelBuyer::new(id, capacity, expires_at));
    }

    /// The peer confirmed the channel we funded.
    ///
    /// It becomes the one in use if there is none, and otherwise the
    /// replacement waiting behind it.
    pub fn confirmed(&mut self, id: ChannelId) {
        let Some(mut channel) = self.pending.take() else {
            return;
        };
        // The peer names the channel it verified, which is what the funding
        // determined; trust it over what we derived locally.
        channel.id = id;

        // A channel already drained to its capacity has nothing to wait
        // behind: the provider settled it the moment it filled, so a purchase
        // that still named it — even topped to exactly its capacity — would be
        // refused in full, and refused again on every re-buy.
        if self.active.is_none_or(|active| active.exhausted()) {
            self.active = Some(channel);
        } else {
            self.next = Some(channel);
        }
    }

    /// Whether to open a replacement for the channel in use.
    ///
    /// Rollover is started by the funder alone — only the party putting up new
    /// funds decides when — so this is only ever asked about our own channel.
    /// It stays false while one is already on the way, or the threshold would
    /// re-trigger on every check and fund a channel each time.
    ///
    /// Two triggers, and the second is the one that matters in practice:
    ///
    /// 1. **Past the threshold.** The channel is far enough through its
    ///    capacity to be worth replacing.
    /// 2. **Not enough headroom for another purchase like the last one.**
    ///    A threshold alone is reactive, and a purchase is not gradual: one
    ///    can take a channel from empty to full in a single step, and then
    ///    there is nothing to move onto.
    pub fn needs_rollover(&self, threshold_pct: u8) -> bool {
        if self.next.is_some() || self.pending.is_some() {
            return false;
        }
        let Some(active) = self.active else {
            return false;
        };
        if active.capacity == 0 {
            return false;
        }

        // Room for more than the next purchase and the one after it. Exactly
        // two is already too late: the replacement has to be funded, sent,
        // verified and confirmed before the channel in use runs dry, and that
        // is a round trip plus a mint swap.
        if active.headroom() <= self.last_grant.saturating_mul(2) {
            return true;
        }

        // Widened rather than saturated: saturating either side would make a
        // large channel look permanently past its threshold.
        (active.cumulative as u128) * 100 >= (active.capacity as u128) * (threshold_pct as u128)
    }

    /// Whether to open a replacement for the channel in use, and why.
    ///
    /// [`Self::needs_rollover`], plus the second clock a channel runs on: past
    /// its expiry we can reclaim it, so the receiver has to settle it before
    /// then, and it has to be replaced before the receiver does. A channel drawn
    /// slowly enough reaches that long before it fills.
    ///
    /// `margin_ms` is the safety margin,
    /// [`NodePolicy::safety_margin_ms`](crate::config::NodePolicy::safety_margin_ms).
    ///
    /// Quiet while a funding request is out, from the moment it is asked for —
    /// see [`Self::funding_requested`].
    pub fn rollover_due(
        &self,
        threshold_pct: u8,
        now: Millis,
        margin_ms: u64,
    ) -> Option<RolloverReason> {
        if self.funding_in_flight(now) {
            return None;
        }
        if self.needs_rollover(threshold_pct) {
            return Some(RolloverReason::Capacity);
        }
        if self.next.is_some() || self.pending.is_some() {
            return None;
        }
        self.active
            .filter(|active| active.expiring(now, margin_ms))
            .map(|_| RolloverReason::Expiry)
    }

    /// Stop using a channel that is about to expire.
    ///
    /// Inside the safety margin a confirmed replacement takes over at once,
    /// whatever is left on the old channel: the receiver is about to settle it,
    /// and anything signed on it after that is refused. Without a replacement
    /// the old channel stays in use until `settle_lead_ms` before expiry, when
    /// the receiver settles it — and from then there is nothing to buy on until
    /// the replacement is confirmed. The budget is not touched: what was
    /// bought on the channel is kept apart from it.
    ///
    /// Returns the channel given up, which is the receiver's to settle, not
    /// ours.
    pub fn retire_expiring(
        &mut self,
        now: Millis,
        margin_ms: u64,
        settle_lead_ms: u64,
    ) -> Option<ChannelId> {
        let active = self.active?;
        if !active.expiring(now, margin_ms) {
            return None;
        }
        if self.next.is_some() {
            self.active = self.next.take();
        } else if active.expiring(now, settle_lead_ms) {
            self.active = None;
        } else {
            return None;
        }
        // The undo step describes channels that are no longer both there, so
        // a late refusal must not bring the retired one back.
        self.prior = None;
        Some(active.id)
    }

    /// Commit to a purchase we have decided to send.
    ///
    /// The grant is added to our count of the budget, the deadline becomes the
    /// later of the old one and now plus the window, and the reserved rate
    /// replaces the one before — exactly as the provider will apply it.
    ///
    /// Returns the channel that this purchase exhausted, if any. That channel
    /// has been drained to its capacity and can be settled — its replacement is
    /// already carrying the overflow.
    pub fn record(&mut self, purchase: Purchase, now: Millis) -> Option<ChannelId> {
        self.prior = Some(Prior {
            active: self.active,
            next: self.next,
            deadline: self.deadline,
            reserved: self.reserved,
            started: self.started,
            resumed: self.resumed,
            grant: purchase.grant,
        });

        if let Some(active) = self.active.as_mut() {
            debug_assert_eq!(active.id, purchase.first.channel_id);
            active.cumulative = purchase.first.cumulative;
        }
        if let Some(leg) = purchase.second
            && let Some(next) = self.next.as_mut()
        {
            debug_assert_eq!(next.id, leg.channel_id);
            next.cumulative = leg.cumulative;
        }

        // A changed reserved rate splits the second being drawn, as it does at
        // the provider.
        if purchase.reserved_rate != self.reserved
            && let Some(second) = self.second.take()
        {
            let owed = second.close(now, self.reserved);
            self.take(owed);
        }
        self.remaining = self.remaining_at(now).saturating_add(purchase.grant);
        self.deadline = self.deadline.max(now + purchase.window_ms);
        self.reserved = purchase.reserved_rate;
        self.started = true;
        self.resumed = 0;
        self.last_topup = Some(now);
        self.last_grant = purchase.grant;
        self.second.get_or_insert(Second::starting(now));

        // A purchase at or under the cap does not disprove it, so the cap
        // stands until it expires on its own.
        self.retire_exhausted()
    }

    /// Move on from a channel drained to its capacity.
    fn retire_exhausted(&mut self) -> Option<ChannelId> {
        let active = self.active?;
        if !active.exhausted() {
            return None;
        }
        // Only step forward once the replacement is actually there. Without one
        // there is nothing to step to, and the buyer stops buying until the
        // rollover completes — which is the correct outcome, not a stall: the
        // channel really is full.
        let replacement = self.next.take()?;
        self.active = Some(replacement);
        Some(active.id)
    }

    /// Record that the provider refused a purchase, why, and the highest
    /// reserved rate it said it would accept.
    ///
    /// The refusal costs nothing directly — an unclaimed state is worth nothing
    /// to the provider, so our money is untouched. Because the provider did not
    /// turn its ratchet, we undo ours: the next purchase has to be built on the
    /// last total the provider actually accepted, or it would be refused for
    /// exactly the same reason. `refused` identifies which purchase was
    /// refused, so a stale refusal for one we have already moved past rewinds
    /// nothing.
    ///
    /// By reason: for capacity we keep to the rate named for `hold_ms`. Too
    /// soon needs nothing more, since the next purchase already waits a gap
    /// from the one refused; nor does a window or reserved rate out of range,
    /// since every purchase is fitted to the Offer as it now stands.
    pub fn record_reject(
        &mut self,
        refused: &[(ChannelId, u64)],
        reason: ReasonCode,
        max_reserved_rate: u64,
        now: Millis,
        hold_ms: u64,
    ) {
        if reason == ReasonCode::RateExceedsCapacity {
            self.capped_at = Some((max_reserved_rate, now + hold_ms));
        }

        // A purchase is refused in full, so it is enough that any of the totals
        // named is one we currently hold — they were all sent together.
        let holds = |c: Option<ChannelBuyer>| {
            c.is_some_and(|c| {
                refused
                    .iter()
                    .any(|(id, cum)| *id == c.id && *cum == c.cumulative)
            })
        };
        if !holds(self.active) && !holds(self.next) {
            return;
        }
        if let Some(prior) = self.prior.take() {
            self.active = prior.active;
            self.next = prior.next;
            self.remaining = self.remaining.saturating_sub(prior.grant);
            self.deadline = prior.deadline;
            self.reserved = prior.reserved;
            self.started = prior.started;
            self.resumed = prior.resumed;
        }
    }
}
