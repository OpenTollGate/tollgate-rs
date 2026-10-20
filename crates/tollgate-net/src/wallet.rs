//! What this node is holding, and how it spends it.
//!
//! A thin layer over cdk's wallet, and deliberately nothing more. Proof
//! selection, change, keyset rotation, the spent-proof bookkeeping and the
//! database behind it are all Cashu problems that cdk has already solved; what
//! this adds is the part that is TollGate's — a node holds paper from several
//! issuers, and what it can do with a holding depends on who issued it.
//!
//! - **money** — sat-denominated tokens from a mint that deals in money. Any
//!   peer that accepts that mint will take them, so this is what a node pays
//!   with generally, and the only thing it can top itself up in.
//! - **prepaid transit** — byte-denominated tokens from an *upstream*: capacity
//!   bought and not yet spent. Also spending power, but only at the one node
//!   that issued them, since nobody else's mint honours them.
//!
//! What a node never accumulates is its own vouchers. A customer pays in the
//! paper this node issued, and redeeming that cancels this node's own claim —
//! the money side of being paid arrives as money, at the market, which is why
//! selling shows up in the first list and not the second.
//!
//! So one cdk wallet per (mint, unit), created on demand and kept in one
//! database. Balances are reported money first, because a node short of money
//! cannot buy from anyone, where a node short of one upstream's vouchers is
//! only short with that upstream.
//!
//! # Taking money out
//!
//! What the node holds is the operator's, and the operator takes it out in
//! one of two ways: as a token ([`Wallet::export`]), which any Cashu wallet
//! redeems, or paid over Lightning by the mint that holds it
//! ([`Wallet::pay_out`]) to an invoice or a Lightning address. `merchantd`
//! can also do the second by itself, above a threshold: a sweep.
//!
//! # Why the node holds anything at all
//!
//! It could buy money for each purchase and hold nothing. That is what it did
//! first, and it is wrong twice over: every channel it funds then waits on a
//! Lightning settlement — seconds, inside a rollover that has milliseconds —
//! and the money it *takes* has nowhere to go. A balance answers both. Buying
//! becomes local, and payment received is payment kept.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use cdk::amount::SplitTarget;
use cdk::nuts::nut00::KnownMethod;
use cdk::nuts::{CurrencyUnit, MeltQuoteState, MintQuoteState, PaymentMethod, State, Token};
use cdk::wallet::{ReceiveOptions, SendOptions, Wallet as CdkWallet};
use cdk_common::database::WalletDatabase;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tracing::{debug, info};

use crate::mint::currency_unit;

/// What a holding can be spent on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// Money. Any peer that accepts the issuing mint will take it, so it buys
    /// from whoever this node decides to buy from.
    Money,
    /// Capacity bought from one upstream and not yet spent. Spending power at
    /// that node and nowhere else — nobody else's mint honours its paper.
    PrepaidTransit,
}

impl Kind {
    /// What a unit means to a node that sells `resource`.
    ///
    /// Anything the node does not itself sell is money to it; its own resource
    /// unit, issued by somebody else, is a prepayment to that somebody.
    pub fn of(unit: &str, resource: &str) -> Self {
        if unit == resource {
            Kind::PrepaidTransit
        } else {
            Kind::Money
        }
    }
}

