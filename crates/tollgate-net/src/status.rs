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

/// A peer's price for delivering resources, signed (`≤0` = free / it pays).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Pricing {
    pub price_per_second: i64,
    pub price_per_unit: i64,
}

/// One peer relationship, as the node sees it. A TollGate peering is inherently
/// **bidirectional** — both nodes meter what they `delivered` and charge for it
/// at their own (signed) price — so a single peer carries both directions. All
/// fields are from *our* perspective.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerStatus {
    pub pubkey: String,
    pub ip: Option<String>,
    /// Access state: `Active`, `Suspended`, `New`, `Cut-off`, …
    pub state: String,
    /// Units we delivered to them this session (we charge for this).
    pub delivered: u64,
    /// Units we received from them this session (they charge for this).
    pub received: u64,
    /// What we charge them for our delivery.
    #[serde(default)]
    pub our_price: Pricing,
    /// What they charge us for their delivery.
    #[serde(default)]
    pub their_price: Pricing,
    /// Their prepaid balance with us (scaled), draining as we deliver. 0 when we
    /// don't sell to them.
    pub their_balance: u64,
    /// Our prepaid balance with them (scaled), draining as they deliver. 0 when
    /// we don't buy from them.
    pub our_balance: u64,
    /// Seconds the metering session has run.
    pub metered_secs: u64,
    /// Milliseconds since the peer was last heard from.
    pub idle_ms: u64,
    /// Metering drift vs the peer's own count, as a fraction (0.02 = 2% transit
    /// loss). `None` until we have both our measurement and the peer's report to
    /// compare — billing uses the higher value, but we surface the discrepancy.
    #[serde(default)]
    pub drift: Option<f64>,
}

impl PeerStatus {
    /// Net balance position with this peer (scaled): `+` we're a net earner
    /// (they've prepaid us more than we've prepaid them), `-` a net spender.
    pub fn net_balance(&self) -> i64 {
        self.their_balance as i64 - self.our_balance as i64
    }
}

impl NodeStatus {
    /// `(active, suspended, other)` peer counts by access state.
    pub fn state_counts(&self) -> (usize, usize, usize) {
        let mut active = 0;
        let mut suspended = 0;
        let mut other = 0;
        for p in &self.peers {
            match p.state.as_str() {
                "Active" => active += 1,
                "Suspended" => suspended += 1,
                _ => other += 1,
            }
        }
        (active, suspended, other)
    }
}

/// Format a net balance (scaled milli-units) in sats with a sign.
pub fn fmt_net(net_scaled: i64) -> String {
    format!("{:+}", net_scaled / 1000)
}

/// Format a drift fraction as a percentage, or `-` when not yet comparable.
pub fn fmt_drift(drift: Option<f64>) -> String {
    match drift {
        Some(d) => format!("{:.1}%", d * 100.0),
        None => "-".to_string(),
    }
}

/// Render the one-shot plain-text table for `tolltop --once`. One row per peer —
/// a peering is bidirectional: DELIVERED is what we delivered to them (we charge),
/// RECEIVED is what they delivered to us (they charge), NET is our net balance
/// (sats; + earner / - spender), DRIFT is metering disagreement vs their report.
pub fn render_table(status: &NodeStatus) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let _ = writeln!(out, "node {}  unit={}", short(&status.pubkey), status.unit);
    let _ = writeln!(
        out,
        "{:<13} {:<19} {:<10} {:>11} {:>11} {:>8} {:>6} {:>7}",
        "PEER", "IP", "STATE", "DELIVERED", "RECEIVED", "NET", "DRIFT", "METERED"
    );
    for p in &status.peers {
        let _ = writeln!(
            out,
            "{:<13} {:<19} {:<10} {:>11} {:>11} {:>8} {:>6} {:>6}s",
            short(&p.pubkey),
            p.ip.as_deref().unwrap_or("-"),
            p.state,
            fmt_units(p.delivered, &status.unit),
            fmt_units(p.received, &status.unit),
            fmt_net(p.net_balance()),
            fmt_drift(p.drift),
            p.metered_secs,
        );
    }
    let (a, s, o) = status.state_counts();
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

