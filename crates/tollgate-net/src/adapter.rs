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
use std::sync::atomic::{AtomicBool, Ordering};

use tollgate_core::metering::Counters;

/// Name of the nftables table TollGate manages.
/// Only referenced by the Linux nftables backend (and its tests).
#[cfg(any(target_os = "linux", test))]
pub const NFT_TABLE: &str = "tollgate";

pub struct IpAdapter {
    backend: Backend,
    /// Whether a forward chain exists to host per-peer counter rules. Set at
    /// `init`; false in sets-only mode, where counters have nowhere to attach.
    counters_available: AtomicBool,
}

#[derive(Clone, Copy)]
enum Backend {
    #[cfg(target_os = "linux")]
    Nftables,
    /// No enforcement on this platform — decisions are logged only. On Linux it
    /// is never constructed (the nftables backend is always selected) but its
    /// match arms must still compile, so the variant stays.
    #[cfg_attr(target_os = "linux", allow(dead_code))]
    LogOnly,
}

impl IpAdapter {
    pub fn new() -> Self {
        #[cfg(target_os = "linux")]
        let backend = Backend::Nftables;
        #[cfg(not(target_os = "linux"))]
        let backend = Backend::LogOnly;
        Self {
            backend,
            counters_available: AtomicBool::new(false),
        }
    }

    /// Create the base table and sets (idempotent). When `install_forward_chain`
    /// is set, also install a `policy drop` forward chain that enforces payment
    /// on its own; otherwise only the sets are managed and the operator wires
    /// `@paid_peers_v4/v6` into their existing firewall. Requires root; callers
    /// should treat failure as non-fatal and warn.
    #[cfg_attr(not(target_os = "linux"), allow(unused_variables))]
    pub fn init(&self, install_forward_chain: bool) -> anyhow::Result<()> {
        // Per-peer counter rules live in the forward chain; without it (sets-only
        // mode) there is nowhere to attach them, so don't try.
        self.counters_available
            .store(install_forward_chain, Ordering::Relaxed);
        match self.backend {
            #[cfg(target_os = "linux")]
            Backend::Nftables => nft::init(install_forward_chain),
            Backend::LogOnly => {
                tracing::warn!(
                    "firewall enforcement is not implemented on this platform; \
                     access decisions are logged but NOT enforced"
                );
                Ok(())
            }
        }
    }