/// One issuer's paper, in one unit, and how much of it there is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Holding {
    /// The mint that issued it.
    pub mint: String,
    /// What it is denominated in — `sat` for money, `byte` for capacity.
    pub unit: String,
    /// How much, in that unit.
    pub amount: u64,
    /// What it can be spent on, and with whom.
    ///
    /// Stated rather than left for each reader to infer: both kinds are
    /// spending power, and the difference is whether it is general or is good
    /// at exactly one node.
    pub kind: Kind,
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
    /// The unit this node sells. Holdings in it are somebody else's capacity;
    /// holdings in anything else are money.
    resource: String,
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
    ///
    /// `resource` is the unit this node sells, which is what tells a holding in
    /// it apart from money.
    pub async fn open(
        path: impl Into<PathBuf>,
        seed: [u8; 64],
        resource: impl Into<String>,
    ) -> Result<Self> {
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
            resource: resource.into(),
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
            kind: Kind::of(&unit, &self.resource),
            unit,
            amount,
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

    /// Take a token in and hand its value back out as several tokens, none
    /// worth more than `most`: one payment split into purchases a seller
    /// takes one at a time.
    ///
    /// Returns the parts and what was left over. Only what the token itself
    /// brought in is spent — the swap fees included — never anything else
    /// held of that paper, and once the token is in nothing is an error: a
    /// part already made is money, and is returned with the rest. What is
    /// left over (fees rounding a part down, or a part the wallet could not
    /// make) stays in the wallet.
    pub async fn split(&self, token: &str, most: u64) -> Result<(Vec<String>, u64)> {
        if most == 0 {
            bail!("parts of nothing are not a split");
        }
        let held = self.deposit(token).await?;
        let wallet = self.wallet_for(&held.mint, &held.unit).await?;
        let mut left = held.amount;
        let mut parts = Vec::new();
        while left > 0 {
            match send_within(&wallet, left.min(most), left).await {
                Ok(part) => {
                    left -= part.spent();
                    parts.push(part.token);
                }
                Err(e) => {
                    debug!(left, error = %format!("{e:#}"), "could not make a part");
                    break;
                }
            }
        }
        debug!(mint = %held.mint, unit = %held.unit, parts = parts.len(), left, "split");
        Ok((parts, left))
    }

    /// Hand `amount` of one issuer's paper back out as a token, the fee for
    /// making it included: the token and the swap fee together come to no
    /// more than `amount`, so the fee comes off the token rather than out of
    /// anything else held.
    ///
    /// What a market does with a payment it took and could not sell against.
    /// cdk chooses the proofs; which ones they are does not matter, since the
    /// balance of that paper ends no lower than before the payment came in.
    /// Redeeming the token costs whoever redeems it the mint's input fee on
    /// its proofs, as any token does.
    pub async fn refund(&self, mint: &str, unit: &str, amount: u64) -> Result<Sent> {
        let wallet = self.wallet_for(mint, unit).await?;
        let refund = send_within(&wallet, amount, amount)
            .await
            .with_context(|| format!("hand {amount} {unit} of {mint} back"))?;
        debug!(%mint, %unit, amount, worth = refund.worth, fee = refund.fee, "refunded");
        Ok(refund)
    }

    /// What is held, money first.
    ///
    /// Money before prepaid transit because a node short of money cannot buy
    /// from anybody, where a node short of one upstream's vouchers is only
    /// short with that upstream.
    pub async fn balances(&self) -> Vec<Holding> {
        // Every issuer the database holds paper from, not only those touched
        // since this process started: after a restart, money held at a mint
        // nothing has used yet is still money.
        for (mint, unit) in self.held_issuers().await {
            if let Err(e) = self.wallet_for(&mint, &unit).await {
                debug!(%mint, %unit, error = %format!("{e:#}"), "cannot open a held issuer's wallet");
            }
        }
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
                kind: Kind::of(&unit, &self.resource),
                unit,
                amount,
            });
        }

        holdings.sort_by_key(|h| (h.kind != Kind::Money, h.unit.clone(), h.mint.clone()));
        holdings
    }

    /// Every (mint, unit) the database holds unspent paper of.
    async fn held_issuers(&self) -> BTreeSet<(String, String)> {
        match self
            .store
            .get_proofs(None, None, Some(vec![State::Unspent]), None)
            .await
        {
            Ok(proofs) => proofs
                .into_iter()
                .map(|p| (normalise(&p.mint_url.to_string()), p.unit.to_string()))
                .collect(),
            Err(e) => {
                debug!(error = %e, "cannot list what the wallet holds");
                BTreeSet::new()
            }
        }
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
    ///
    /// The quote's state is asked first, and nothing is minted until it is
    /// paid: cdk reserves a quote before it looks at the amount, and minting
    /// an unpaid one fails in a way it treats as ambiguous, leaving the quote
    /// reserved — every later attempt, paid or not, is then refused as
    /// "already in use by another operation". A quote left that way by an
    /// earlier run is released and tried once more.
    pub async fn collect(&self, top_up: &TopUp) -> Result<u64> {
        let wallet = self.wallet_for(&top_up.mint, &top_up.unit).await?;
        let claim = |e: cdk::Error| {
            anyhow!(
                "claim {} {} from {}: {e}",
                top_up.amount,
                top_up.unit,
                top_up.mint
            )
        };
        let state = wallet
            .check_mint_quote_status(&top_up.quote)
            .await
            .map_err(claim)?
            .state;
        match state {
            MintQuoteState::Paid => {}
            MintQuoteState::Issued => bail!("quote {} was already claimed", top_up.quote),
            _ => bail!("quote {} is not paid yet", top_up.quote),
        }
        let proofs = match wallet
            .mint(&top_up.quote, SplitTarget::default(), None)
            .await
        {
            Err(e) if quote_in_use(&e) => {
                self.recover(&top_up.mint, &top_up.unit).await?;
                wallet
                    .mint(&top_up.quote, SplitTarget::default(), None)
                    .await
                    .map_err(claim)?
            }
            other => other.map_err(claim)?,
        };

        let minted: u64 = proofs
            .iter()
            .map(|p| u64::from(p.amount))
            .fold(0, u64::saturating_add);
        info!(mint = %top_up.mint, unit = %top_up.unit, minted, "topped up");
        Ok(minted)
    }

    /// Mint `amount` of an issuer's paper at that issuer, and hold it.
    ///
    /// For a mint that issues without being paid, which is how a TollGate
    /// node's mint hands out its vouchers while there is no market: an ordinary
    /// NUT-04 quote, already paid when it is checked, and minted at once. A
    /// mint that does not issue this way says so by advertising no mint quotes
    /// in `unit`, and that is refused here before anything is asked of it.
    ///
    /// A mint caps what one quote may issue, so a larger amount is asked for
    /// in several. Returns what was minted.
    pub async fn issue(&self, mint: &str, unit: &str, amount: u64) -> Result<u64> {
        if amount == 0 {
            bail!("minting nothing is not an issuance");
        }
        let wallet = self.wallet_for(mint, unit).await?;
        let method = PaymentMethod::Known(KnownMethod::Bolt11);

        let info = wallet
            .load_mint_info()
            .await
            .map_err(|e| anyhow!("read what {mint} offers: {e}"))?;
        let Some(settings) = info.nuts.nut04.get_settings(&currency_unit(unit), &method) else {
            bail!("{mint} does not mint {unit} for the asking");
        };
        let ceiling = settings
            .max_amount
            .map(u64::from)
            .filter(|max| *max > 0)
            .unwrap_or(amount);

        let mut minted = 0u64;
        while minted < amount {
            let ask = (amount - minted).min(ceiling);
            let quote = wallet
                .mint_quote(method.clone(), Some(ask.into()), None, None)
                .await
                .map_err(|e| anyhow!("ask {mint} for {ask} {unit}: {e}"))?;
            let proofs = wallet
                .mint(&quote.id, SplitTarget::default(), None)
                .await
                .map_err(|e| anyhow!("mint {ask} {unit} at {mint}: {e}"))?;

            let got: u64 = proofs
                .iter()
                .map(|p| u64::from(p.amount))
                .fold(0, u64::saturating_add);
            // A mint that quotes and then issues nothing would otherwise be
            // asked again forever.
            if got == 0 {
                bail!("{mint} issued nothing against a quote for {ask} {unit}");
            }
            minted = minted.saturating_add(got);
        }

        debug!(%mint, %unit, minted, "minted");
        Ok(minted)
    }

    /// Finish or roll back anything a previous run left half-done.
    ///
    /// cdk drives minting and sending as sagas, and a saga interrupted mid-way
    /// leaves its quote or its proofs reserved — the next attempt is refused
    /// with "already in use by another operation". A node is killed mid-mint by
    /// an ordinary restart, so this is not an edge case; cdk expects it to be
    /// called at startup and we were not calling it.
    ///
    /// Needs the mint to be reachable, so a failure here is not fatal: it is
    /// retried the next time anything claims.
    async fn recover(&self, mint: &str, unit: &str) -> Result<()> {
        let wallet = self.wallet_for(mint, unit).await?;
        match wallet.recover_incomplete_sagas().await {
            Ok(report) => {
                if report.recovered + report.compensated + report.failed > 0 {
                    info!(
                        %mint,
                        recovered = report.recovered,
                        rolled_back = report.compensated,
                        failed = report.failed,
                        "finished what an earlier run left open"
                    );
                }
                Ok(())
            }
            Err(e) => {
                debug!(%mint, error = %e, "could not recover incomplete operations");
                Ok(())
            }
        }
    }

    /// Claim everything paid for and not yet collected.
    ///
    /// A quote lives in the wallet's own database from the moment it is asked
    /// for, so an invoice paid while the node was restarting, or while the mint
    /// was rate-limiting the status polls, is money waiting rather than money
    /// lost. This is what goes and gets it.
    ///
    /// Quotes that are simply unpaid fail here and stay pending, which is why
    /// nothing is removed and the count is of what was actually minted.
    pub async fn claim_pending(&self, mint: &str, unit: &str) -> Result<u64> {
        // A quote a killed run left reserved cannot be minted until the saga
        // that reserved it is settled, and that is exactly the state a claim is
        // called in.
        self.recover(mint, unit).await?;

        let wallet = self.wallet_for(mint, unit).await?;
        let pending = wallet
            .get_unissued_mint_quotes()
            .await
            .map_err(|e| anyhow!("list unclaimed quotes at {mint}: {e}"))?;

        if pending.is_empty() {
            debug!(%mint, %unit, "nothing outstanding to claim");
            return Ok(0);
        }

        debug!(%mint, %unit, outstanding = pending.len(), "claiming");

        let mut claimed = 0u64;
        for (i, quote) in pending.iter().enumerate() {
            // Paced. Every attempt is a request to somebody else's server, and
            // a node that has been running a while has a quote per unpaid
            // top-up: claiming forty of them as fast as the loop goes is how a
            // mint starts answering "rate limit exceeded" to everything, which
            // is the failure this whole path exists to recover from.
            if i > 0 {
                tokio::time::sleep(BETWEEN_CLAIMS).await;
            }

            match wallet
                .mint(&quote.id.to_string(), SplitTarget::default(), None)
                .await
            {
                Ok(proofs) => {
                    let minted: u64 = proofs
                        .iter()
                        .map(|p| u64::from(p.amount))
                        .fold(0, u64::saturating_add);
                    claimed = claimed.saturating_add(minted);
                    info!(%mint, %unit, minted, quote = %quote.id, "claimed");
                }
                // Usually "not paid yet", which is not a fault: the quote stays
                // in the database and the next attempt tries again.
                Err(e) => debug!(%mint, quote = %quote.id, error = %e, "not claimable yet"),
            }
        }
        Ok(claimed)
    }

    /// Take one issuer's paper out as a token: `amount` of it, or with `None`
    /// everything held. The mint's swap fee comes out of what is held, so a
    /// token for everything is worth everything less that fee.
    ///
    /// The token is money: whoever holds the string can redeem it.
    pub async fn export(&self, mint: &str, unit: &str, amount: Option<u64>) -> Result<Sent> {
        let wallet = self.wallet_for(mint, unit).await?;
        let held: u64 = wallet.total_balance().await.unwrap_or_default().into();
        if held == 0 {
            bail!("nothing of {mint}'s {unit} is held");
        }
        let want = amount.unwrap_or(held);
        if want == 0 {
            bail!("exporting nothing is not a token");
        }
        if want > held {
            bail!("{want} {unit} asked for, {held} held at {mint}");
        }
        let sent = send_within(&wallet, want, held)
            .await
            .with_context(|| format!("take {want} {unit} out of {mint} as a token"))?;
        if amount.is_some() && sent.worth != want {
            bail!(
                "{want} {unit} and the mint's fee come to more than the {held} held; at most {} can go",
                sent.worth
            );
        }
        info!(%mint, %unit, worth = sent.worth, fee = sent.fee, "exported as a token");
        Ok(sent)
    }

    /// Pay one issuer's money out over Lightning, by having the mint that
    /// issued it pay `to`.
    ///
    /// [`Size::Exactly`] pays that much, its fees on top; [`Size::Within`]
    /// pays as much as fits in a budget with the fees, which is how the whole
    /// of a balance is paid out. Paying a Lightning address asks it for an
    /// invoice of the size wanted, so only a mint that deals in sats (or
    /// msats) can pay one; an invoice says its own amount, and `Within` only
    /// checks that it fits.
    ///
    /// The mint reserves a fee for routing and returns what it did not use as
    /// change, so what leaves the wallet is the amount, the fee actually paid
    /// and the input fee.
    pub async fn pay_out(
        &self,
        mint: &str,
        unit: &str,
        to: &Destination,
        size: Size,
    ) -> Result<Payout> {
        let wallet = self.wallet_for(mint, unit).await?;
        let held: u64 = wallet.total_balance().await.unwrap_or_default().into();
        let budget = match size {
            Size::Exactly(0) => bail!("paying out nothing is not a payment"),
            Size::Exactly(_) => held,
            Size::Within(budget) => budget.min(held),
        };
        if budget == 0 {
            bail!("nothing of {mint}'s {unit} is held to pay out");
        }

        let finalized = match to {
            Destination::Invoice(invoice) => {
                let quote = wallet
                    .melt_quote(
                        PaymentMethod::Known(KnownMethod::Bolt11),
                        invoice,
                        None,
                        None,
                    )
                    .await
                    .map_err(|e| anyhow!("ask {mint} to pay the invoice: {e}"))?;
                if let Size::Exactly(amount) = size
                    && u64::from(quote.amount) != amount
                {
                    bail!("the invoice is for {}, not {amount}", quote.amount);
                }
                melt_within(&wallet, &quote, budget).await?
            }
            Destination::Address(address) => {
                let msat_per_unit = match unit {
                    "sat" => 1000,
                    "msat" => 1,
                    _ => bail!("{mint} deals in {unit}; a Lightning address is paid in sats"),
                };
                let mut want = match size {
                    Size::Exactly(amount) => amount,
                    Size::Within(_) => budget,
                };
                let mut last = anyhow!("nothing to pay");
                let mut done = None;
                for _ in 0..8 {
                    if want == 0 {
                        break;
                    }
                    // The invoice is asked for here rather than by cdk, which
                    // refuses a callback on any host but the address's own —
                    // and Wallet of Satoshi's, among others, is on another.
                    let invoice = lnurl_invoice(address, want.saturating_mul(msat_per_unit))
                        .await
                        .map_err(|e| {
                            anyhow!("ask {address} for an invoice of {want} {unit}: {e:#}")
                        })?;
                    let quote = wallet
                        .melt_quote(
                            PaymentMethod::Known(KnownMethod::Bolt11),
                            &invoice,
                            None,
                            None,
                        )
                        .await
                        .map_err(|e| anyhow!("ask {mint} to pay {address} {want} {unit}: {e}"))?;
                    if u64::from(quote.amount) != want {
                        bail!(
                            "{address} sent an invoice for {}, not the {want} asked for",
                            quote.amount
                        );
                    }
                    let need = u64::from(quote.amount).saturating_add(quote.fee_reserve.into());
                    if need > budget {
                        if matches!(size, Size::Exactly(_)) {
                            bail!(
                                "{want} {unit} and a fee reserve of {} come to more than the {budget} held",
                                quote.fee_reserve
                            );
                        }
                        last = anyhow!("{want} {unit} and its fee reserve come to {need}");
                        want = want.saturating_sub((need - budget).max(1));
                        continue;
                    }
                    match melt_within(&wallet, &quote, budget).await {
                        Ok(finalized) => {
                            done = Some(finalized);
                            break;
                        }
                        Err(e) if matches!(size, Size::Within(_)) => {
                            // Usually the input fee tipping it over: a little less.
                            last = e;
                            want = want.saturating_sub((want / 100).max(1));
                        }
                        Err(e) => return Err(e),
                    }
                }
                done.ok_or(last)?
            }
        };

        let state = finalized.state();
        if state != MeltQuoteState::Paid {
            bail!("{mint} did not pay: the payment is {state}");
        }
        let payout = Payout {
            mint: normalise(mint),
            unit: unit.to_string(),
            paid: finalized.amount().into(),
            fee: finalized.fee_paid().into(),
            preimage: finalized.payment_proof().map(str::to_owned),
        };
        info!(mint = %payout.mint, unit = %payout.unit, paid = payout.paid, fee = payout.fee, to = %to, "paid out");
        Ok(payout)
    }
}

