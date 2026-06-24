//! Node status snapshot: the serializable view served on the control socket and
//! rendered by `tollgate status` / `tollgate top`.

use serde::{Deserialize, Serialize};

/// A point-in-time snapshot of the node and the peers it tracks.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeStatus {
    /// This node's compressed pubkey, hex.
    pub pubkey: String,
    /// Resource unit metered ("bytes", "wh", …).
    pub unit: String,
    pub peers: Vec<PeerStatus>,
    /// What this node advertises (its own PriceSheet). `default` so a snapshot
    /// from an older node without pricing still deserializes.
    #[serde(default)]
    pub pricing: PricingStatus,
}

/// The node's advertised pricing — a flattened, serde-friendly view of its
/// PriceSheet (products × mint options + the metering interval range).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PricingStatus {
    pub products: Vec<ProductStatus>,
    pub min_interval_ms: u32,
    pub max_interval_ms: u32,
}

/// One advertised product: its id and the mints it can be paid through.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProductStatus {
    /// Canonical product id, hex.
    pub product_id: String,
    pub pricing_scale: u32,
    pub mints: Vec<MintStatus>,
}

/// One mint option's price within a product.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MintStatus {
    pub mint_url: String,
    pub mint_unit: String,
    pub price_per_second: i64,
    pub price_per_unit: i64,
}

/// One peer's state as the node sees it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerStatus {
    pub pubkey: String,
    pub ip: Option<String>,
    /// Lifecycle phase: `New`, `BootstrapPending`, `Active`, `Suspended`, `Closed`.
    pub phase: String,
    /// Scaled milli-unit balance.
    pub balance: u64,
    pub delivered: u64,
    pub received: u64,
    /// Whether the firewall currently allows delivery (true for `Active`).
    pub allowed: bool,
    /// Whether the metering loop is sampling this peer.
    pub metered: bool,
    /// Milliseconds since the peer was last heard from.
    pub idle_ms: u64,
}

impl NodeStatus {
    /// `(active, suspended, other)` peer counts.
    pub fn phase_counts(&self) -> (usize, usize, usize) {
        let mut active = 0;
        let mut suspended = 0;
        let mut other = 0;
        for p in &self.peers {
            match p.phase.as_str() {
                "Active" => active += 1,
                "Suspended" => suspended += 1,
                _ => other += 1,
            }
        }
        (active, suspended, other)
    }
}

/// Render the one-shot plain-text table for `tollgate status`.
pub fn render_table(status: &NodeStatus) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let _ = writeln!(out, "node {}  unit={}", short(&status.pubkey), status.unit);
    let _ = writeln!(
        out,
        "{:<13} {:<15} {:<10} {:>9} {:>8} {:>12} {:>6}",
        "PEER", "IP", "PHASE", "BALANCE", "ACCESS", "DELIVERED", "IDLE"
    );
    for p in &status.peers {
        let _ = writeln!(
            out,
            "{:<13} {:<15} {:<10} {:>9} {:>8} {:>12} {:>5}s",
            short(&p.pubkey),
            p.ip.as_deref().unwrap_or("-"),
            p.phase,
            p.balance,
            if p.allowed { "allowed" } else { "blocked" },
            p.delivered,
            p.idle_ms / 1000,
        );
    }
    let (a, s, o) = status.phase_counts();
    let _ = write!(
        out,
        "{} peers ({} active, {} suspended, {} other)",
        status.peers.len(),
        a,
        s,
        o
    );
    out
}

