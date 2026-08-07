//! What this node is holding, and how it spends it.
//!
//! A thin layer over cdk's wallet, and deliberately nothing more. Proof
//! selection, change, keyset rotation, the spent-proof bookkeeping and the
//! database behind it are all Cashu problems that cdk has already solved; what
//! this adds is the part that is TollGate's — a node holds paper from *several*
//! issuers in *two kinds* of unit, and which is which decides what it can do.
//!
//! - **money** — sat-denominated tokens from a mint that deals in money. This
//!   is what the node pays peers with, and the only thing it can top up.
//! - **other people's vouchers** — byte-denominated tokens from peers it has
//!   sold to. Revenue, not spending power: a claim on somebody else's capacity,
//!   worth exactly as much as that peer is willing to carry.
//!
//! So one cdk wallet per (mint, unit), created on demand and kept in one
//! database. Balances are reported money first, because the two answer
//! different questions.
//!
//! # Why the node holds anything at all
//!
//! It could buy money for each purchase and hold nothing. That is what it did
//! first, and it is wrong twice over: every channel it funds then waits on a
//! Lightning settlement — seconds, inside a rollover that has milliseconds —
//! and the money it *takes* has nowhere to go. A balance answers both. Buying
//! becomes local, and payment received is payment kept.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use cdk::amount::SplitTarget;
use cdk::nuts::nut00::KnownMethod;
use cdk::nuts::{CurrencyUnit, PaymentMethod, Token};
use cdk::wallet::{ReceiveOptions, SendOptions, Wallet as CdkWallet};
use cdk_common::database::WalletDatabase;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tracing::{debug, info};

use crate::mint::currency_unit;

/// One issuer's paper, in one unit, and how much of it there is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Holding {
    /// The mint that issued it.
    pub mint: String,
    /// What it is denominated in — `sat` for money, `byte` for a peer's
    /// vouchers.
    pub unit: String,
    /// How much, in that unit.
    pub amount: u64,
    /// Whether this node can pay anybody with it.
    ///
    /// Stated rather than inferred by every reader: a byte-denominated voucher
    /// is a claim on one peer's capacity, and no peer but its issuer will take
    /// it.
    pub spendable: bool,
}

/// A top-up waiting to be paid.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TopUp {
    /// The mint selling the money.
    pub mint: String,
    /// What it is denominated in.
    pub unit: String,
    /// How much was asked for.
    pub amount: u64,
    /// The mint's quote id, to claim it with once it is paid.
    pub quote: String,
    /// The bolt11 invoice to pay.
    pub request: String,
}

/// Everything this node holds, across issuers and units.
#[derive(Clone)]
pub struct Wallet {
    /// One cdk wallet per (mint, unit). Created on demand: a node learns which
    /// issuers it holds paper from by being paid in it.
    wallets: Arc<Mutex<BTreeMap<(String, String), CdkWallet>>>,
    store: Arc<dyn WalletDatabase<cdk_common::database::Error> + Send + Sync>,
    /// Derives the wallet's keys. The node's own identity, hashed, so a
    /// restored config restores the balance with it.
    seed: [u8; 64],
    path: PathBuf,
}

impl std::fmt::Debug for Wallet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Wallet")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

impl Wallet {
    /// Open the wallet database at `path`, creating it if it is not there.
    ///
    /// The seed is derived from the node's own secret, so the wallet is part of
    /// the node's identity rather than a separate thing to back up: restore the
    /// config on a new box and the proofs it holds are recoverable from it.
    pub async fn open(path: impl Into<PathBuf>, seed: [u8; 64]) -> Result<Self> {
        let path = path.into();
        if let Some(dir) = path.parent()
            && !dir.as_os_str().is_empty()
        {
            std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        }

        let store = cdk_sqlite::wallet::WalletSqliteDatabase::new(&path)
            .await
            .with_context(|| format!("open the wallet at {}", path.display()))?;

        Ok(Self {
            wallets: Arc::new(Mutex::new(BTreeMap::new())),
            store: Arc::new(store),
            seed,
            path,
        })
    }

    /// Where the wallet lives, for anything that has to say so.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The cdk wallet for one issuer's paper, made if this is the first time.
    async fn wallet_for(&self, mint: &str, unit: &str) -> Result<CdkWallet> {
        let key = (normalise(mint), unit.to_string());
        let mut wallets = self.wallets.lock().await;
        if let Some(wallet) = wallets.get(&key) {
            return Ok(wallet.clone());
        }

        let wallet = CdkWallet::new(
            &key.0,
            currency_unit(unit),
            Arc::clone(&self.store),
            self.seed,
            None,
        )
        .map_err(|e| anyhow!("open a wallet for {unit} at {mint}: {e}"))?;
        wallets.insert(key, wallet.clone());
        Ok(wallet)
    }

