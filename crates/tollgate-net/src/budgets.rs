//! Each payer's budget, kept on disk so it survives a reconnect and a restart.
//!
//! A budget belongs to the payer, not to a session or a channel. Core asks for
//! it to be kept ([`Action::SaveBudget`](tollgate_core::Action::SaveBudget))
//! at every purchase it accepts and when a session ends, and takes it back when
//! the payer connects again. This is where it is kept in between: one small
//! JSON file per instance, rewritten whole each time.
//!
//! The deadline is kept as a clock time, since core's clock starts again with
//! the node. A record past its deadline is dropped when the file is read and
//! whenever it is written. A node that crashes loses at most what was drawn
//! since the last write, in the payer's favor.
//!
//! The record is kept by the payer's key. Under `enforcer.identity: address`
//! the address is part of it, so the budget comes back only to the same key
//! from the same address.

use std::collections::BTreeMap;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tollgate_core::Millis;
use tollgate_core::grant::Budget;
use tollgate_protocol::PubKey;
use tracing::warn;

use crate::wire::PeerIdentity;

/// One payer's budget, as it is written down.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    /// Units left.
    pub remaining: u64,
    /// When they expire, in milliseconds since the Unix epoch.
    pub deadline_unix_ms: u64,
}

/// Every payer's budget, and where they are kept.
#[derive(Debug, Default)]
pub struct BudgetStore {
    /// The file, or `None` to keep them in memory only.
    path: Option<PathBuf>,
    records: BTreeMap<String, Record>,
}

/// What a budget is kept under: the payer's key, and under
/// `enforcer.identity: address` the address it pays from as well.
pub fn key(peer: PubKey, addr: IpAddr, identity: PeerIdentity) -> String {
    let pubkey = hex::encode(peer.0);
    match identity {
        PeerIdentity::Pubkey => pubkey,
        PeerIdentity::Address => format!("{pubkey}@{}", canonical(addr)),
    }
}

/// An IPv4 address the same whether it arrived as itself or IPv6-mapped.
fn canonical(addr: IpAddr) -> IpAddr {
    match addr {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(addr, IpAddr::V4),
        v4 => v4,
    }
}

/// Milliseconds since the Unix epoch, now.
fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_millis() as u64
}

impl BudgetStore {
    /// Budgets kept in memory only: they survive a reconnect, not a restart.
    pub fn in_memory() -> Self {
        Self::default()
    }