/// How long to leave between claim attempts against one mint.
const BETWEEN_CLAIMS: std::time::Duration = std::time::Duration::from_millis(400);

/// A token made out of what is held, and what making it cost.
#[derive(Clone)]
pub struct Sent {
    /// The token. Money: kept out of `Debug`, so it cannot reach a log.
    pub token: String,
    /// What the token is worth.
    pub worth: u64,
    /// The mint's fee on the swap that made it, paid out of the same budget.
    pub fee: u64,
}

impl Sent {
    /// What making the token took out of the wallet.
    fn spent(&self) -> u64 {
        self.worth + self.fee
    }
}

impl std::fmt::Debug for Sent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Sent")
            .field("worth", &self.worth)
            .field("fee", &self.fee)
            .finish_non_exhaustive()
    }
}

/// Where a payout goes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Destination {
    /// A BOLT11 invoice, which says its own amount.
    Invoice(String),
    /// A Lightning address, `user@domain`, which is asked for an invoice.
    Address(String),
}

impl std::str::FromStr for Destination {
    type Err = anyhow::Error;

    fn from_str(text: &str) -> Result<Self> {
        let text = text.trim();
        let text = text
            .strip_prefix("lightning:")
            .or_else(|| text.strip_prefix("LIGHTNING:"))
            .unwrap_or(text);
        if text.is_empty() || text.chars().any(char::is_whitespace) {
            bail!("not an invoice or a Lightning address: {text:?}");
        }
        let lower = text.to_ascii_lowercase();
        if lower.starts_with("lnbc") || lower.starts_with("lntb") || lower.starts_with("lntbs") {
            return Ok(Destination::Invoice(lower));
        }
        match lower.split_once('@') {
            Some((user, domain)) if !user.is_empty() && domain.contains('.') => {
                Ok(Destination::Address(lower))
            }
            _ => bail!("not an invoice or a Lightning address: {text:?}"),
        }
    }
}