    /// Permit the peer at `ip` to forward traffic (add to the paid set) and,
    /// when a forward chain is present, ensure its per-peer byte counters exist.
    pub fn allow(&self, ip: IpAddr) {
        match self.backend {
            #[cfg(target_os = "linux")]
            Backend::Nftables => {
                nft::apply(ip, true);
                if self.counters_available.load(Ordering::Relaxed) {
                    nft::ensure_counters(ip);
                }
            }
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

    /// Read the cumulative byte counters for the peer at `ip`. Zero on platforms
    /// without enforcement, or when counters haven't been installed (e.g.
    /// `sets-only` mode, where there is no forward chain to attach them to).
    #[cfg_attr(not(target_os = "linux"), allow(unused_variables))]
    pub fn read_counters(&self, ip: IpAddr) -> Counters {
        match self.backend {
            #[cfg(target_os = "linux")]
            Backend::Nftables => nft::read(ip),
            Backend::LogOnly => Counters::default(),
        }
    }
}

/// Read cumulative byte counters for a network interface straight from the
/// kernel's `/sys/class/net/<iface>/statistics` files. Backend-independent (no
/// nftables) — this is the **consumer's** own view of an upstream link, the
/// receive-side measurement it reconciles against the provider's `delivered`:
/// `received` = the link's `rx` (bytes the upstream delivered to us),
/// `delivered` = its `tx` (bytes we sent up).
///
/// Returns zeros if the counters can't be read (non-Linux host, missing iface),
/// so a node without an uplink meter simply has no independent measurement rather
/// than failing. Note this counts *all* bytes on the interface, including the
/// thin TollGate control traffic, so a fully honest reading can sit slightly
/// above the provider's forwarded-only count.
pub fn read_iface_counters(iface: &str) -> Counters {
    let stat = |kind: &str| -> u64 {
        std::fs::read_to_string(format!("/sys/class/net/{iface}/statistics/{kind}_bytes"))
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0)
    };
    Counters {
        delivered: stat("tx"),
        received: stat("rx"),
    }
}

/// Ensure a per-upstream **receive** counter exists, keyed by the upstream's
/// next-hop MAC. Metering a peer we buy from can't go by IP — its forwarded
/// traffic carries the far endpoints' addresses, not the upstream's — so on a
/// shared L2 segment the next-hop MAC is the only per-peer key (see
/// `docs/design/network-peering/peering-ip.md`). Idempotent and lazy: a no-op
/// until the MAC resolves (retried on the next call) and on non-Linux hosts.
pub fn ensure_upstream_counter(ip: IpAddr) {
    #[cfg(target_os = "linux")]
    nft::ensure_upstream_counter(ip);
    #[cfg(not(target_os = "linux"))]
    let _ = ip;
}

/// Cumulative bytes received from the upstream at `ip` (its per-MAC counter).
/// Zero until [`ensure_upstream_counter`] has installed it, or off Linux.
pub fn read_upstream_received(ip: IpAddr) -> u64 {
    #[cfg(target_os = "linux")]
    {
        nft::read_upstream_received(ip)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = ip;
        0
    }
}

/// nftables named-counter identifier for an upstream's received bytes, keyed by
/// IP (the rule that feeds it matches the resolved MAC). Distinct prefix from the
/// per-IP customer counters so the two never collide.
#[cfg(any(target_os = "linux", test))]
fn upstream_counter_name(ip: IpAddr) -> String {
    format!("tg_up_{}", ip.to_string().replace(['.', ':'], "_"))
}

/// Parse the MAC for `ip` from `/proc/net/arp`-format text. Pure, for testing.
/// Columns: `IP address  HW type  Flags  HW address  Mask  Device`. Returns
/// `None` if the IP is absent or its entry is incomplete (all-zero MAC).
#[cfg(any(target_os = "linux", test))]
fn parse_arp_mac(arp_table: &str, ip: &str) -> Option<String> {
    for line in arp_table.lines().skip(1) {
        let mut cols = line.split_whitespace();
        if cols.next() != Some(ip) {
            continue;
        }
        // Skip HW type and Flags; the 3rd remaining column is the HW address.
        let mac = cols.nth(2)?;
        return (mac != "00:00:00:00:00:00").then(|| mac.to_string());
    }
    None
}

/// Build the `nft` argument vector to add/remove `ip` from its paid-peers set.
/// Pure (no I/O) so it can be unit-tested on any platform. Only compiled for the
/// Linux backend and for tests (the macOS/other builds use the log-only path).
#[cfg(any(target_os = "linux", test))]
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

/// nftables named-counter identifiers for a peer: `(delivered, received)`.
/// `.`/`:` in the address are replaced with `_` to form a valid identifier.
#[cfg(any(target_os = "linux", test))]
fn counter_names(ip: IpAddr) -> (String, String) {
    let key = ip.to_string().replace(['.', ':'], "_");
    (format!("tg_d_{key}"), format!("tg_r_{key}"))
}

/// Extract a named counter's byte total from `nft -j list counters` JSON.
/// Pure, so it is unit-tested without nftables.
#[cfg(any(target_os = "linux", test))]
fn parse_counter_bytes(json: &str, name: &str) -> Option<u64> {
    let value: serde_json::Value = serde_json::from_str(json).ok()?;
    let items = value.get("nftables")?.as_array()?;
    for item in items {
        let Some(counter) = item.get("counter") else {
            continue;
        };
        if counter.get("name").and_then(|n| n.as_str()) == Some(name) {
            return counter.get("bytes").and_then(|b| b.as_u64());
        }
    }
    None
}

#[cfg(target_os = "linux")]
mod nft {
    use std::collections::HashSet;
    use std::io::Write;
    use std::net::IpAddr;
    use std::process::Command;
    use std::sync::{LazyLock, Mutex};

    use tollgate_core::metering::Counters;

    use super::{NFT_TABLE, counter_names, nft_element_args, parse_counter_bytes};

    /// Peers whose counter rules have been installed this process-run. The base
    /// table is recreated on every `init`, so this in-memory view stays in sync.
    static INSTALLED: LazyLock<Mutex<HashSet<IpAddr>>> =
        LazyLock::new(|| Mutex::new(HashSet::new()));

    /// Base ruleset: a dedicated table and the two paid-peer sets. When
    /// `install_forward_chain` is set, also a `forward` chain that drops transit
    /// traffic unless it is established or from/to a paid peer. Only the
    /// `forward` hook is touched, so the host's own input/output (management
    /// plane) is unaffected.
    ///
    /// The `add table; delete table; add table` prologue makes this idempotent
    /// across restarts: the table is recreated clean each time, so re-running
    /// never duplicates rules. (Paid peers are re-added as sessions re-establish.)
    pub fn init(install_forward_chain: bool) -> anyhow::Result<()> {
        let mut ruleset = format!(
            "add table inet {t}\n\
             delete table inet {t}\n\
             add table inet {t}\n\
             add set inet {t} paid_peers_v4 {{ type ipv4_addr; flags interval; }}\n\
             add set inet {t} paid_peers_v6 {{ type ipv6_addr; flags interval; }}\n",
            t = NFT_TABLE
        );
        if install_forward_chain {
            ruleset.push_str(&format!(
                "add chain inet {t} forward {{ type filter hook forward priority 0; policy drop; }}\n\
                 add rule inet {t} forward ct state established,related accept\n\
                 add rule inet {t} forward ip saddr @paid_peers_v4 accept\n\
                 add rule inet {t} forward ip daddr @paid_peers_v4 accept\n\
                 add rule inet {t} forward ip6 saddr @paid_peers_v6 accept\n\
                 add rule inet {t} forward ip6 daddr @paid_peers_v6 accept\n",
                t = NFT_TABLE
            ));
        }

        run_batch(&ruleset)?;
        let mode = if install_forward_chain {
            "enforcing"
        } else {
            "sets-only"
        };
        tracing::info!("nftables table `{NFT_TABLE}` initialized ({mode})");
        Ok(())
    }