    /// Budgets kept at `path`, read from it if it is there. A file that cannot
    /// be read is reported and started over: the payers lose what was in it,
    /// which is what losing the disk would have done.
    pub fn open(path: &Path) -> Self {
        let mut store = Self {
            path: Some(path.to_owned()),
            records: BTreeMap::new(),
        };
        match std::fs::read(path) {
            Ok(bytes) => match serde_json::from_slice(&bytes) {
                Ok(records) => store.records = records,
                Err(e) => {
                    warn!(path = %path.display(), error = %e, "budgets file unreadable; starting over")
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => warn!(path = %path.display(), error = %e, "could not read the budgets file"),
        }
        store.prune(unix_now());
        store
    }

    /// The budget kept under `key`, on core's clock, where `now` is core's
    /// time now. `None` if there is none, or its deadline has passed.
    pub fn get(&self, key: &str, now: Millis) -> Option<Budget> {
        let record = self.records.get(key)?;
        let left = record.deadline_unix_ms.checked_sub(unix_now())?;
        (record.remaining > 0 && left > 0).then_some(Budget {
            remaining: record.remaining,
            deadline: now + left,
        })
    }

    /// Keep `budget`, whose deadline is on core's clock with `now` its time
    /// now, under `key`. A budget with nothing left is let go.
    pub fn put(&mut self, key: &str, budget: Budget, now: Millis) -> Result<()> {
        let unix = unix_now();
        let left = budget.deadline.saturating_since(now);
        if budget.remaining == 0 || left == 0 {
            if self.records.remove(key).is_none() {
                return Ok(());
            }
        } else {
            self.records.insert(
                key.to_owned(),
                Record {
                    remaining: budget.remaining,
                    deadline_unix_ms: unix.saturating_add(left),
                },
            );
        }
        self.prune(unix);
        self.write()
    }

    /// How many budgets are kept.
    pub fn len(&self) -> usize {
        self.records.len()
    }

    /// Whether none are.
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    fn prune(&mut self, unix: u64) {
        self.records
            .retain(|_, r| r.remaining > 0 && r.deadline_unix_ms > unix);
    }

    /// Write the whole file, through a temporary one renamed into place, so a
    /// crash leaves the old file or the new one and never half of either.
    fn write(&self) -> Result<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        }
        let tmp = path.with_extension("json.tmp");
        let bytes = serde_json::to_vec(&self.records).expect("records serialize");
        {
            use std::io::Write;
            let mut file =
                std::fs::File::create(&tmp).with_context(|| format!("create {}", tmp.display()))?;
            file.write_all(&bytes)
                .with_context(|| format!("write {}", tmp.display()))?;
            // On the disk before it replaces the old file, or a power cut
            // could leave the new name pointing at nothing.
            file.sync_all()
                .with_context(|| format!("sync {}", tmp.display()))?;
        }
        std::fs::rename(&tmp, path).with_context(|| format!("replace {}", path.display()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tempdir::Dir as TempDir;

    fn peer(seed: u8) -> PubKey {
        let mut b = [seed; 33];
        b[0] = 0x02;
        PubKey(b)
    }

    #[test]
    fn a_budget_survives_the_store_being_opened_again() {
        let dir = TempDir::new();
        let path = dir.path().join("budgets.json");
        let key = key(peer(1), "10.0.0.7".parse().unwrap(), PeerIdentity::Address);

        let mut store = BudgetStore::open(&path);
        store
            .put(
                &key,
                Budget {
                    remaining: 700,
                    deadline: Millis(60_000),
                },
                Millis(0),
            )
            .expect("write");

        // A restart: core's clock starts again from nothing.
        let reopened = BudgetStore::open(&path);
        let back = reopened.get(&key, Millis(5)).expect("kept");
        assert_eq!(back.remaining, 700);
        assert!(back.deadline <= Millis(60_005) && back.deadline >= Millis(59_000));
    }

    #[test]
    fn nothing_left_or_past_its_deadline_is_let_go() {
        let mut store = BudgetStore::in_memory();
        let k = key(peer(1), "10.0.0.7".parse().unwrap(), PeerIdentity::Pubkey);
        store
            .put(
                &k,
                Budget {
                    remaining: 1,
                    deadline: Millis(10_000),
                },
                Millis(0),
            )
            .expect("put");
        assert_eq!(store.len(), 1);
        store
            .put(
                &k,
                Budget {
                    remaining: 0,
                    deadline: Millis(10_000),
                },
                Millis(0),
            )
            .expect("put");
        assert!(store.is_empty());
        store
            .put(
                &k,
                Budget {
                    remaining: 5,
                    deadline: Millis(10),
                },
                Millis(10),
            )
            .expect("put");
        assert!(store.get(&k, Millis(10)).is_none(), "already expired");
    }

    #[test]
    fn under_address_identity_a_budget_comes_back_only_to_the_same_address() {
        let a: IpAddr = "10.0.0.7".parse().unwrap();
        let b: IpAddr = "10.0.0.8".parse().unwrap();
        let mapped: IpAddr = "::ffff:10.0.0.7".parse().unwrap();
        assert_ne!(
            key(peer(1), a, PeerIdentity::Address),
            key(peer(1), b, PeerIdentity::Address)
        );
        assert_eq!(
            key(peer(1), a, PeerIdentity::Address),
            key(peer(1), mapped, PeerIdentity::Address)
        );
        assert_eq!(
            key(peer(1), a, PeerIdentity::Pubkey),
            key(peer(1), b, PeerIdentity::Pubkey),
            "a proven key is the payer wherever it is"
        );
        assert_ne!(
            key(peer(1), a, PeerIdentity::Pubkey),
            key(peer(2), a, PeerIdentity::Pubkey)
        );
    }

    #[test]
    fn an_unreadable_file_starts_over() {
        let dir = TempDir::new();
        let path = dir.path().join("budgets.json");
        std::fs::write(&path, b"not json").unwrap();
        assert!(BudgetStore::open(&path).is_empty());
    }
}