/// Render the node's advertised pricing as a plain-text table (for `--once`).
///
/// Prices are *per unit of the resource we sell* (`status.unit`, e.g. bytes) and
/// *per second*, denominated in each mint option's currency (`CCY`, e.g. sat).
pub fn render_pricing(status: &NodeStatus) -> String {
    use std::fmt::Write;
    let p = &status.pricing;
    let mut out = String::new();
    let _ = writeln!(
        out,
        "pricing — selling {}  ·  {} product(s), interval {}–{} ms",
        status.unit,
        p.products.len(),
        p.min_interval_ms,
        p.max_interval_ms
    );
    if p.products.is_empty() {
        let _ = write!(out, "(no products — this node sells nothing)");
        return out;
    }
    let _ = writeln!(
        out,
        "{:<13} {:<26} {:<5} {:>8} {:>9}",
        "PRODUCT", "MINT", "CCY", "PER_SEC", "PER_UNIT"
    );
    for product in &p.products {
        if product.mints.is_empty() {
            let _ = writeln!(out, "{:<13} (no mints)", short(&product.product_id));
            continue;
        }
        for m in &product.mints {
            let _ = writeln!(
                out,
                "{:<13} {:<26} {:<5} {:>8} {:>9}",
                short(&product.product_id),
                m.mint_url,
                m.mint_unit,
                m.price_per_second,
                m.price_per_unit,
            );
        }
    }
    let total: usize = p.products.iter().map(|pr| pr.mints.len()).sum();
    let _ = write!(out, "{total} mint option(s)");
    out
}

/// Abbreviate a long hex pubkey to `123456…cdef` for display.
pub fn short(hex: &str) -> String {
    if hex.len() > 12 {
        format!("{}…{}", &hex[..6], &hex[hex.len() - 4..])
    } else {
        hex.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> NodeStatus {
        NodeStatus {
            pubkey: format!("02{}", "ab".repeat(32)),
            unit: "bytes".to_string(),
            peers: vec![
                PeerStatus {
                    pubkey: format!("03{}", "cd".repeat(32)),
                    ip: Some("10.0.0.2".to_string()),
                    phase: "Active".to_string(),
                    balance: 6000,
                    delivered: 1200,
                    received: 0,
                    allowed: true,
                    metered: true,
                    idle_ms: 1500,
                },
                PeerStatus {
                    pubkey: format!("04{}", "ef".repeat(32)),
                    ip: None,
                    phase: "Suspended".to_string(),
                    balance: 0,
                    delivered: 8000,
                    received: 0,
                    allowed: false,
                    metered: false,
                    idle_ms: 9000,
                },
            ],
            pricing: PricingStatus {
                products: vec![ProductStatus {
                    product_id: format!("aa{}", "bb".repeat(31)),
                    pricing_scale: 1000,
                    mints: vec![MintStatus {
                        mint_url: "http://mint:3338".to_string(),
                        mint_unit: "sat".to_string(),
                        price_per_second: 0,
                        price_per_unit: 1,
                    }],
                }],
                min_interval_ms: 5000,
                max_interval_ms: 60000,
            },
        }
    }

    #[test]
    fn json_round_trips_on_a_single_line() {
        let status = sample();
        let json = serde_json::to_string(&status).unwrap();
        // Single-line JSON is required: the control socket frames one per line.
        assert!(!json.contains('\n'));
        let back: NodeStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(back.peers.len(), 2);
        assert_eq!(back.peers[0].phase, "Active");
        assert!(!back.peers[1].allowed);
    }

    #[test]
    fn phase_counts_and_table_render() {
        let status = sample();
        assert_eq!(status.phase_counts(), (1, 1, 0));

        let table = render_table(&status);
        assert!(table.contains("allowed"), "{table}");
        assert!(table.contains("blocked"), "{table}");
        assert!(
            table.contains("2 peers (1 active, 1 suspended, 0 other)"),
            "{table}"
        );
    }

    #[test]
    fn short_abbreviates_long_hex_and_passes_short_through() {
        assert_eq!(short(&format!("02{}", "ab".repeat(32))), "02abab…abab");
        assert_eq!(short("0203"), "0203");
    }

    #[test]
    fn render_pricing_lists_products_or_says_empty() {
        let status = sample();
        let out = render_pricing(&status);
        assert!(out.contains("selling bytes"), "{out}"); // the resource we sell
        assert!(out.contains("1 product(s)"), "{out}");
        assert!(out.contains("http://mint:3338"), "{out}");
        assert!(out.contains("interval 5000–60000 ms"), "{out}");
        assert!(out.contains("1 mint option(s)"), "{out}");

        let mut empty = sample();
        empty.pricing = PricingStatus::default();
        assert!(render_pricing(&empty).contains("no products"));
    }
}