impl std::fmt::Display for Destination {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            // An invoice is long; its start says what it is.
            Destination::Invoice(invoice) => write!(f, "{}…", &invoice[..invoice.len().min(24)]),
            Destination::Address(address) => f.write_str(address),
        }
    }
}

/// How much a payout pays.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Size {
    /// Exactly this much, the fees on top.
    Exactly(u64),
    /// As much as fits in this budget, the fees included.
    Within(u64),
}

/// A payment made over Lightning out of what is held.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Payout {
    /// The mint that paid it.
    pub mint: String,
    /// The unit it was paid out of.
    pub unit: String,
    /// What the payee got.
    pub paid: u64,
    /// The fees the mint took for it.
    pub fee: u64,
    /// The Lightning preimage, proof the payment arrived.
    pub preimage: Option<String>,
}

/// What a Lightning address answers at `/.well-known/lnurlp/<user>`, and
/// its callback: the parts of LUD-06 and LUD-16 a payout needs.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LnurlPay {
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    reason: Option<String>,
    #[serde(default)]
    callback: Option<String>,
    #[serde(default)]
    min_sendable: Option<u64>,
    #[serde(default)]
    max_sendable: Option<u64>,
    /// The invoice, in the callback's answer.
    #[serde(default)]
    pr: Option<String>,
}

