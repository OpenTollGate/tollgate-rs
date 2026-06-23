//! Access control levels (see `docs/design/core/tollgate-access-control.md`).

/// What a peer is currently allowed to do.
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub enum AccessLevel {
    /// Blocked; no resource delivery; hidden from discovery. The default for a
    /// new peer — no service until it pays.
    #[default]
    None,
    /// Allowed and metered (charged per the agreed price).
    Active,
    /// Allowed but free — delivered without metering.
    ZeroPrice,
    /// Blocked, e.g. after the balance is exhausted; hidden from discovery.
    /// Distinct from [`None`](Self::None) so the host can re-admit on top-up.
    Suspended,
}

impl AccessLevel {
    /// Whether resources may be delivered to/from the peer.
    pub const fn allows_delivery(self) -> bool {
        matches!(self, Self::Active | Self::ZeroPrice)
    }

    /// Whether delivery is charged for.
    pub const fn is_metered(self) -> bool {
        matches!(self, Self::Active)
    }

    /// Whether the peer should be advertised to others (e.g. included in a FIPS
    /// bloom filter). Mirrors delivery: blocked peers are hidden.
    pub const fn is_visible(self) -> bool {
        self.allows_delivery()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn predicate_truth_table() {
        use AccessLevel::*;
        // (level, allows_delivery, is_metered, is_visible)
        let cases = [
            (None, false, false, false),
            (Active, true, true, true),
            (ZeroPrice, true, false, true),
            (Suspended, false, false, false),
        ];
        for (level, deliver, metered, visible) in cases {
            assert_eq!(
                level.allows_delivery(),
                deliver,
                "{level:?} allows_delivery"
            );
            assert_eq!(level.is_metered(), metered, "{level:?} is_metered");
            assert_eq!(level.is_visible(), visible, "{level:?} is_visible");
        }
    }

    #[test]
    fn default_is_blocked() {
        assert_eq!(AccessLevel::default(), AccessLevel::None);
    }
}
