//! Policy the operator sets, in the form core consumes it.
//!
//! This is not the YAML schema — parsing, file cascades and defaults-on-disk
//! are the host's job. These are the already-resolved values core reads when it
//! decides whether to honor a grant and how hard to shape a peer.
//!
//! There is no price anywhere, because delivery has no price: one voucher buys
//! one unit. What a unit costs in money is settled where vouchers are sold.

use alloc::string::String;
use alloc::vec::Vec;

/// What a payer may buy: the terms advertised in the Offer, and the capacity
/// this node will promise across all its payers.
///
/// Each payer has one budget, one deadline and one reserved rate. A purchase
/// adds to the budget, and every second the node draws
/// `max(units moved, reserved rate × 1 s)` from it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrantPolicy {
    /// Shortest window a TopUp may carry, in milliseconds. Never below
    /// [`Self::min_topup_gap_ms`], or a budget could expire before its payer is
    /// allowed to renew it — see [`Self::window_range_valid`].
    pub min_window_ms: u64,
    /// Longest window, in milliseconds: how long a budget can be kept without
    /// buying again. A month by default, so a phone can use a data pack over
    /// weeks. A reserved budget drains at its rate whatever the window, so a
    /// long window lets nobody bank capacity.
    pub max_window_ms: u64,
    /// Smallest reserved rate a TopUp may carry, in units per second. `0` lets
    /// a payer reserve nothing and pay only for what it moves; above `0` this
    /// node sells only time at a speed.
    pub min_reserved_rate: u64,
    /// Shortest time between two TopUps from one payer, in milliseconds.
    ///
    /// Every TopUp costs signature checks and a write to disk, and nothing is
    /// lost by buying often, so this is what bounds how often a payer can make
    /// the node do that. On a constrained provider it is the binding limit, not
    /// bandwidth. The host checks it before verifying any signature.
    pub min_topup_gap_ms: u64,
    /// Units per second this node will reserve across **all** its payers
    /// together, or `None` for "whatever the link will bear". A TopUp whose
    /// reserved rate would take the sum of every connected payer's past it is
    /// refused, with the rate still free attached. It counts reservations
    /// only: speed given above them is spare capacity.
    pub max_rate: Option<u64>,
}

impl Default for GrantPolicy {
    fn default() -> Self {
        Self {
            // One second to thirty days.
            min_window_ms: 1_000,
            max_window_ms: 2_592_000_000,
            min_reserved_rate: 0,
            // One TopUp a second per payer, worst case.
            min_topup_gap_ms: 1_000,
            max_rate: None,
        }
    }
}

impl GrantPolicy {
    /// Whether a payer-chosen window falls inside what we advertise.
    pub fn window_acceptable(&self, window_ms: u64) -> bool {
        (self.min_window_ms..=self.max_window_ms).contains(&window_ms)
    }

    /// Whether the window range can be advertised at all: a range that is not
    /// one, or whose shortest window is shorter than the gap between
    /// purchases, which would let a budget expire before its payer may renew
    /// it. A node refuses to start with either.
    pub fn window_range_valid(&self) -> bool {
        self.min_window_ms <= self.max_window_ms && self.min_window_ms >= self.min_topup_gap_ms
    }
}

/// How fast a payer that has budget is carried. The node's own policy; it never
/// reaches the protocol.
///
/// A payer is promised its reserved rate and nothing more. Anything above it is
/// spare capacity, which the node may give or keep, and admission control does
/// not count it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BurstPolicy {
    /// A payer that reserved a rate is carried at `max(reserved, rate)`. `0`,
    /// the default, carries it at exactly its reserved rate.
    pub rate: u64,
    /// A payer that reserved nothing is carried at this, in units per second.
    /// `u64::MAX`, the default, is as fast as the link allows; `0` leaves it
    /// only the minimum flow allowance until it reserves.
    pub unreserved_rate: u64,
}

impl Default for BurstPolicy {
    fn default() -> Self {
        Self {
            rate: 0,
            unreserved_rate: u64::MAX,
        }
    }
}