impl LnurlPay {
    /// The service's own refusal, if this is one.
    fn refused(&self) -> Option<String> {
        (self.status.as_deref() == Some("ERROR"))
            .then(|| self.reason.clone().unwrap_or_else(|| "refused".into()))
    }
}

/// Where to ask a Lightning address for an invoice of `msat`: its callback
/// with the amount added.
///
/// Any HTTPS host. LUD-16 does not tie the callback to the address's domain,
/// and services that put it elsewhere are common.
fn lnurl_callback(pay: &LnurlPay, msat: u64) -> Result<reqwest::Url> {
    if let Some(reason) = pay.refused() {
        bail!("{reason}");
    }
    let callback = pay
        .callback
        .as_deref()
        .context("no callback in its answer")?;
    let mut url = reqwest::Url::parse(callback).context("its callback is not a URL")?;
    if url.scheme() != "https" {
        bail!("its callback is not HTTPS: {callback}");
    }
    if let Some(min) = pay.min_sendable
        && msat < min
    {
        bail!("it takes at least {} sat", min.div_ceil(1000));
    }
    if let Some(max) = pay.max_sendable
        && msat > max
    {
        bail!("it takes at most {} sat", max / 1000);
    }
    url.query_pairs_mut()
        .append_pair("amount", &msat.to_string());
    Ok(url)
}