    /// Add or remove `ip` from the paid-peers set. `nft` output is captured (not
    /// inherited) so a benign "element does not exist" on delete doesn't leak to
    /// our own stdout/stderr.
    pub fn apply(ip: std::net::IpAddr, add: bool) {
        let args = nft_element_args(ip, add);
        match Command::new("nft").args(&args).output() {
            Ok(out) if out.status.success() => {
                tracing::debug!(%ip, add, "nftables set updated");
            }
            Ok(out) => {
                let stderr = String::from_utf8_lossy(&out.stderr);
                // A delete of an absent element is benign (e.g. revoking a peer
                // that was never granted); an add failure is not.
                if add {
                    tracing::warn!(%ip, status = %out.status, %stderr, "nft add element failed");
                } else {
                    tracing::debug!(%ip, "nft delete element (likely absent)");
                }
            }
            Err(e) => tracing::warn!(%ip, err = %e, "could not run nft"),
        }
    }

    /// Install per-peer byte counters and the counter rules that feed them, once
    /// per peer per process-run. The counter rules are `insert`ed at the top of
    /// the forward chain so they run before the accept verdicts. No-op in
    /// `sets-only` mode is expected to fail (no forward chain) — logged, retried.
    pub fn ensure_counters(ip: IpAddr) {
        {
            let mut installed = INSTALLED.lock().unwrap_or_else(|e| e.into_inner());
            if !installed.insert(ip) {
                return;
            }
        }
        let (delivered, received) = counter_names(ip);
        let (proto, addr) = match ip {
            IpAddr::V4(a) => ("ip", a.to_string()),
            IpAddr::V6(a) => ("ip6", a.to_string()),
        };
        let batch = format!(
            "add counter inet {t} {delivered}\n\
             add counter inet {t} {received}\n\
             insert rule inet {t} forward {proto} daddr {addr} counter name {delivered}\n\
             insert rule inet {t} forward {proto} saddr {addr} counter name {received}\n",
            t = NFT_TABLE
        );
        if let Err(e) = run_batch(&batch) {
            tracing::warn!(%ip, err = %e, "could not install peer counters");
            // Allow a retry on the next allow().
            INSTALLED
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&ip);
        }
    }

