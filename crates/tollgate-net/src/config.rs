//! Node configuration and identity.
//!
//! A subset of `docs/design/core/tollgate-configuration.md` — enough for the
//! bootstrap-only IP deployment. Loaded from an explicit `--config` path or the
//! standard cascade.

use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::Deserialize;
use tollgate_protocol::{DEFAULT_PRICING_SCALE, MintPrice, PriceSheet, ProductOffer};

/// Metering interval range advertised in the PriceSheet (ms). The server meters
/// every 5s; the range is what a peer may negotiate to once interval handling
/// lands (not configurable yet).
const DEFAULT_MIN_INTERVAL_MS: u32 = 5_000;
const DEFAULT_MAX_INTERVAL_MS: u32 = 60_000;

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
    /// Resource unit this node meters, advertised in Announce ("bytes", "wh", …).
    pub unit: String,
    /// Path to the Unix control socket served for `tolltop` / status tooling.
    pub control_socket: PathBuf,
    /// How often (seconds) to sample meters / charge peers / send MeteringReports.
    /// Lower = finer-grained reports (and faster tests); the drain *rate* is
    /// unchanged (cost is per elapsed second). Clamped to ≥1.
    pub metering_interval_secs: u64,
    /// Upstream peers this node buys from. On `serve`, it connects, pays, and
    /// auto-tops-up each, tracking them as upstream peers (the mesh's inbound
    /// direction — what `tolltop` shows with a `↑` direction).
    pub upstreams: Vec<UpstreamConfig>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            secret_key_file: None,
            listen: "127.0.0.1:4747".to_string(),
            mints: Vec::new(),
            products: Vec::new(),
            firewall: FirewallMode::default(),
            unit: "bytes".to_string(),
            control_socket: PathBuf::from("/tmp/tollgate.sock"),
            metering_interval_secs: 5,
            upstreams: Vec::new(),
        }
    }
}

/// How `tollgate-net` manages the host firewall.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FirewallMode {
    /// Install a `policy drop` forward chain that enforces payment on its own.
    /// Correct for a dedicated TollGate node. (Default.)
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

/// An upstream peer this node *buys* from — it connects, pays, and auto-tops-up,
/// tracking the relationship like any other peer (the inbound/mesh direction).
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct UpstreamConfig {
    /// Peer HTTP origin, e.g. `http://peer:4747`.
    pub peer: String,
    /// Mint to draw bootstrap tokens on.
    pub mint: String,
    /// Initial token amount in sats.
    pub amount: u64,
    /// Top-up amount in sats (also the low-balance watermark).
    pub topup: u64,
    /// Seconds between polls for MeteringReports.
    pub interval_secs: u64,
    /// Uplink interface facing this upstream (e.g. `eth0`) to meter for an
    /// independent receive-side count. When set, we report the bytes we actually
    /// received over the link instead of echoing the provider's `delivered`,
    /// surfacing real transit drift. Absent = acknowledge the provider's count.
    /// Only correct when this upstream owns the interface; for shared links set
    /// `meter_upstream`.
    pub meter_iface: Option<String>,
    /// Meter this upstream by its next-hop MAC (a per-peer nftables counter) for an
    /// independent receive-side count that stays correct when several upstreams
    /// share an interface. Requires NET_ADMIN. Takes priority over `meter_iface`.
    pub meter_upstream: bool,
}

impl Default for UpstreamConfig {
    fn default() -> Self {
        Self {
            peer: String::new(),
            mint: String::new(),
            amount: 8,
            topup: 8,
            interval_secs: 5,
            meter_iface: None,
            meter_upstream: false,
        }
    }
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

