//! Products and the canonical `product_id`.

use alloc::string::String;
use alloc::vec::Vec;
use sha2::{Digest, Sha256};

/// Default pricing scale: prices are expressed in thousandths of the base unit
/// (milli-units), so integer arithmetic keeps sub-unit precision.
pub const DEFAULT_PRICING_SCALE: u32 = 1000;

/// Per-mint pricing for a product. Both prices are signed — a peer may pay you
/// (positive), be free (zero), or be paid by you (negative).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MintPrice {
    pub mint_url: String,
    pub price_per_second: i64,
    pub price_per_unit: i64,
}

/// A product fingerprint. Any change to pricing, mints, or extensions changes
/// the id, so a single comparison detects a re-priced product.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct ProductId(pub [u8; 32]);

/// Compute the canonical `product_id`.
///
/// **Canonicalization decision** (the fix for the cross-implementation hashing
/// gap): we deliberately do *not* `SHA256(cbor_map)`. Two encoders can order
/// map keys differently and produce different ids for the same product, which
/// would silently break product matching between Rust, Go, and esp32. Instead
/// we hash an explicit, fixed byte layout behind a domain-separation tag:
///
/// ```text
/// "tollgate/product-id/v1"
/// pricing_scale            : u32 big-endian
/// count(prices)            : u32 big-endian
/// for each price (sorted by mint_url bytes):
///     len(mint_url)        : u32 big-endian
///     mint_url             : raw UTF-8 bytes
///     price_per_second     : i64 big-endian
///     price_per_unit       : i64 big-endian
/// len(extensions)          : u32 big-endian
/// extensions               : raw bytes, hashed verbatim
/// ```
///
/// `extensions` is treated as an **opaque** byte string: the producer
/// serializes it once and every implementation hashes the identical bytes,
/// so core never has to agree on how to (re-)encode it. Prices are sorted, so
/// the id is independent of the order they were declared in.
pub fn product_id(pricing_scale: u32, prices: &[MintPrice], extensions: &[u8]) -> ProductId {
    let mut sorted: Vec<&MintPrice> = prices.iter().collect();
    sorted.sort_unstable_by(|a, b| a.mint_url.as_bytes().cmp(b.mint_url.as_bytes()));

    let mut hasher = Sha256::new();
    hasher.update(b"tollgate/product-id/v1");
    hasher.update(pricing_scale.to_be_bytes());
    hasher.update((sorted.len() as u32).to_be_bytes());
    for price in sorted {
        let url = price.mint_url.as_bytes();
        hasher.update((url.len() as u32).to_be_bytes());
        hasher.update(url);
        hasher.update(price.price_per_second.to_be_bytes());
        hasher.update(price.price_per_unit.to_be_bytes());
    }
    hasher.update((extensions.len() as u32).to_be_bytes());
    hasher.update(extensions);

    let digest = hasher.finalize();
    let mut id = [0u8; 32];
    id.copy_from_slice(&digest);
    ProductId(id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::ToString;
    use alloc::vec;

    fn price(url: &str, per_sec: i64, per_unit: i64) -> MintPrice {
        MintPrice {
            mint_url: url.to_string(),
            price_per_second: per_sec,
            price_per_unit: per_unit,
        }
    }

    #[test]
    fn product_id_is_declaration_order_independent() {
        let a = price("https://mint-a.example", 10, 1);
        let b = price("https://mint-b.example", 20, 2);
        let id1 = product_id(DEFAULT_PRICING_SCALE, &[a.clone(), b.clone()], b"");
        let id2 = product_id(DEFAULT_PRICING_SCALE, &[b, a], b"");
        assert_eq!(id1, id2);
    }

    #[test]
    fn product_id_changes_with_pricing() {
        let base = vec![price("https://mint.example", 10, 1)];
        let dearer = vec![price("https://mint.example", 11, 1)];
        assert_ne!(
            product_id(DEFAULT_PRICING_SCALE, &base, b""),
            product_id(DEFAULT_PRICING_SCALE, &dearer, b""),
        );
    }

    #[test]
    fn product_id_changes_with_extensions() {
        let p = vec![price("https://mint.example", 10, 1)];
        assert_ne!(
            product_id(DEFAULT_PRICING_SCALE, &p, b""),
            product_id(DEFAULT_PRICING_SCALE, &p, b"bandwidth=10000"),
        );
    }
}
