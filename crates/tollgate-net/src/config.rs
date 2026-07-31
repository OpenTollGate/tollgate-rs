//! The YAML an operator writes, and how it becomes a [`NodeConfig`].
//!
//! Every parameter has a default, so a minimal file only says what differs.
//! Delivery has no price to configure: one voucher buys one unit, and what a
//! unit costs in money is decided where vouchers are sold.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use tollgate_core::buyer::BuyerPolicy;
use tollgate_core::config::{GrantPolicy, NodePolicy, PeerPolicy};
use tollgate_protocol::{DEFAULT_PORT, PubKey};

use crate::identity::Identity;
use crate::node::{NodeConfig, PeerConfig};

/// The whole configuration file.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct File {
    /// Node identity.
    pub identity: IdentitySection,
    /// This node's own mint, and the unit it denominates in.
    pub mint: MintSection,
    /// Which mints this node takes payment in, and the default surcharge.
    pub vouchers: VouchersSection,
    /// The minimum flow allowance.
    pub access: AccessSection,
    /// Channel parameters.
    pub channels: ChannelsSection,
    /// Bounds on what a payer may buy in one purchase.
    pub grants: GrantsSection,
    /// How this node buys from its peers.
    pub buying: BuyingSection,
    /// Where to listen.
    pub network: NetworkSection,
    /// Per-peer overrides, keyed by hex-encoded compressed pubkey.
    pub peers: BTreeMap<String, PeerSection>,
}

/// Node identity.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct IdentitySection {
    /// 32-byte secp256k1 secret key in hex. Generated if absent.
    pub secret_key: Option<String>,
}

/// This node's own mint.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct MintSection {
    /// Mint URL advertised to peers.
    pub url: String,
    /// Quantity unit. Fixed by the resource and identical across every node
    /// selling it.
    pub unit: String,
}

impl Default for MintSection {
    fn default() -> Self {
        Self {
            url: "http://127.0.0.1:3338".into(),
            unit: "byte".into(),
        }
    }
}

/// Which mints this node will take payment in.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct VouchersSection {
    /// Most preferred first, at least one. Defaults to this node's own mint.
    pub accepted_mints: Vec<String>,
    /// Unsigned surcharge on what a peer pushes at us. The net rate is `m - 1`,
    /// so `2` charges an upload like a download and `k + 1` charges it `k`
    /// times.
    pub received_multiplier: u16,
}

/// The minimum flow allowance.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct AccessSection {
    /// Traffic every peer gets without paying.
    pub minimum_flow: MinimumFlow,
}

/// Traffic given away.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct MinimumFlow {
    /// Off by default: it is traffic given away, and it can be farmed.
    pub enabled: bool,
    /// Keep it small. Being a rate, it cannot be accumulated.
    pub bytes_per_second: u64,
}

/// Channel parameters.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct ChannelsSection {
    /// Units of capacity a new outgoing channel opens with.
    pub initial_capacity: u64,
    /// Percentage of capacity at which the funder starts a rollover.
    pub rollover_threshold_pct: u8,
}

impl Default for ChannelsSection {
    fn default() -> Self {
        Self {
            initial_capacity: 1_000_000_000,
            rollover_threshold_pct: 80,
        }
    }
}

/// What this node will accept when a peer buys capacity.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct GrantsSection {
    /// `[min, max]` window in milliseconds. The payer picks any window in this
    /// range, per grant, without negotiating.
    pub window_range_ms: [u32; 2],
    /// Units per second this node will commit to one peer. Absent means the
    /// link is the only limit.
    pub max_rate: Option<u64>,
}

impl Default for GrantsSection {
    fn default() -> Self {
        Self {
            window_range_ms: [200, 30_000],
            max_rate: None,
        }
    }
}

/// How this node buys.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct BuyingSection {
    /// Buy this percentage of observed demand.
    pub headroom_pct: u32,
    /// Only jump mid-window if the target exceeds the rate in force by this
    /// percentage. This hysteresis is what keeps forfeiture affordable.
    pub raise_threshold_pct: u32,
    /// Renew this long before the deadline.
    pub renew_lead_ms: u32,
    /// Window to ask for, clamped to what the provider advertises.
    pub window_ms: u32,
    /// Never buy below this rate.
    pub min_rate: u64,
    /// Never buy above this rate — the operator's spending ceiling.
    pub max_rate: u64,
}

impl Default for BuyingSection {
    fn default() -> Self {
        let d = BuyerPolicy::default();
        Self {
            headroom_pct: d.headroom_pct,
            raise_threshold_pct: d.raise_threshold_pct,
            renew_lead_ms: d.renew_lead_ms,
            window_ms: d.window_ms,
            min_rate: d.min_rate,
            max_rate: d.max_rate,
        }
    }
}

/// Where to listen.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct NetworkSection {
    /// Control-plane listen address. The data plane is the next port up.
    pub listen: String,
}

impl Default for NetworkSection {
    fn default() -> Self {
        Self {
            listen: format!("0.0.0.0:{DEFAULT_PORT}"),
        }
    }
}

/// Per-peer overrides.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct PeerSection {
    /// Do not charge this peer. One-sided, and not transitive.
    pub no_charge: bool,
    /// Refuse this peer entirely.
    pub blocked: bool,
    /// Override the node-wide received multiplier.
    pub received_multiplier: Option<u16>,
    /// Static endpoint to dial. Peers without one have to dial us.
    pub endpoint: Option<String>,
}

