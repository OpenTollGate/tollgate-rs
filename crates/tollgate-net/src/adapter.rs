//! IP resource adapter — access control via OS firewall, metering via OS counters.
//!
//! Bootstrap-only placeholder. Real nftables/iptables enforcement comes next.

use tollgate_core::AccessLevel;

pub struct IpAdapter;

impl IpAdapter {
    pub fn new() -> Self {
        Self
    }

    pub fn set_access(&self, pubkey_hex: &str, level: AccessLevel) {
        // TODO: install/remove nftables rules per peer.
        tracing::debug!(peer = pubkey_hex, ?level, "set_access (stub)");
    }
}