/// An invoice of `msat` from a Lightning address, `user@domain`.
async fn lnurl_invoice(address: &str, msat: u64) -> Result<String> {
    let (user, domain) = address.split_once('@').context("not user@domain")?;
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .build()?;
    let get = |url: reqwest::Url| {
        let client = client.clone();
        async move {
            let answer = client.get(url.clone()).send().await?;
            let status = answer.status();
            let body: LnurlPay = answer
                .json()
                .await
                .with_context(|| format!("{url} answered {status}, not LNURL"))?;
            anyhow::Ok(body)
        }
    };
    let pay = get(reqwest::Url::parse(&format!(
        "https://{domain}/.well-known/lnurlp/{user}"
    ))?)
    .await?;
    let answer = get(lnurl_callback(&pay, msat)?).await?;
    if let Some(reason) = answer.refused() {
        bail!("{reason}");
    }
    answer.pr.context("no invoice in its answer")
}

/// Melt against `quote` if it, its fee reserve and the input fee fit in
/// `budget`; otherwise leave everything as it was.
async fn melt_within(
    wallet: &CdkWallet,
    quote: &cdk::wallet::MeltQuote,
    budget: u64,
) -> Result<cdk_common::common::FinalizedMelt> {
    let prepared = wallet
        .prepare_melt(&quote.id, HashMap::new())
        .await
        .map_err(|e| anyhow!("set {} aside to pay with: {e}", quote.amount))?;
    let total = u64::from(quote.amount)
        .saturating_add(quote.fee_reserve.into())
        .saturating_add(prepared.total_fee().into());
    if total > budget {
        let _ = prepared.cancel().await;
        bail!(
            "{} with its fees comes to {total}, more than {budget}",
            quote.amount
        );
    }
    prepared
        .confirm()
        .await
        .map_err(|e| anyhow!("pay {}: {e}", quote.amount))
}

