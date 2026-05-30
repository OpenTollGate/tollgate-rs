//! IP resource adapter — access control via the host firewall.
//!
//! Identity is by pubkey, but enforcement is by IP (see
//! `docs/design/network-peering/peering-ip.md`): the driver maps pubkey →
//! source-IP at Announce and calls [`IpAdapter::allow`] / [`IpAdapter::deny`]
//! here as access levels change.
//!
//! On Linux we manage an `inet tollgate` nftables table with two named sets,
//! `paid_peers_v4` / `paid_peers_v6`; set membership means "allowed to
//! forward". On other platforms enforcement is not yet implemented and
//! decisions are logged only (so a node still runs for development).

use std::net::IpAddr;

/// Name of the nftables table TollGate manages.
pub const NFT_TABLE: &str = "tollgate";

pub struct IpAdapter {
    backend: Backend,
}

#[derive(Clone, Copy)]
enum Backend {
    #[cfg(target_os = "linux")]
    Nftables,
    /// No enforcement on this platform — decisions are logged only.
    LogOnly,
}

impl IpAdapter {
    pub fn new() -> Self {
        #[cfg(target_os = "linux")]
        let backend = Backend::Nftables;
        #[cfg(not(target_os = "linux"))]
        let backend = Backend::LogOnly;
        Self { backend }
    }

    /// Create the base table, sets, and enforcing forward chain (idempotent).
    /// Requires root; callers should treat failure as non-fatal and warn.
    pub fn init(&self) -> anyhow::Result<()> {
        match self.backend {
            #[cfg(target_os = "linux")]
            Backend::Nftables => nft::init(),
            Backend::LogOnly => {
                tracing::warn!(
                    "firewall enforcement is not implemented on this platform; \
                     access decisions are logged but NOT enforced"
                );
                Ok(())
            }
        }
    }

    /// Permit the peer at `ip` to forward traffic (add to the paid set).
    pub fn allow(&self, ip: IpAddr) {
        match self.backend {
            #[cfg(target_os = "linux")]
            Backend::Nftables => nft::apply(ip, true),
            Backend::LogOnly => tracing::info!(%ip, "allow (log-only, not enforced)"),
        }
    }

    /// Revoke the peer at `ip` (remove from the paid set).
    pub fn deny(&self, ip: IpAddr) {
        match self.backend {
            #[cfg(target_os = "linux")]
            Backend::Nftables => nft::apply(ip, false),
            Backend::LogOnly => tracing::info!(%ip, "deny (log-only, not enforced)"),
        }
    }
}

/// Build the `nft` argument vector to add/remove `ip` from its paid-peers set.
/// Pure (no I/O) so it can be unit-tested on any platform.
fn nft_element_args(ip: IpAddr, add: bool) -> Vec<String> {
    let (set, addr) = match ip {
        IpAddr::V4(v4) => ("paid_peers_v4", v4.to_string()),
        IpAddr::V6(v6) => ("paid_peers_v6", v6.to_string()),
    };
    let op = if add { "add" } else { "delete" };
    vec![
        op.to_string(),
        "element".to_string(),
        "inet".to_string(),
        NFT_TABLE.to_string(),
        set.to_string(),
        format!("{{ {addr} }}"),
    ]
}

#[cfg(target_os = "linux")]
mod nft {
    use std::process::Command;

    use super::{NFT_TABLE, nft_element_args};

    /// Base ruleset: a dedicated table, the two paid-peer sets, and a forward
    /// chain that drops transit traffic unless it is established or comes
    /// from/to a paid peer. Only the `forward` hook is touched, so the host's
    /// own input/output (management plane) is unaffected.
    pub fn init() -> anyhow::Result<()> {
        let ruleset = format!(
            "add table inet {t}\n\
             add set inet {t} paid_peers_v4 {{ type ipv4_addr; flags interval; }}\n\
             add set inet {t} paid_peers_v6 {{ type ipv6_addr; flags interval; }}\n\
             add chain inet {t} forward {{ type filter hook forward priority 0; policy drop; }}\n\
             add rule inet {t} forward ct state established,related accept\n\
             add rule inet {t} forward ip saddr @paid_peers_v4 accept\n\
             add rule inet {t} forward ip daddr @paid_peers_v4 accept\n\
             add rule inet {t} forward ip6 saddr @paid_peers_v6 accept\n\
             add rule inet {t} forward ip6 daddr @paid_peers_v6 accept\n",
            t = NFT_TABLE
        );

        use std::io::Write;
        let mut child = Command::new("nft")
            .arg("-f")
            .arg("-")
            .stdin(std::process::Stdio::piped())
            .spawn()?;
        child
            .stdin
            .take()
            .ok_or_else(|| anyhow::anyhow!("failed to open nft stdin"))?
            .write_all(ruleset.as_bytes())?;
        let status = child.wait()?;
        if !status.success() {
            anyhow::bail!("nft -f failed with status {status}");
        }
        tracing::info!("nftables table `{NFT_TABLE}` initialized");
        Ok(())
    }

    /// Add or remove `ip` from the paid-peers set.
    pub fn apply(ip: std::net::IpAddr, add: bool) {
        let args = nft_element_args(ip, add);
        match Command::new("nft").args(&args).status() {
            Ok(status) if status.success() => {
                tracing::debug!(%ip, add, "nftables set updated");
            }
            Ok(status) => {
                // A delete of an absent element is benign; an add failure is not.
                if add {
                    tracing::warn!(%ip, %status, "nft add element failed");
                } else {
                    tracing::debug!(%ip, %status, "nft delete element (likely absent)");
                }
            }
            Err(e) => tracing::warn!(%ip, err = %e, "could not run nft"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nft_args_add_ipv4() {
        let args = nft_element_args("10.0.0.5".parse().unwrap(), true);
        assert_eq!(
            args,
            vec![
                "add",
                "element",
                "inet",
                "tollgate",
                "paid_peers_v4",
                "{ 10.0.0.5 }"
            ]
        );
    }

    #[test]
    fn nft_args_delete_ipv4() {
        let args = nft_element_args("10.0.0.12".parse().unwrap(), false);
        assert_eq!(args[0], "delete");
        assert_eq!(args[4], "paid_peers_v4");
        assert_eq!(args[5], "{ 10.0.0.12 }");
    }

    #[test]
    fn nft_args_ipv6_uses_v6_set() {
        let args = nft_element_args("fd00::5".parse().unwrap(), true);
        assert_eq!(args[4], "paid_peers_v6");
        assert_eq!(args[5], "{ fd00::5 }");
    }
}