    /// Take a token in and hold it.
    ///
    /// Receiving is a swap at the issuing mint, so this both proves the paper
    /// was good and moves it under keys only this node has.
    pub async fn deposit(&self, token: &str) -> Result<Holding> {
        let (mint, unit, _) = describe(token)?;
        let wallet = self.wallet_for(&mint, &unit).await?;
        let amount: u64 = wallet
            .receive(token.trim(), ReceiveOptions::default())
            .await
            .map_err(|e| anyhow!("receive {unit} from {mint}: {e}"))?
            .into();

        info!(%mint, %unit, amount, "deposited");
        Ok(Holding {
            mint,
            unit: unit.clone(),
            amount,
            spendable: is_money(&unit),
        })
    }

    /// Take exactly `amount` of one issuer's paper out, as a token.
    ///
    /// cdk decides which proofs go and swaps for change where it has to; what
    /// comes back is a token for exactly the amount asked for, or an error and
    /// an untouched balance.
    pub async fn spend(&self, mint: &str, unit: &str, amount: u64) -> Result<String> {
        if amount == 0 {
            bail!("spending nothing is not a payment");
        }
        let wallet = self.wallet_for(mint, unit).await?;
        let prepared = wallet
            .prepare_send(amount.into(), SendOptions::default())
            .await
            .map_err(|e| anyhow!("this node cannot pay {amount} {unit} from {mint}: {e}"))?;
        let token = prepared
            .confirm(None)
            .await
            .map_err(|e| anyhow!("send {amount} {unit} from {mint}: {e}"))?;

        debug!(%mint, %unit, amount, "spent");
        Ok(token.to_string())
    }

    /// What is held, money first.
    ///
    /// Money before vouchers because they answer different questions: money is
    /// what this node can spend, and a peer's vouchers are what it has been
    /// paid.
    pub async fn balances(&self) -> Vec<Holding> {
        let wallets: Vec<((String, String), CdkWallet)> = self
            .wallets
            .lock()
            .await
            .iter()
            .map(|(k, w)| (k.clone(), w.clone()))
            .collect();

        let mut holdings = Vec::new();
        for ((mint, unit), wallet) in wallets {
            let amount: u64 = wallet.total_balance().await.unwrap_or_default().into();
            if amount == 0 {
                continue;
            }
            holdings.push(Holding {
                mint,
                spendable: is_money(&unit),
                unit,
                amount,
            });
        }

        holdings.sort_by(|a, b| {
            b.spendable
                .cmp(&a.spendable)
                .then_with(|| a.unit.cmp(&b.unit))
                .then_with(|| a.mint.cmp(&b.mint))
        });
        holdings
    }

    /// How much of one issuer's paper is held.
    pub async fn balance_of(&self, mint: &str, unit: &str) -> u64 {
        match self.wallet_for(mint, unit).await {
            Ok(wallet) => wallet.total_balance().await.unwrap_or_default().into(),
            Err(_) => 0,
        }
    }

    /// Buy money at a mint that sells it, and return the invoice to pay.
    ///
    /// Nothing is held until it is paid, which is [`Wallet::collect`]'s job:
    /// somebody paying a Lightning invoice takes as long as they take, and the
    /// node has other things to do meanwhile.
    pub async fn top_up(&self, mint: &str, unit: &str, amount: u64) -> Result<TopUp> {
        if amount == 0 {
            bail!("topping up by nothing is not a top-up");
        }
        let wallet = self.wallet_for(mint, unit).await?;
        let quote = wallet
            .mint_quote(
                PaymentMethod::Known(KnownMethod::Bolt11),
                Some(amount.into()),
                None,
                None,
            )
            .await
            .map_err(|e| anyhow!("ask {mint} for {amount} {unit}: {e}"))?;

        info!(%mint, %unit, amount, "waiting on payment for a top-up");
        Ok(TopUp {
            mint: normalise(mint),
            unit: unit.to_string(),
            amount,
            quote: quote.id.to_string(),
            request: quote.request.clone(),
        })
    }

