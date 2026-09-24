//! The delivery gate.
//!
//! Core decides *what* to enforce; the host's resource adapter decides *how* —
//! a FIPS delivery filter, an nftables rule, a token bucket. The level says only
//! whether delivery is allowed and whether it is metered. How much a peer has
//! left to spend lives in [`crate::grant`] and never surfaces here.

/// A peer's delivery status. Exactly one at any time.
///
/// `Active` and `Free` are a **TollGate session**: the two sides agreed a
/// price and payment is flowing, or agreed not to charge. `None` is everything
/// else — a peer that has not paid yet and one whose payment has lapsed alike.
/// The minimum flow allowance is not a session; it is what a peer gets at
/// `None`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AccessLevel {
    /// No TollGate session: nothing funded, or every channel drained and none
    /// replacing it. Only the minimum flow allowance is delivered, and nothing
    /// if the allowance is zero. TollGate messages always flow, so the peer
    /// can pay its way into a session without reconnecting.
    #[default]
    None,
    /// Channels funded. Delivery allowed, metered, shaped to what was bought —
    /// never below the allowance, including between grants.
    Active,
    /// Neither side charges the other. Delivery allowed and unmetered — a
    /// decision about the relationship, not a price set to zero.
    Free,
}

impl AccessLevel {
    /// Whether the peer is in a TollGate session, so resources may be delivered
    /// for or through it beyond the minimum flow allowance.
    pub fn delivery_allowed(self) -> bool {
        matches!(self, Self::Active | Self::Free)
    }

    /// Whether traffic is drawn against a grant. `Free` peers are delivered to
    /// without any grant existing.
    pub fn metered(self) -> bool {
        matches!(self, Self::Active)
    }

    /// Whether the peer should appear in reachability advertisements.
    ///
    /// In FIPS this is bloom-filter inclusion: advertising a peer we will not
    /// deliver through invites other nodes to route into a blackhole. It is
    /// inferred from the level rather than set separately, so the two can never
    /// disagree.
    pub fn advertise(self) -> bool {
        self.delivery_allowed()
    }
}
