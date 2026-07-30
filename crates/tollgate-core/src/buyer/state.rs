//! What this node has bought from one peer, and the policy it buys under.

use crate::time::Millis;

/// How aggressively to buy. Deliberately simple — the design leaves window
/// choice open ("the payer trades responsiveness against forfeiture and message
/// count, with no obvious default"), so this is one workable policy rather than
/// the policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuyerPolicy {
    /// Buy this percentage of observed demand. Above 100 leaves headroom so a
    /// rising flow is not shaped before the next purchase lands.
    pub headroom_pct: u32,
    /// Only jump mid-window if the target exceeds the rate in force by this
    /// percentage.
    ///
    /// This is the hysteresis that makes the forfeiture rule livable: raising
    /// the rate early burns the remainder, and the design points out that large
    /// jumps are cheap while small adjustments are punitive. A high threshold
    /// is what stops the buyer fiddling.
    pub raise_threshold_pct: u32,
    /// Renew this long before the deadline, so the next grant lands before the
    /// current one lapses and the peer drops to the minimum flow allowance.
    pub renew_lead_ms: u32,
    /// Window to ask for, clamped to what the provider advertised.
    ///
    /// Short windows keep the forfeit small and reaction quick, at the cost of
    /// more signature verifications for the provider.
    pub window_ms: u32,
    /// Never buy below this rate, so an idle link keeps a little capacity ready.
    pub min_rate: u64,
    /// Never buy above this rate. The operator's spending ceiling — vouchers
    /// cost money to acquire, whatever the protocol thinks.
    pub max_rate: u64,
}

impl Default for BuyerPolicy {
    fn default() -> Self {
        Self {
            headroom_pct: 125,
            raise_threshold_pct: 150,
            renew_lead_ms: 500,
            window_ms: 2_000,
            min_rate: 0,
            max_rate: u64::MAX,
        }
    }
}

/// Window bounds the provider advertised in its Offer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowBounds {
    /// Smallest window the provider will accept.
    pub min_ms: u32,
    /// Largest window the provider will accept.
    pub max_ms: u32,
}

impl WindowBounds {
    /// Fit a preferred window into what the provider will take.
    pub fn clamp(&self, want_ms: u32) -> u32 {
        want_ms.clamp(self.min_ms, self.max_ms)
    }
}

/// A purchase decided but not yet sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Purchase {
    /// The new cumulative total to sign. Monotonic, which is what the ratchet
    /// requires and what makes the message idempotent.
    pub cumulative: u64,
    /// Window to spend it in, already clamped to the provider's range.
    pub window_ms: u32,
    /// Rate this buys.
    pub rate: u64,
    /// Units bought by this purchase alone.
    pub grant: u64,
    /// Units of the previous grant given up to make this one. Zero on a
    /// renewal that waited for the deadline; the price of reacting early
    /// otherwise. Worth logging — it is the cost the policy is trading against.
    pub forfeited: u64,
    /// Why the buyer acted, for the operator's benefit.
    pub trigger: Trigger,
}

/// What prompted a purchase.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trigger {
    /// No grant has been bought yet on this channel.
    First,
    /// The grant in force is about to lapse.
    Renewal,
    /// Demand climbed far enough to be worth forfeiting the remainder for.
    DemandRose,
    /// The provider refused the last purchase and named a rate it would take.
    Rebuy,
}

/// The payer's side of one channel.
#[derive(Debug, Clone, Copy, Default)]
pub struct Buyer {
    /// Cumulative units signed for on this channel, ever.
    pub(super) cumulative: u64,
    /// Rate the grant in force bought.
    pub(super) rate: u64,
    /// When it lapses.
    pub(super) deadline: Millis,
    /// Whether anything has been bought yet.
    pub(super) started: bool,
    /// Ceiling the provider last told us it would honor, from a TopUpReject.
    /// Cleared once a purchase under it succeeds.
    pub(super) capped_at: Option<u64>,
    /// State as it stood before the most recent purchase.
    ///
    /// A TopUp is fire-and-forget, so we assume it landed and advance. If a
    /// TopUpReject comes back, the provider never turned its ratchet — and if
    /// we kept our own advanced, every subsequent purchase would be computed
    /// from a total the provider does not recognise, and would be refused for
    /// the same reason as the first. So we keep exactly one step of history to
    /// undo.
    pub(super) prior: Option<Prior>,
}

/// The buyer's state immediately before a purchase, kept so a rejection can be
/// undone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Prior {
    pub(super) cumulative: u64,
    pub(super) rate: u64,
    pub(super) deadline: Millis,
    pub(super) started: bool,
}

impl Buyer {
    /// A buyer that has not yet purchased anything.
    pub const fn new() -> Self {
        Self {
            cumulative: 0,
            rate: 0,
            deadline: Millis::ZERO,
            started: false,
            capped_at: None,
            prior: None,
        }
    }

    /// Cumulative units signed for.
    pub fn cumulative(&self) -> u64 {
        self.cumulative
    }

    /// The rate currently bought.
    pub fn rate(&self) -> u64 {
        self.rate
    }

    /// When the grant in force lapses.
    pub fn deadline(&self) -> Millis {
        self.deadline
    }

    /// Units of the grant in force that are still unspent from our side's point
    /// of view — an upper bound, since we cannot see the provider's counters.
    pub fn unspent_at(&self, now: Millis) -> u64 {
        if !self.started || now >= self.deadline {
            return 0;
        }
        crate::grant::units_in(self.rate, self.deadline.saturating_since(now))
    }

    /// Commit to a purchase we have decided to send.
    pub fn record(&mut self, purchase: Purchase, now: Millis) {
        self.prior = Some(Prior {
            cumulative: self.cumulative,
            rate: self.rate,
            deadline: self.deadline,
            started: self.started,
        });
        self.cumulative = purchase.cumulative;
        self.rate = purchase.rate;
        self.deadline = now + purchase.window_ms as u64;
        self.started = true;
        self.capped_at = None;
    }

    /// Record that the provider refused a purchase, and at what rate it said it
    /// would accept one.
    ///
    /// The refusal costs nothing directly — an unclaimed state is worth nothing
    /// to the provider, so our money is untouched. What it buys us is the rate
    /// to re-purchase at, in one round trip.
    ///
    /// Because the provider did not turn its ratchet, we undo ours: the next
    /// purchase has to be built on the last total the provider actually
    /// accepted, or it would be refused for exactly the same reason.
    /// `cumulative_rejected` identifies which purchase was refused, so a stale
    /// rejection for a purchase we have already moved past only records the
    /// cap and does not rewind anything.
    pub fn record_reject(&mut self, cumulative_rejected: u64, max_rate_available: u64) {
        self.capped_at = Some(max_rate_available);

        if cumulative_rejected != self.cumulative {
            return;
        }
        if let Some(prior) = self.prior.take() {
            self.cumulative = prior.cumulative;
            self.rate = prior.rate;
            self.deadline = prior.deadline;
            self.started = prior.started;
        }
    }
}