/// A token worth up to `want`, where it and the swap fee for making it
/// together come to no more than `budget`.
///
/// Sized by the fee cdk quotes for the proofs it picked, not by guessing a
/// unit at a time: a fee larger than a guess or two would otherwise fail
/// every time. When cdk cannot find `want` plus its fee at all — the wallet
/// holds `budget` and no more — the fee on everything held is the estimate,
/// since no subset of it costs more.
async fn send_within(wallet: &CdkWallet, want: u64, budget: u64) -> Result<Sent> {
    let mut want = want.min(budget);
    let mut last = anyhow!("nothing to send");
    for _ in 0..8 {
        if want == 0 {
            break;
        }
        let prepared = match wallet
            .prepare_send(want.into(), SendOptions::default())
            .await
        {
            Ok(prepared) => prepared,
            Err(e) => {
                debug!(want, budget, error = %e, "not enough for that and its fee");
                last = anyhow!("prepare {want}: {e}");
                let held = wallet
                    .get_unspent_proofs()
                    .await
                    .map_err(|e| anyhow!("list what is held: {e}"))?;
                let fee: u64 = wallet
                    .get_proofs_fee(&held)
                    .await
                    .map_err(|e| anyhow!("price a swap: {e}"))?
                    .total
                    .into();
                // Always less than last time, so this ends.
                want = budget.saturating_sub(fee).min(want - 1);
                continue;
            }
        };
        let fee: u64 = prepared.swap_fee().into();
        if want.saturating_add(fee) > budget {
            let _ = prepared.cancel().await;
            last = anyhow!("{want} and a fee of {fee} come to more than {budget}");
            want = budget.saturating_sub(fee).min(want - 1);
            continue;
        }
        let token = prepared
            .confirm(None)
            .await
            .map_err(|e| anyhow!("send {want}: {e}"))?;
        return Ok(Sent {
            token: token.to_string(),
            worth: want,
            fee,
        });
    }
    Err(last)
}