impl File {
    /// Read a configuration file.
    pub fn load(path: &Path) -> Result<Self> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        serde_yaml::from_str(&text).with_context(|| format!("parse {}", path.display()))
    }

    /// Resolve into what the node actually runs on.
    pub fn resolve(&self) -> Result<NodeConfig> {
        let identity = match &self.identity.secret_key {
            Some(hex) => Identity::from_hex(hex)?,
            None => Identity::generate(),
        };

        // An Offer with no mints is malformed, and a node that names none has
        // simply not said which of its own it means.
        let accepted_mints = if self.vouchers.accepted_mints.is_empty() {
            vec![self.mint.url.clone()]
        } else {
            self.vouchers.accepted_mints.clone()
        };

        let [min_window_ms, max_window_ms] = self.grants.window_range_ms;
        if min_window_ms == 0 || min_window_ms > max_window_ms {
            bail!("grants.window_range_ms must be a non-empty range starting above zero");
        }

        let policy = NodePolicy {
            unit: self.mint.unit.clone(),
            accepted_mints,
            received_multiplier: self.vouchers.received_multiplier,
            minimum_flow: if self.access.minimum_flow.enabled {
                self.access.minimum_flow.bytes_per_second
            } else {
                0
            },
            grants: GrantPolicy {
                min_window_ms,
                max_window_ms,
                max_rate: self.grants.max_rate,
            },
            initial_channel_capacity: self.channels.initial_capacity,
            rollover_threshold_pct: self.channels.rollover_threshold_pct,
        };

        let buyer = BuyerPolicy {
            headroom_pct: self.buying.headroom_pct,
            raise_threshold_pct: self.buying.raise_threshold_pct,
            renew_lead_ms: self.buying.renew_lead_ms,
            window_ms: self.buying.window_ms,
            min_rate: self.buying.min_rate,
            max_rate: self.buying.max_rate,
        };

        let listen: SocketAddr = self.network.listen.parse().with_context(|| {
            format!("network.listen {:?} is not an address", self.network.listen)
        })?;

        let mut peers = Vec::new();
        for (key, section) in &self.peers {
            let pubkey = parse_pubkey(key)?;
            let policy = PeerPolicy {
                no_charge: section.no_charge,
                blocked: section.blocked,
                received_multiplier: section.received_multiplier,
            };
            // A peer with no endpoint is one that dials us; we still hold its
            // policy, we just never reach out.
            if let Some(endpoint) = &section.endpoint {
                peers.push(PeerConfig {
                    pubkey,
                    endpoint: endpoint.clone(),
                    policy,
                });
            }
        }

        Ok(NodeConfig {
            identity,
            policy,
            buyer,
            listen,
            peers,
        })
    }
}

/// Parse a hex-encoded compressed public key.
fn parse_pubkey(s: &str) -> Result<PubKey> {
    let bytes = hex::decode(s).with_context(|| format!("peer key {s:?} is not hex"))?;
    let bytes: [u8; 33] = bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("peer key {s:?} is not 33 bytes"))?;
    Ok(PubKey(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_file_resolves_to_working_defaults() {
        let file: File = serde_yaml::from_str("{}").expect("parse");
        let config = file.resolve().expect("resolve");

        assert_eq!(config.policy.unit, "byte");
        assert_eq!(config.policy.accepted_mints.len(), 1, "own mint by default");
        assert_eq!(
            config.policy.minimum_flow, 0,
            "the allowance is off by default"
        );
        assert_eq!(config.listen.port(), DEFAULT_PORT);
    }

    #[test]
    fn the_allowance_is_zero_unless_it_is_switched_on() {
        // A rate configured but not enabled is a trap worth closing: it reads as
        // "4 KiB/s free" but must mean nothing until `enabled` is set.
        let file: File = serde_yaml::from_str(
            "access:\n  minimum_flow:\n    enabled: false\n    bytes_per_second: 4096\n",
        )
        .expect("parse");
        assert_eq!(file.resolve().expect("resolve").policy.minimum_flow, 0);
    }

    #[test]
    fn an_inverted_window_range_is_rejected() {
        let file: File =
            serde_yaml::from_str("grants:\n  window_range_ms: [30000, 200]\n").expect("parse");
        assert!(file.resolve().is_err());
    }

    #[test]
    fn a_peer_without_an_endpoint_is_not_dialled() {
        let key = "02".to_string() + &"11".repeat(32);
        let yaml = format!("peers:\n  \"{key}\":\n    no_charge: true\n");
        let file: File = serde_yaml::from_str(&yaml).expect("parse");
        assert!(file.resolve().expect("resolve").peers.is_empty());
    }

    #[test]
    fn a_peer_with_an_endpoint_is_dialled() {
        let key = "02".to_string() + &"11".repeat(32);
        let yaml = format!("peers:\n  \"{key}\":\n    endpoint: \"10.0.0.1:4747\"\n");
        let file: File = serde_yaml::from_str(&yaml).expect("parse");
        let peers = file.resolve().expect("resolve").peers;
        assert_eq!(peers.len(), 1);
        assert_eq!(peers[0].endpoint, "10.0.0.1:4747");
    }

    #[test]
    fn a_misspelt_key_is_an_error_rather_than_silently_ignored() {
        // `deny_unknown_fields` is what stops `recieved_multiplier` from
        // quietly meaning "default".
        assert!(serde_yaml::from_str::<File>("vouchers:\n  recieved_multiplier: 2\n").is_err());
    }
}