    /// Build the PriceSheet this node advertises: one offer per configured
    /// product, each priced across all accepted mints. v1 assumes the "sat" unit
    /// and one rate per product (no per-mint differentiation yet), so whichever
    /// mint a client picks, the price core charges is the same.
    pub fn price_sheet(&self) -> PriceSheet {
        let offers = self
            .products
            .iter()
            .map(|p| {
                let scale = if p.pricing_scale == 0 {
                    DEFAULT_PRICING_SCALE
                } else {
                    p.pricing_scale
                };
                let prices: Vec<MintPrice> = self
                    .mints
                    .iter()
                    .map(|url| MintPrice {
                        mint_url: url.clone(),
                        price_per_second: p.price_per_second,
                        price_per_unit: p.price_per_unit,
                        mint_unit: "sat".to_string(),
                    })
                    .collect();
                ProductOffer::new(scale, &prices, Vec::new())
            })
            .collect();
        PriceSheet::new(offers, DEFAULT_MIN_INTERVAL_MS, DEFAULT_MAX_INTERVAL_MS)
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
    fn load_parses_an_explicit_file() {
        let mut path = std::env::temp_dir();
        path.push(format!("tollgate-test-cfg-{}.yaml", std::process::id()));
        std::fs::write(
            &path,
            "listen: \"0.0.0.0:9999\"\nunit: \"wh\"\nfirewall: sets-only\nmints:\n  - \"http://m\"\n",
        )
        .unwrap();

        let cfg = Config::load(Some(&path)).expect("load explicit config");
        assert_eq!(cfg.listen, "0.0.0.0:9999");
        assert_eq!(cfg.unit, "wh");
        assert_eq!(cfg.firewall, FirewallMode::SetsOnly);
        assert_eq!(cfg.mints, vec!["http://m".to_string()]);

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn load_missing_explicit_file_errors() {
        let result = Config::load(Some(Path::new("/nonexistent/tollgate-does-not-exist.yaml")));
        assert!(result.is_err());
    }

    #[test]
    fn price_sheet_offers_each_product_across_all_mints() {
        let cfg: Config = serde_yaml::from_str(
            "mints:\n  - \"http://m1\"\n  - \"http://m2\"\nproducts:\n  - pricing_scale: 1000\n    price_per_second: 0\n    price_per_unit: 7\n",
        )
        .unwrap();

        let sheet = cfg.price_sheet();
        assert_eq!(
            sheet.interval_ms,
            (DEFAULT_MIN_INTERVAL_MS, DEFAULT_MAX_INTERVAL_MS)
        );
        assert_eq!(sheet.products.len(), 1);
        let offer = &sheet.products[0];
        assert_eq!(offer.pricing_scale, 1000);
        assert_eq!(offer.mints.len(), 2);
        // Each mint option carries the product's single rate and the sat unit.
        for mint in &offer.mints {
            assert_eq!(mint.price_per_unit, 7);
            assert_eq!(mint.price_per_second, 0);
            assert_eq!(mint.mint_unit, "sat");
        }
        // pricing_scale 0 in config falls back to the default.
        let zero_scale: Config =
            serde_yaml::from_str("mints:\n  - \"http://m\"\nproducts:\n  - price_per_unit: 1\n")
                .unwrap();
        assert_eq!(
            zero_scale.price_sheet().products[0].pricing_scale,
            DEFAULT_PRICING_SCALE
        );
    }

    #[test]
    fn price_sheet_handles_empty_mints_and_empty_products() {
        // A product but no accepted mints → an offer with zero mint options
        // (this is the detect-only shape — a node that advertises but sells nothing).
        let no_mints: Config = serde_yaml::from_str("products:\n  - price_per_unit: 1\n").unwrap();
        let sheet = no_mints.price_sheet();
        assert_eq!(sheet.products.len(), 1);
        assert!(sheet.products[0].mints.is_empty());

        // No products at all → an empty sheet (a node that sells nothing).
        let empty: Config = serde_yaml::from_str("{}").unwrap();
        assert!(empty.price_sheet().products.is_empty());
    }

    #[test]
    fn upstreams_parse_with_field_defaults() {
        let cfg: Config = serde_yaml::from_str(
            "upstreams:\n  - peer: \"http://gw:4747\"\n    mint: \"http://m:3338\"\n    amount: 20\n",
        )
        .unwrap();
        assert_eq!(cfg.upstreams.len(), 1);
        let u = &cfg.upstreams[0];
        assert_eq!(u.peer, "http://gw:4747");
        assert_eq!(u.amount, 20);
        assert_eq!(u.topup, 8); // default
        assert_eq!(u.interval_secs, 5); // default

        let empty: Config = serde_yaml::from_str("{}").unwrap();
        assert!(empty.upstreams.is_empty());
    }

    #[test]
    fn metering_interval_defaults_to_5_and_parses() {
        let cfg: Config = serde_yaml::from_str("{}").unwrap();
        assert_eq!(cfg.metering_interval_secs, 5);
        let cfg: Config = serde_yaml::from_str("metering_interval_secs: 1").unwrap();
        assert_eq!(cfg.metering_interval_secs, 1);
    }

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