/// This node's own settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodePolicy {
    /// Quantity unit — `"byte"` for network forwarding. Fixed by the resource
    /// and identical across every node selling it.
    pub unit: String,
    /// Mints whose vouchers we take payment in, most preferred first. Never
    /// empty. Each entry is that issuer's credit risk, taken on deliberately.
    pub accepted_mints: Vec<String>,
    /// What one unit from a payer draws from its budget, where one unit to it
    /// draws one, before per-peer overrides:
    /// `moved = to_payer + from_payer × from_payer_weight`. `1` charges both
    /// directions alike, `10` suits a 100/10 line, `0` makes what the payer
    /// sends free. Unsigned, so no payer is ever paid for sending. Fixed for a
    /// session.
    pub from_payer_weight: u16,
    /// Traffic every peer gets without paying, in units per second.
    ///
    /// This is a **rate and the floor of the shaper**, not a stored quantity —
    /// an unused second of it is gone. It is what a peer falls back to when its
    /// budget runs out or expires, which is what leaves the link alive enough
    /// to carry the TopUp that revives it. Keep it small: it is given away, and its resale
    /// value is bounded by economics rather than by cryptography.
    pub minimum_flow: u64,
    /// What a payer may buy.
    pub grants: GrantPolicy,
    /// How fast a payer is carried above its reserved rate, before per-peer
    /// overrides.
    pub burst: BurstPolicy,
    /// How often the host reads the meters and ticks core, in milliseconds.
    ///
    /// A payer's speed is clipped near the end of its budget so it cannot move
    /// more in one tick than it has left, and this is that tick.
    pub tick_ms: u64,
    /// Units of capacity to open the first outgoing channel to a peer with.
    ///
    /// Channels exist to bound the issuer's spent-proof set, not to prevent
    /// theft, so this trades how often a rollover runs against how much is
    /// committed at once. It starts small because a new peering may not last,
    /// and grows by [`Self::capacity_growth_pct`] as the relationship proves
    /// itself.
    pub initial_channel_capacity: u64,
    /// No outgoing channel is opened smaller than this.
    pub min_channel_capacity: u64,
    /// No outgoing channel is opened larger than this, however long the
    /// relationship. It is the most either side has at stake in one channel:
    /// the funder's lockup, and the earnings a receiver loses if it forgets the
    /// last signed state before settling.
    pub max_channel_capacity: u64,
    /// What a replacement channel is sized at, as a percentage of the one it
    /// replaces, when the rollover was forced by use. `200` doubles it; `100`
    /// never grows.
    ///
    /// A percentage rather than a float for the same reason as
    /// [`Self::rollover_threshold_pct`].
    pub capacity_growth_pct: u32,
    /// The safety margin before a channel's expiry, in milliseconds: the funder
    /// rolls a channel over when it enters it, and the receiver settles it
    /// halfway through ([`Self::settle_lead_ms`]).
    ///
    /// It does not depend on the window. What a payer buys on a channel is kept
    /// in its budget, apart from the channel, and survives the channel's
    /// settlement, so no window puts a lower bound on how long a channel must
    /// live. Both ends of a channel must use the same value; it is not
    /// advertised.
    pub safety_margin_ms: u64,
    /// Drop a peer that has sent nothing at all for this long, in
    /// milliseconds. Zero disables it.
    ///
    /// Only silence, not non-payment: a peer that stops buying simply runs out
    /// of budget and falls to the minimum flow allowance, which costs nothing to
    /// hold open and needs no timer.
    pub stale_timeout_ms: u64,
    /// Percentage of channel capacity at which the funder starts a rollover.
    ///
    /// A percentage rather than a fraction so the comparison stays integer —
    /// there is no reason to pull float math onto a device that would rather
    /// not have it.
    pub rollover_threshold_pct: u8,
}

impl Default for NodePolicy {
    fn default() -> Self {
        Self {
            unit: String::from("byte"),
            accepted_mints: Vec::new(),
            from_payer_weight: 1,
            minimum_flow: 0,
            grants: GrantPolicy::default(),
            burst: BurstPolicy::default(),
            tick_ms: 1_000,
            initial_channel_capacity: Self::DEFAULT_INITIAL_CAPACITY,
            min_channel_capacity: 1 << 27,
            max_channel_capacity: 1 << 34,
            capacity_growth_pct: 200,
            safety_margin_ms: 60_000,
            stale_timeout_ms: Self::DEFAULT_STALE_TIMEOUT_MS,
            rollover_threshold_pct: 80,
        }
    }
}

