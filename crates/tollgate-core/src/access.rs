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
    /// replacing it. Nothing is metered; the peer is carried at the minimum
    /// flow allowance if there is one, and not at all if the allowance is zero.
    /// TollGate messages always flow, so the peer can pay its way into a
    /// session without reconnecting.
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
    /// for or through it beyond the minimum flow allowance. The gate an adapter
    /// applies is [`carried`](Self::carried).
    pub fn delivery_allowed(self) -> bool {
        matches!(self, Self::Active | Self::Free)
    }

    /// Whether a peer's traffic is carried at all, given the rate core shaped
    /// it to. This is the gate every adapter applies.
    ///
    /// Not the same question as [`delivery_allowed`](Self::delivery_allowed).
    /// A peer that is not paying is still carried at the minimum flow
    /// allowance, because the allowance is a rate rather than a level — so it
    /// is the rate, not the level, that closes the gate, and only when the
    /// allowance is zero. Traffic to and from this node itself is never gated
    /// by this: the peer must always be able to reach us to pay.
    pub fn carried(self, rate: u64) -> bool {
        self == Self::Free || rate > 0
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unpaid_peer_is_carried_at_the_allowance() {
        assert!(AccessLevel::None.carried(4_096), "at the allowance");
        assert!(
            !AccessLevel::None.delivery_allowed(),
            "but not advertised or metered"
        );
    }

    #[test]
    fn with_no_allowance_an_unpaid_peer_is_not_carried() {
        assert!(!AccessLevel::None.carried(0), "with the allowance disabled");
    }

    #[test]
    fn a_lapsed_grant_with_no_allowance_closes_the_gate_too() {
        // Active with nothing live is a peer between grants — a channel still
        // funded but its grant run out — and core shapes it to the allowance;
        // zero allowance leaves it nothing.
        assert!(AccessLevel::Active.carried(1_250_000));
        assert!(!AccessLevel::Active.carried(0));
    }

    #[test]
    fn a_free_peer_is_always_carried() {
        assert!(AccessLevel::Free.carried(u64::MAX));
        assert!(AccessLevel::Free.carried(0));
    }
}
