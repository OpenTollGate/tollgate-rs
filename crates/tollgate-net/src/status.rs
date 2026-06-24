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
}
