//! Pricing and products.
//!
//! v1 is **static** pricing only. Dynamic (formula-driven) pricing — evaluating
//! an expression against opaque link metrics — is deferred; see
//! `docs/design/core/tollgate-pricing.md`.

use alloc::vec::Vec;
use tollgate_protocol::{MintPrice, ProductId, product_id};

/// The rate a node charges a peer, as scaled integers (in the same scale as the
/// peer's balance — milli-units when `pricing_scale` = 1000). Both are signed:
/// positive = the peer pays, zero = free, negative = we pay (not used in
/// bootstrap-only mode).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Price {
    pub per_second: i64,
    pub per_unit: i64,
}

impl Price {
    /// Cost (in scaled units) of `elapsed_ms` of time plus `units` delivered.
    /// **Signed**: a negative rate yields a negative cost — the node *pays* the
    /// peer to attract resources (negative pricing in
    /// `docs/design/core/tollgate-pricing.md`), so billing credits the balance
    /// rather than debiting it.
    pub fn cost_scaled(&self, elapsed_ms: u64, units: u64) -> i64 {
        let time = (elapsed_ms as i64).saturating_mul(self.per_second) / 1000;
        let unit = (units as i64).saturating_mul(self.per_unit);
        time.saturating_add(unit)
    }
}

/// A priced offer across one or more mints.
#[derive(Clone, Debug)]
pub struct Product {
    /// Divisor for sub-unit precision (default [`tollgate_protocol::DEFAULT_PRICING_SCALE`]).
    pub pricing_scale: u32,
    /// Per-mint pricing. A peer picks one mint from this list.
    pub prices: Vec<MintPrice>,
    /// Opaque, implementation-defined extension bytes, hashed into the id.
    pub extensions: Vec<u8>,
}

impl Product {
    /// The canonical fingerprint of this product.
    pub fn id(&self) -> ProductId {
        product_id(self.pricing_scale, &self.prices, &self.extensions)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cost_combines_time_and_units() {
        let price = Price {
            per_second: 2,
            per_unit: 3,
        };
        // 2 s × 2 + 10 units × 3 = 4 + 30 = 34
        assert_eq!(price.cost_scaled(2000, 10), 34);
    }

    #[test]
    fn cost_is_zero_for_free_price() {
        assert_eq!(Price::default().cost_scaled(10_000, 1_000), 0);
    }

    #[test]
    fn negative_rate_yields_a_signed_credit() {
        // Negative pricing: the node pays the peer, so the cost is negative and
        // billing will *credit* the balance (no clamp to zero).
        let price = Price {
            per_second: 0,
            per_unit: -2,
        };
        assert_eq!(price.cost_scaled(0, 10), -20); // 10 units × −2 = −20
        // Time and units combine with sign: −5/s for 2 s, +3/unit for 10 units.
        let mixed = Price {
            per_second: -5,
            per_unit: 3,
        };
        assert_eq!(mixed.cost_scaled(2000, 10), 20); // −10 + 30
    }
}