impl NodePolicy {
    /// 1 GiB: one proof at the largest denomination the channel backend
    /// funds with, so the first channel costs the smallest funding blob and
    /// spent-proof entry it can.
    pub const DEFAULT_INITIAL_CAPACITY: u64 = 1 << 30;

    /// Fit a capacity into the operator's bounds.
    pub fn clamp_capacity(&self, capacity: u64) -> u64 {
        // `max` first, so a misconfigured `min > max` still yields the ceiling
        // rather than panicking in `clamp`.
        capacity
            .max(self.min_channel_capacity)
            .min(self.max_channel_capacity)
    }

    /// What the first channel to a peer opens with.
    pub fn first_channel_capacity(&self) -> u64 {
        self.clamp_capacity(self.initial_channel_capacity)
    }

    /// What to open a replacement for a channel of `capacity` with, after a
    /// rollover that use forced.
    ///
    /// Growth is what "start small, grow with the relationship" comes down to:
    /// a peer that has filled one channel is likely to fill the next, and a
    /// bigger one means fewer rollovers for the same traffic.
    pub fn grown_capacity(&self, capacity: u64) -> u64 {
        let grown = (capacity as u128) * (self.capacity_growth_pct as u128) / 100;
        self.clamp_capacity(grown.min(u64::MAX as u128) as u64)
    }

    /// The stale timeout a node runs with unless told otherwise.
    pub const DEFAULT_STALE_TIMEOUT_MS: u64 = 60_000;

    /// How long we may send a peer nothing before we send a keepalive.
    ///
    /// A third of the stale timeout, so two can be lost before the peer gives
    /// up on us. The peer's timeout is not advertised, so this assumes it is
    /// the same as ours — as the safety margin assumes the same floor. With
    /// ours switched off, the default's: the peer may still have one.
    pub fn keepalive_interval_ms(&self) -> u64 {
        let timeout = match self.stale_timeout_ms {
            0 => Self::DEFAULT_STALE_TIMEOUT_MS,
            ms => ms,
        };
        (timeout / 3).max(1)
    }

    /// How long before expiry the receiver settles a channel.
    ///
    /// Half the safety margin: late enough that a funder rolling over on time
    /// has already moved onto the replacement, early enough to leave time
    /// before the refund path opens and the earnings on it are gone. Core asks
    /// for the settlement once; retrying one that fails is the host's to do.
    pub fn settle_lead_ms(&self) -> u64 {
        self.safety_margin_ms / 2
    }
}

/// Per-peer overrides on top of [`NodePolicy`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PeerPolicy {
    /// Do not charge this peer at all.
    ///
    /// One-sided: it controls only whether *we* charge, never whether the peer
    /// charges back. It is also **not transitive** — free for that peer's own
    /// traffic, never free for anything it is merely the nominal beneficiary
    /// of, which would make an uncharged peer a way to launder free transit.
    pub no_charge: bool,
    /// Refuse this peer entirely.
    pub blocked: bool,
    /// Override the node-wide from-payer weight for this peer. A peering
    /// partner usually gets `0`. Like the node-wide one, it is fixed for a
    /// session: a change applies from the peer's next.
    pub from_payer_weight: Option<u16>,
    /// Override [`BurstPolicy::rate`] for this peer.
    pub burst_rate: Option<u64>,
    /// Override [`BurstPolicy::unreserved_rate`] for this peer.
    pub unreserved_rate: Option<u64>,
}

impl PeerPolicy {
    /// The from-payer weight a session with this peer starts at.
    pub fn weight(&self, node: &NodePolicy) -> u16 {
        self.from_payer_weight.unwrap_or(node.from_payer_weight)
    }

    /// How fast this peer is carried above its reserved rate.
    pub fn burst(&self, node: &NodePolicy) -> BurstPolicy {
        BurstPolicy {
            rate: self.burst_rate.unwrap_or(node.burst.rate),
            unreserved_rate: self.unreserved_rate.unwrap_or(node.burst.unreserved_rate),
        }
    }
}
