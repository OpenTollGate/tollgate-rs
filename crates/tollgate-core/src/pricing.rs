//! Pricing and products.
//!
//! v1 is **static** pricing only. Dynamic (formula-driven) pricing — evaluating
//! an expression against opaque link metrics — is deferred; see
//! `docs/design/core/tollgate-pricing.md`.

use alloc::vec::Vec;
use tollgate_protocol::{MintPrice, ProductId, product_id};

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