/// Format a usage count for display, scaled to the resource unit being sold.
/// For `bytes`, step through B/KB/MB/GB/TB (1024-based) with 2 decimals as the
/// number climbs; any other unit is shown as the raw integer.
pub fn fmt_units(n: u64, unit: &str) -> String {
    if unit != "bytes" {
        return n.to_string();
    }
    const STEP: f64 = 1024.0;
    const SUFFIXES: [&str; 6] = ["B", "KB", "MB", "GB", "TB", "PB"];
    let mut value = n as f64;
    let mut i = 0;
    while value >= STEP && i < SUFFIXES.len() - 1 {
        value /= STEP;
        i += 1;
    }
    if i == 0 {
        format!("{n} B") // whole bytes — no decimals below 1 KB
    } else {
        format!("{value:.2} {}", SUFFIXES[i])
    }
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
                // A customer we sell to (we deliver, they prepay us).
                PeerStatus {
                    pubkey: format!("03{}", "cd".repeat(32)),
                    ip: Some("10.0.0.2".to_string()),
                    state: "Active".to_string(),
                    delivered: 1536, // 1.50 KB delivered to them
                    received: 340,   // 340 B received from them
                    our_price: Pricing {
                        price_per_second: 0,
                        price_per_unit: 1,
                    },
                    their_price: Pricing::default(),
                    their_balance: 6000, // they prepaid us → net earner
                    our_balance: 0,
                    metered_secs: 42,
                    idle_ms: 1500,
                    drift: Some(0.02), // 2% transit loss vs their report
                },
                // A provider we buy from (they deliver, we prepay them).
                PeerStatus {
                    pubkey: format!("04{}", "ef".repeat(32)),
                    ip: None,
                    state: "Suspended".to_string(),
                    delivered: 120,
                    received: 8000,
                    our_price: Pricing::default(),
                    their_price: Pricing {
                        price_per_second: 0,
                        price_per_unit: 1,
                    },
                    their_balance: 0,
                    our_balance: 5000, // we prepaid them → net spender
                    metered_secs: 80,
                    idle_ms: 9000,
                    drift: None,
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
        assert_eq!(back.peers[0].state, "Active");
        assert_eq!(back.peers[0].net_balance(), 6000); // net earner
        assert_eq!(back.peers[0].drift, Some(0.02));
        assert_eq!(back.peers[1].net_balance(), -5000); // net spender
        assert_eq!(back.peers[1].drift, None);
    }

    #[test]
    fn state_counts_and_table_render() {
        let status = sample();
        assert_eq!(status.state_counts(), (1, 1, 0));

        let table = render_table(&status);
        assert!(table.contains("DELIVERED"), "{table}"); // doc terminology
        assert!(table.contains("RECEIVED"), "{table}");
        assert!(table.contains("Active"), "{table}");
        assert!(table.contains("Suspended"), "{table}");
        assert!(table.contains("1.50 KB"), "{table}"); // delivered, byte-scaled
        assert!(table.contains("340 B"), "{table}"); // received, byte-scaled
        assert!(table.contains("+6"), "{table}"); // net balance (earner)
        assert!(table.contains("-5"), "{table}"); // net balance (spender)
        assert!(table.contains("2.0%"), "{table}"); // drift
        assert!(table.contains("42s"), "{table}"); // metering duration
        assert!(
            table.contains("2 peers (1 active, 1 suspended, 0 other)"),
            "{table}"
        );
    }

    #[test]
    fn fmt_units_scales_bytes_and_passes_other_units_through() {
        assert_eq!(fmt_units(0, "bytes"), "0 B");
        assert_eq!(fmt_units(512, "bytes"), "512 B");
        assert_eq!(fmt_units(1024, "bytes"), "1.00 KB");
        assert_eq!(fmt_units(1536, "bytes"), "1.50 KB");
        assert_eq!(fmt_units(1_572_864, "bytes"), "1.50 MB");
        assert_eq!(fmt_units(3_221_225_472, "bytes"), "3.00 GB");
        // Other units are shown raw.
        assert_eq!(fmt_units(2500, "wh"), "2500");
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
