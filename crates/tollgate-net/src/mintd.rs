//! `mint.yaml`: what `mintd` reads.
//!
//! The mint is its own daemon so that `tollgated` holds none of its keys, and
//! so that issuing — the one power that has to be guarded — sits in a process
//! that does nothing else (`docs/design/core/tollgate-daemons.md`). Every
//! setting here is read at startup; changing one means restarting `mintd`.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::mint::{IssueLimit, Listeners, MintConfig};

/// The whole of `mint.yaml`.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct MintdFile {
    /// Quantity unit. Must match `mint.unit` in `tollgate.yaml`.
    pub unit: String,
    /// The URL peers reach this mint on, as `tollgated` advertises it. The
    /// mint names itself by it in its info.
    pub url: String,
    /// The mint's own secret, as hex. Generated on first start. Empty picks
    /// the state directory.
    pub seed_file: String,
    /// The mint database: the spent-proof set and the quotes issued. Empty
    /// picks the state directory.
    pub file: String,
    /// Largest amount one quote may be for. A peering grows to the largest
    /// channel it funds, so this has to be at least that.
    pub max_amount: u64,
    /// The mint as peers and wallets see it.
    pub public: ListenSection,
    /// The mint as `merchantd` sees it: mint quotes are paid on creation.
    pub private: ListenSection,
    /// Whether the public listener pays mint quotes on creation too.
    ///
    /// On, service is free to anyone who can reach the mint. It exists for
    /// testing and for handing free vouchers to third parties; a node that
    /// sells turns it off.
    pub auto_accept: bool,
    /// Mint quotes the public listener serves per minute, across everybody
    /// who asks. `0` is unlimited. What a quote may be for is not capped.
    pub issue_quotes_per_minute: u64,
    /// Where the local socket `minttop` reads is served. Empty picks the
    /// default.
    pub control_socket: String,
}

/// One listener's address.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ListenSection {
    /// `host:port` to bind.
    pub listen: String,
}

impl Default for MintdFile {
    fn default() -> Self {
        Self {
            unit: "byte".into(),
            url: "http://127.0.0.1:3338".into(),
            seed_file: String::new(),
            file: String::new(),
            // 1 TiB: well over the largest channel `tollgated` funds (16 GiB),
            // so one sale at the market fits one quote. Still within one mint
            // request's outputs: 512 of the largest denomination, 2^31.
            max_amount: 1 << 40,
            public: ListenSection {
                listen: "0.0.0.0:3338".into(),
            },
            // Loopback: whoever reaches it can print this node's vouchers.
            private: ListenSection {
                listen: "127.0.0.1:3337".into(),
            },
            auto_accept: true,
            issue_quotes_per_minute: IssueLimit::default().quotes_per_minute,
            control_socket: String::new(),
        }
    }
}

impl MintdFile {
    /// Read `mint.yaml`.
    pub fn load(path: &Path) -> Result<Self> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        serde_yaml::from_str(&text).with_context(|| format!("parse {}", path.display()))
    }

    /// Where the seed lives.
    pub fn seed_path(&self) -> PathBuf {
        if self.seed_file.is_empty() {
            crate::mint::default_seed_path()
        } else {
            self.seed_file.clone().into()
        }
    }

    /// Where the database lives.
    pub fn db_path(&self) -> PathBuf {
        if self.file.is_empty() {
            crate::mint::default_path()
        } else {
            self.file.clone().into()
        }
    }

    /// Where the control socket is served.
    pub fn control_path(&self) -> PathBuf {
        if self.control_socket.is_empty() {
            default_control_path()
        } else {
            self.control_socket.clone().into()
        }
    }

    /// The mint these settings describe, given its seed.
    pub fn mint_config(&self, seed: Vec<u8>) -> MintConfig {
        MintConfig {
            url: self.url.clone(),
            unit: self.unit.clone(),
            seed,
            file: self.db_path(),
            max_amount: self.max_amount.max(1),
        }
    }

    /// Where and how the mint is served.
    pub fn listeners(&self) -> Result<Listeners> {
        let parse = |what: &str, addr: &str| -> Result<SocketAddr> {
            addr.parse()
                .with_context(|| format!("{what}.listen {addr:?} is not an address"))
        };
        Ok(Listeners {
            public: parse("public", &self.public.listen)?,
            private: parse("private", &self.private.listen)?,
            auto_accept: self.auto_accept,
            issue_limit: IssueLimit {
                quotes_per_minute: self.issue_quotes_per_minute,
            },
        })
    }
}

/// Where `mintd` serves the socket `minttop` reads, unless told otherwise.
pub fn default_control_path() -> PathBuf {
    std::env::temp_dir().join("mintd.sock")
}

/// What `mintd` publishes on its control socket, one JSON object per
/// connection.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct Snapshot {
    /// The URL peers reach the mint on.
    pub url: String,
    /// The unit it issues in.
    pub unit: String,
    /// The active keyset for that unit.
    pub keyset: String,
    /// The public listener's address.
    pub public: String,
    /// The private listener's address.
    pub private: String,
    /// Whether the public listener gives vouchers away.
    pub auto_accept: bool,
    /// Quotes per minute the public listener serves; `0` is unlimited.
    pub issue_quotes_per_minute: u64,
    /// Mint quotes served on the public listener since start.
    pub public_quotes: u64,
    /// Mint quotes served on the private listener since start.
    pub private_quotes: u64,
    /// Public quotes refused for being over the limit since start.
    pub refused_quotes: u64,
    /// Seconds since `mintd` started.
    pub uptime_secs: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_file_is_a_free_byte_mint_on_the_usual_ports() {
        let file: MintdFile = serde_yaml::from_str("{}").expect("parse");
        let listeners = file.listeners().expect("listeners");
        assert_eq!(file.unit, "byte");
        assert!(listeners.auto_accept, "free until something sells");
        assert_eq!(listeners.public.port(), 3338);
        assert!(listeners.private.ip().is_loopback(), "private stays local");
        assert_eq!(listeners.issue_limit, IssueLimit::default());
    }

    #[test]
    fn auto_accept_and_its_request_limit_can_be_set() {
        let file: MintdFile =
            serde_yaml::from_str("auto_accept: false\nissue_quotes_per_minute: 3\n")
                .expect("parse");
        let listeners = file.listeners().expect("listeners");
        assert!(!listeners.auto_accept);
        assert_eq!(listeners.issue_limit.quotes_per_minute, 3);
    }

    #[test]
    fn a_removed_value_cap_is_refused_rather_than_ignored() {
        assert!(serde_yaml::from_str::<MintdFile>("issue_burst_bytes: 7\n").is_err());
    }

    /// The configs the packages install have to parse.
    #[test]
    fn the_configs_the_packages_install_are_valid() {
        for (name, text) in [
            (
                "openwrt",
                include_str!("../../../packaging/openwrt-ipk/files/etc/tollgate/mint.yaml"),
            ),
            ("macos", include_str!("../../../packaging/macos/mint.yaml")),
        ] {
            let file: MintdFile = serde_yaml::from_str(text)
                .unwrap_or_else(|e| panic!("the {name} mint config does not parse: {e}"));
            file.listeners()
                .unwrap_or_else(|e| panic!("the {name} mint config does not resolve: {e:#}"));
            assert_eq!(file.unit, "byte", "{name}");
            assert!(
                Path::new(&file.file).is_absolute(),
                "{name}: a relative database"
            );
            assert!(
                Path::new(&file.seed_file).is_absolute(),
                "{name}: a relative seed"
            );
        }
    }
}
