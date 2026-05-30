//! Node configuration and identity.
//!
//! A subset of `docs/design/core/tollgate-configuration.md` — enough for the
//! bootstrap-only IP deployment. Loaded from an explicit `--config` path or the
//! standard cascade.

use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::Deserialize;

/// Node configuration.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Config {
    /// hex-encoded secp256k1 secret key. Omit to generate an ephemeral one.
    pub secret_key: Option<String>,
    /// Listen address for the HTTP/WS transport (TollGate default port is 4747).
    pub listen: String,
    /// Mints whose tokens this node accepts.
    pub mints: Vec<String>,
    /// Products this node sells. Static pricing only in v1.
    pub products: Vec<ProductConfig>,
    /// How TollGate manages the host firewall.
    pub firewall: FirewallMode,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            secret_key: None,
            listen: "127.0.0.1:4747".to_string(),
            mints: Vec::new(),
            products: Vec::new(),
            firewall: FirewallMode::default(),
        }
    }
}

/// How `tollgate-net` manages the host firewall.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FirewallMode {
    /// Install a `policy drop` forward chain that enforces payment on its own.
    /// Correct for a dedicated TollGate gateway. (Default.)
    #[default]
    Enforcing,
    /// Only manage the `paid_peers` sets; do not install a forward chain. Use on
    /// a box that already has a firewall — reference `@paid_peers_v4/v6` from
    /// your own ruleset. Without that wiring, access is tracked but NOT enforced.
    SetsOnly,
}

impl FirewallMode {
    /// Whether `init` should install the enforcing forward chain.
    pub fn installs_forward_chain(self) -> bool {
        matches!(self, FirewallMode::Enforcing)
    }
}

/// A single static product offer.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct ProductConfig {
    pub pricing_scale: u32,
    pub price_per_second: i64,
    pub price_per_unit: i64,
}

impl Config {
    /// Load from `explicit`, or search `./tollgate.yaml`,
    /// `~/.config/tollgate/tollgate.yaml`, `/etc/tollgate/tollgate.yaml`.
    /// A missing file yields defaults.
    pub fn load(explicit: Option<&Path>) -> anyhow::Result<Self> {
        let path = explicit.map(PathBuf::from).or_else(Self::search_paths);
        match path {
            Some(p) => {
                let text = std::fs::read_to_string(&p)
                    .with_context(|| format!("reading config {}", p.display()))?;
                let cfg: Config = serde_yaml::from_str(&text)
                    .with_context(|| format!("parsing config {}", p.display()))?;
                tracing::info!(path = %p.display(), "loaded config");
                Ok(cfg)
            }
            None => {
                tracing::info!("no config file found; using defaults");
                Ok(Config::default())
            }
        }
    }

    fn search_paths() -> Option<PathBuf> {
        let mut candidates = vec![PathBuf::from("tollgate.yaml")];
        if let Some(dir) = dirs::config_dir() {
            candidates.push(dir.join("tollgate/tollgate.yaml"));
        }
        candidates.push(PathBuf::from("/etc/tollgate/tollgate.yaml"));
        candidates.into_iter().find(|p| p.exists())
    }
}

/// The node's signing identity (a secp256k1 keypair).
pub struct Identity {
    pub public_key: secp256k1::PublicKey,
    #[allow(dead_code)]
    secret_key: secp256k1::SecretKey,
}

impl Identity {
    /// Load the configured key, or generate an ephemeral one if none is set.
    pub fn load_or_generate(cfg: &Config) -> anyhow::Result<Self> {
        let secp = secp256k1::Secp256k1::new();
        let secret_key = match &cfg.secret_key {
            Some(hex_key) => {
                let bytes = hex::decode(hex_key).context("decoding secret_key hex")?;
                secp256k1::SecretKey::from_slice(&bytes).context("invalid secret_key")?
            }
            None => {
                let (secret_key, _) = secp.generate_keypair(&mut secp256k1::rand::rngs::OsRng);
                secret_key
            }
        };
        let public_key = secret_key.public_key(&secp);
        Ok(Self {
            public_key,
            secret_key,
        })
    }

    /// The compressed public key as lowercase hex (33 bytes → 66 chars).
    pub fn pubkey_hex(&self) -> String {
        hex::encode(self.public_key.serialize())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn firewall_defaults_to_enforcing() {
        let cfg: Config = serde_yaml::from_str("{}").unwrap();
        assert_eq!(cfg.firewall, FirewallMode::Enforcing);
        assert!(cfg.firewall.installs_forward_chain());
    }

    #[test]
    fn firewall_sets_only_parses_and_disables_chain() {
        let cfg: Config = serde_yaml::from_str("firewall: sets-only").unwrap();
        assert_eq!(cfg.firewall, FirewallMode::SetsOnly);
        assert!(!cfg.firewall.installs_forward_chain());
    }
}