    /// Read a peer's cumulative `(delivered, received)` byte counters.
    pub fn read(ip: IpAddr) -> Counters {
        let (delivered_name, received_name) = counter_names(ip);
        let output = Command::new("nft")
            .args(["-j", "list", "counters", "table", "inet", NFT_TABLE])
            .output();
        let json = match output {
            Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).into_owned(),
            Ok(o) => {
                tracing::debug!(status = %o.status, "nft list counters failed");
                return Counters::default();
            }
            Err(e) => {
                tracing::warn!(err = %e, "could not run nft list counters");
                return Counters::default();
            }
        };
        Counters {
            delivered: parse_counter_bytes(&json, &delivered_name).unwrap_or(0),
            received: parse_counter_bytes(&json, &received_name).unwrap_or(0),
        }
    }

    /// Upstreams whose MAC counter rule has been installed this process-run.
    static INSTALLED_UP: LazyLock<Mutex<HashSet<IpAddr>>> =
        LazyLock::new(|| Mutex::new(HashSet::new()));

    /// Resolve `ip`'s next-hop MAC from the kernel neighbour table
    /// (`/proc/net/arp`; IPv4 only — IPv6 upstreams are future work).
    fn resolve_mac(ip: IpAddr) -> Option<String> {
        let table = std::fs::read_to_string("/proc/net/arp").ok()?;
        super::parse_arp_mac(&table, &ip.to_string())
    }

    /// Install the per-upstream receive counter and the `ether saddr <mac>` rule
    /// that feeds it, once the MAC is known. Idempotent — safe to call every poll;
    /// a no-op until the neighbour entry resolves. The table may already exist (a
    /// serving node) or not (a bare consumer), so every line is an idempotent
    /// `add`; a low-priority prerouting chain sees ingress frames with their L2
    /// source intact.
    pub fn ensure_upstream_counter(ip: IpAddr) {
        if INSTALLED_UP
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(&ip)
        {
            return;
        }
        let Some(mac) = resolve_mac(ip) else {
            return; // MAC not resolved yet — retry on the next call
        };
        let name = super::upstream_counter_name(ip);
        let batch = format!(
            "add table inet {t}\n\
             add chain inet {t} ingress {{ type filter hook prerouting priority -300; }}\n\
             add counter inet {t} {name}\n\
             insert rule inet {t} ingress ether saddr {mac} counter name {name}\n",
            t = NFT_TABLE
        );
        if let Err(e) = run_batch(&batch) {
            tracing::warn!(%ip, %mac, err = %e, "could not install upstream MAC counter");
            return;
        }
        INSTALLED_UP
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(ip);
        tracing::info!(%ip, %mac, "metering upstream by next-hop MAC");
    }

    /// Read the upstream's cumulative received-bytes counter (`0` until installed).
    pub fn read_upstream_received(ip: IpAddr) -> u64 {
        let name = super::upstream_counter_name(ip);
        match Command::new("nft")
            .args(["-j", "list", "counters", "table", "inet", NFT_TABLE])
            .output()
        {
            Ok(o) if o.status.success() => {
                parse_counter_bytes(&String::from_utf8_lossy(&o.stdout), &name).unwrap_or(0)
            }
            _ => 0,
        }
    }

    /// Run an `nft -f -` batch from a ruleset string. Output is captured so
    /// nft's diagnostics never leak to our own stdout/stderr; on failure the
    /// captured stderr is included in the error.
    fn run_batch(ruleset: &str) -> anyhow::Result<()> {
        let mut child = Command::new("nft")
            .arg("-f")
            .arg("-")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()?;
        child
            .stdin
            .take()
            .ok_or_else(|| anyhow::anyhow!("failed to open nft stdin"))?
            .write_all(ruleset.as_bytes())?;
        let out = child.wait_with_output()?;
        if !out.status.success() {
            anyhow::bail!(
                "nft batch failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(())
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

    #[test]
    fn counter_names_sanitize_address() {
        let (d, r) = counter_names("10.0.0.5".parse().unwrap());
        assert_eq!(d, "tg_d_10_0_0_5");
        assert_eq!(r, "tg_r_10_0_0_5");
        let (d6, _) = counter_names("fd00::5".parse().unwrap());
        assert_eq!(d6, "tg_d_fd00__5");
    }

    #[test]
    fn upstream_counter_name_has_distinct_prefix() {
        assert_eq!(
            upstream_counter_name("10.0.0.10".parse().unwrap()),
            "tg_up_10_0_0_10"
        );
        // Distinct from the per-IP customer counters so they never collide.
        let (d, _) = counter_names("10.0.0.10".parse().unwrap());
        assert_ne!(upstream_counter_name("10.0.0.10".parse().unwrap()), d);
    }

    #[test]
    fn parse_arp_mac_extracts_resolved_mac() {
        // Real `/proc/net/arp` layout: header row then space-separated columns.
        let arp = "IP address       HW type     Flags       HW address            Mask     Device\n\
                   172.30.0.10      0x1         0x2         52:54:00:aa:bb:cc     *        eth0\n\
                   172.30.0.99      0x1         0x0         00:00:00:00:00:00     *        eth0\n";
        assert_eq!(
            parse_arp_mac(arp, "172.30.0.10").as_deref(),
            Some("52:54:00:aa:bb:cc")
        );
        // Incomplete entry (all-zero MAC) is treated as unresolved.
        assert_eq!(parse_arp_mac(arp, "172.30.0.99"), None);
        // Absent IP.
        assert_eq!(parse_arp_mac(arp, "10.1.2.3"), None);
    }

    #[test]
    fn parse_counter_bytes_finds_named_counter() {
        let json = r#"{
            "nftables": [
                { "metainfo": { "version": "1.0.9" } },
                { "counter": { "family": "inet", "table": "tollgate",
                               "name": "tg_d_10_0_0_5", "packets": 3, "bytes": 1500 } },
                { "counter": { "family": "inet", "table": "tollgate",
                               "name": "tg_r_10_0_0_5", "packets": 2, "bytes": 800 } }
            ]
        }"#;
        assert_eq!(parse_counter_bytes(json, "tg_d_10_0_0_5"), Some(1500));
        assert_eq!(parse_counter_bytes(json, "tg_r_10_0_0_5"), Some(800));
        assert_eq!(parse_counter_bytes(json, "tg_d_missing"), None);
    }
}
