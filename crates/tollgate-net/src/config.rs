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
    /// Path to a file holding the hex-encoded secp256k1 secret key. If the file
    /// is absent, a fresh key is generated and written there (mode 0600). If
    /// unset entirely, an ephemeral key is generated and not persisted.
    pub secret_key_file: Option<PathBuf>,
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
            secret_key_file: None,
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
    /// Resolve the node's signing key:
    /// - `secret_key_file` set and present → load it
    /// - `secret_key_file` set but absent → generate, persist to that path, reuse on restart
    /// - `secret_key_file` unset → generate an ephemeral key (not persisted)
    pub fn load_or_generate(cfg: &Config) -> anyhow::Result<Self> {
        let secp = secp256k1::Secp256k1::new();
        let secret_key = match &cfg.secret_key_file {
            Some(path) if path.exists() => {
                let hex_key = std::fs::read_to_string(path)
                    .with_context(|| format!("reading key file {}", path.display()))?;
                let bytes = hex::decode(hex_key.trim()).context("decoding secret key hex")?;
                secp256k1::SecretKey::from_slice(&bytes).context("invalid secret key")?
            }
            Some(path) => {
                let (secret_key, _) = secp.generate_keypair(&mut secp256k1::rand::rngs::OsRng);
                write_key_file(path, &hex::encode(secret_key.secret_bytes()))
                    .with_context(|| format!("writing key file {}", path.display()))?;
                tracing::info!(path = %path.display(), "generated and saved new identity");
                secret_key
            }
            None => {
                tracing::warn!("no secret_key_file configured; using an ephemeral identity");
                secp.generate_keypair(&mut secp256k1::rand::rngs::OsRng).0
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

/// Write the secret key to `path`, owner-read/write only on Unix (mode 0600).
fn write_key_file(path: &Path, hex_key: &str) -> anyhow::Result<()> {
    use std::io::Write;

    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating key dir {}", parent.display()))?;
        }
    }

    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }

    let mut file = opts.open(path)?;
    file.write_all(hex_key.as_bytes())?;
    file.write_all(b"\n")?;
    Ok(())
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

    #[test]
    fn key_file_is_generated_then_reused() {
        // A unique temp path that does not yet exist.
        let mut path = std::env::temp_dir();
        path.push(format!("tollgate-test-key-{}.hex", std::process::id()));
        let _ = std::fs::remove_file(&path);

        let cfg = Config {
            secret_key_file: Some(path.clone()),
            ..Config::default()
        };

        // First load generates and persists the key.
        let first = Identity::load_or_generate(&cfg).unwrap();
        assert!(path.exists());
        // Second load reuses the same key (stable pubkey across restarts).
        let second = Identity::load_or_generate(&cfg).unwrap();
        assert_eq!(first.pubkey_hex(), second.pubkey_hex());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }

        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn no_key_file_yields_ephemeral_identity() {
        let cfg = Config::default();
        // Two ephemeral identities differ (fresh random key each time).
        let a = Identity::load_or_generate(&cfg).unwrap();
        let b = Identity::load_or_generate(&cfg).unwrap();
        assert_ne!(a.pubkey_hex(), b.pubkey_hex());
    }
}