    /// Claim a top-up once its invoice has been paid.
    ///
    /// Returns what was actually minted, which is not always what was asked
    /// for: a quote can be paid short, and the mint is the authority on what it
    /// received.
    pub async fn collect(&self, top_up: &TopUp) -> Result<u64> {
        let wallet = self.wallet_for(&top_up.mint, &top_up.unit).await?;
        let proofs = wallet
            .mint(&top_up.quote, SplitTarget::default(), None)
            .await
            .map_err(|e| {
                anyhow!(
                    "claim {} {} from {}: {e}",
                    top_up.amount,
                    top_up.unit,
                    top_up.mint
                )
            })?;

        let minted: u64 = proofs
            .iter()
            .map(|p| u64::from(p.amount))
            .fold(0, u64::saturating_add);
        info!(mint = %top_up.mint, unit = %top_up.unit, minted, "topped up");
        Ok(minted)
    }
}

/// Whether a unit is money — something a peer other than its issuer will take.
///
/// The resource unit is the exception rather than the rule: everything a node
/// is paid in that is *not* the thing it sells is money to it.
fn is_money(unit: &str) -> bool {
    unit != "byte"
}

fn normalise(mint_url: &str) -> String {
    mint_url.trim_end_matches('/').to_string()
}

/// A token's issuer, unit and value.
fn describe(token: &str) -> Result<(String, String, u64)> {
    let parsed = Token::from_str(token.trim()).map_err(|e| anyhow!("parse a token: {e}"))?;
    let mint = parsed
        .mint_url()
        .map_err(|e| anyhow!("read a token's mint: {e}"))?
        .to_string();
    let unit = parsed.unit().unwrap_or(CurrencyUnit::Sat).to_string();
    let amount: u64 = parsed
        .value()
        .map_err(|e| anyhow!("read a token's value: {e}"))?
        .into();
    Ok((normalise(&mint), unit, amount))
}

/// Where a node keeps its wallet unless told otherwise.
///
/// Beside the state a service manager already owns, so the packages keep it
/// across an upgrade without a special case.
pub fn default_path() -> PathBuf {
    for candidate in ["/var/lib/tollgate", "/usr/local/var/lib/tollgate"] {
        let dir = Path::new(candidate);
        if dir.is_dir() || std::fs::create_dir_all(dir).is_ok() {
            return dir.join("wallet.sqlite");
        }
    }
    if let Ok(xdg) = std::env::var("XDG_DATA_HOME")
        && !xdg.is_empty()
    {
        return PathBuf::from(format!("{xdg}/tollgate/wallet.sqlite"));
    }
    PathBuf::from("/tmp/tollgate-wallet.sqlite")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A wallet in a directory that removes itself.
    async fn wallet() -> (Wallet, tempdir::Dir) {
        let dir = tempdir::Dir::new();
        let wallet = Wallet::open(dir.path().join("wallet.sqlite"), [7u8; 64])
            .await
            .expect("open");
        (wallet, dir)
    }

    #[tokio::test]
    async fn a_new_wallet_holds_nothing() {
        let (wallet, _dir) = wallet().await;
        assert!(wallet.balances().await.is_empty());
        assert_eq!(wallet.balance_of("https://mint.example", "sat").await, 0);
    }

    #[tokio::test]
    async fn spending_what_is_not_held_fails() {
        let (wallet, _dir) = wallet().await;
        assert!(
            wallet
                .spend("https://mint.example", "sat", 10)
                .await
                .is_err()
        );
        assert!(
            wallet
                .spend("https://mint.example", "sat", 0)
                .await
                .is_err(),
            "spending nothing is not a payment"
        );
        assert!(wallet.balances().await.is_empty());
    }

    #[tokio::test]
    async fn topping_up_by_nothing_is_refused() {
        let (wallet, _dir) = wallet().await;
        assert!(
            wallet
                .top_up("https://mint.example", "sat", 0)
                .await
                .is_err()
        );
    }

    #[test]
    fn what_counts_as_money_is_everything_but_the_resource() {
        // A byte-denominated voucher is a claim on one peer's capacity, and no
        // peer but its issuer will take it.
        assert!(is_money("sat"));
        assert!(is_money("usd"));
        assert!(!is_money("byte"));
    }

    mod tempdir {
        use std::path::{Path, PathBuf};
        use std::sync::atomic::{AtomicU64, Ordering};

        static NEXT: AtomicU64 = AtomicU64::new(0);

        pub struct Dir(PathBuf);

        impl Dir {
            pub fn new() -> Self {
                let path = std::env::temp_dir().join(format!(
                    "tollgate-wallet-test-{}-{}",
                    std::process::id(),
                    NEXT.fetch_add(1, Ordering::Relaxed)
                ));
                std::fs::create_dir_all(&path).expect("create a temporary directory");
                Self(path)
            }

            pub fn path(&self) -> &Path {
                &self.0
            }
        }

        impl Drop for Dir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }
}