/// Whether minting failed because an earlier run left the quote reserved.
///
/// The typed error, not its text, so a cdk upgrade that rewords it still
/// matches.
fn quote_in_use(e: &cdk::Error) -> bool {
    matches!(
        e,
        cdk::Error::Database(cdk::cdk_database::Error::QuoteAlreadyInUse)
    )
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

/// Funding channels from a wallet in this process.
///
/// What `merchantd` does when `tollgated` asks it for vouchers, and what a node
/// running as one process does directly: spend what is held of that mint's
/// paper, minting the shortfall first at a mint that gives it away.
#[derive(Debug, Clone)]
pub struct WalletFunding {
    wallet: Wallet,
    runtime: tokio::runtime::Handle,
}

impl WalletFunding {
    /// Fund out of `wallet`. Must be made inside a Tokio runtime.
    pub fn new(wallet: Wallet) -> Self {
        Self {
            wallet,
            runtime: tokio::runtime::Handle::current(),
        }
    }
}

/// Spend `amount` of `mint`'s paper from `wallet`, minting the shortfall first.
///
/// Whatever is already held of that paper is spent first — a purchase that
/// minted and then failed to open its channel leaves vouchers behind, and they
/// are as good as new ones.
pub async fn fund_from(wallet: &Wallet, mint: &str, unit: &str, amount: u64) -> Result<String> {
    let held = wallet.balance_of(mint, unit).await;
    let short = amount.saturating_sub(held);
    if short > 0 {
        tracing::debug!(amount, held, short, %mint, "minting vouchers");
        wallet.issue(mint, unit, short).await?;
    }
    wallet
        .spend(mint, unit, amount)
        .await
        .with_context(|| format!("take {amount} {unit} from {mint} out to fund"))
}

impl crate::channel::Funding for WalletFunding {
    fn vouchers(&self, mint: &str, unit: &str, amount: u64) -> Result<String> {
        let handle = self.runtime.clone();
        let wallet = self.wallet.clone();
        tokio::task::block_in_place(|| handle.block_on(fund_from(&wallet, mint, unit, amount)))
    }

    fn deposit(&self, token: &str) -> Result<()> {
        let handle = self.runtime.clone();
        let wallet = self.wallet.clone();
        tokio::task::block_in_place(|| handle.block_on(wallet.deposit(token))).map(|_| ())
    }
}

/// Where a node keeps its wallet unless told otherwise.
///
/// Beside the state a service manager already owns, so the packages keep it
/// across an upgrade without a special case.
pub fn default_path() -> PathBuf {
    crate::config::state_file("wallet.sqlite")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A wallet in a directory that removes itself.
    async fn wallet() -> (Wallet, crate::tempdir::Dir) {
        let dir = crate::tempdir::Dir::new();
        let wallet = Wallet::open(dir.path().join("wallet.sqlite"), [7u8; 64], "byte")
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
    fn a_lightning_address_may_call_back_on_another_host() {
        // Wallet of Satoshi's answer, which cdk refuses: the callback is on
        // livingroomofsatoshi.com, not walletofsatoshi.com.
        let pay: LnurlPay = serde_json::from_str(
            r#"{"callback":"https://livingroomofsatoshi.com/api/v1/lnurl/payreq/x?k=v",
                "maxSendable":100000000000,"minSendable":1000,"tag":"payRequest",
                "metadata":"[]"}"#,
        )
        .expect("parse");
        let url = lnurl_callback(&pay, 28_000).expect("a callback");
        assert_eq!(url.host_str(), Some("livingroomofsatoshi.com"));
        assert_eq!(url.query(), Some("k=v&amount=28000"));

        let err = lnurl_callback(&pay, 500).expect_err("below its minimum");
        assert!(err.to_string().contains("at least 1 sat"), "{err}");
    }

    #[test]
    fn a_lightning_address_that_refuses_says_why() {
        let pay: LnurlPay =
            serde_json::from_str(r#"{"status":"ERROR","reason":"no such user"}"#).expect("parse");
        let err = lnurl_callback(&pay, 1000).expect_err("refused");
        assert_eq!(err.to_string(), "no such user");

        let plain: LnurlPay = serde_json::from_str(
            r#"{"callback":"http://example.org/cb","minSendable":1,"maxSendable":10000000}"#,
        )
        .expect("parse");
        assert!(lnurl_callback(&plain, 1000).is_err(), "not HTTPS");
    }

    #[test]
    fn a_destination_is_an_invoice_or_a_lightning_address() {
        assert_eq!(
            "lightning:LNBC10N1PXYZ"
                .parse::<Destination>()
                .expect("invoice"),
            Destination::Invoice("lnbc10n1pxyz".into())
        );
        assert_eq!(
            " Me@Example.org ".parse::<Destination>().expect("address"),
            Destination::Address("me@example.org".into())
        );
        for bad in ["", "me@localhost", "@example.org", "lnbc 1", "cashuAabc"] {
            assert!(bad.parse::<Destination>().is_err(), "{bad:?}");
        }
        let invoice = Destination::Invoice(format!("lnbc{}", "x".repeat(300)));
        assert!(
            invoice.to_string().len() < 40,
            "an invoice is shortened in logs"
        );
    }

    #[tokio::test]
    async fn taking_out_what_is_not_held_fails() {
        let (wallet, _dir) = wallet().await;
        assert!(
            wallet
                .export("https://mint.example", "sat", None)
                .await
                .is_err()
        );
        let to: Destination = "me@example.org".parse().expect("address");
        assert!(
            wallet
                .pay_out("https://mint.example", "sat", &to, Size::Within(u64::MAX))
                .await
                .is_err()
        );
        assert!(
            wallet
                .pay_out("https://mint.example", "sat", &to, Size::Exactly(0))
                .await
                .is_err()
        );
    }

    #[test]
    fn a_quote_left_reserved_is_recognised_by_its_type() {
        // What cdk's wallet returns when reserving a mint quote finds it
        // already reserved: the database error, converted by `?`.
        let reserved = cdk::Error::from(cdk::cdk_database::Error::QuoteAlreadyInUse);
        assert!(quote_in_use(&reserved));
        assert!(!quote_in_use(&cdk::Error::from(
            cdk::cdk_database::Error::UnknownQuote
        )));
        assert!(!quote_in_use(&cdk::Error::InsufficientFunds));
    }

    #[test]
    fn a_sent_token_stays_out_of_debug() {
        let sent = Sent {
            token: "cashuBsecret".into(),
            worth: 10,
            fee: 1,
        };
        assert!(!format!("{sent:?}").contains("cashuB"));
    }

    #[test]
    fn anything_but_the_resource_is_money() {
        // Money buys from whoever accepts its mint. Paper denominated in the
        // thing this node sells is somebody else's capacity, bought and not yet
        // spent, and only that somebody honours it.
        assert_eq!(Kind::of("sat", "byte"), Kind::Money);
        assert_eq!(Kind::of("usd", "byte"), Kind::Money);
        assert_eq!(Kind::of("byte", "byte"), Kind::PrepaidTransit);

        // And a node selling something else reads the same units differently:
        // to one selling watt-hours, a byte voucher is money like any other.
        assert_eq!(Kind::of("byte", "watt_hour"), Kind::Money);
    }

    #[test]
    fn a_node_never_holds_its_own_vouchers() {
        // Worth stating because the wallet's shape assumes it: a customer pays
        // in the paper this node issued, and redeeming that cancels this node's
        // own claim rather than adding to a balance. What selling puts in the
        // wallet is money, deposited at the market.
        //
        // So every `PrepaidTransit` holding is by construction some *other*
        // mint's, and there is no case where the issuer is this node.
        assert_eq!(Kind::of("byte", "byte"), Kind::PrepaidTransit);
    }
}
