//! `merchantd`: the node's commercial side.
//!
//! It decides what this node's capacity sells for, sells it on the market
//! endpoints, buys what `tollgated` needs to pay its peers, and holds every
//! thing of value the node owns (`docs/design/core/tollgate-daemons.md`).
//! `tollgated` reaches it on a local socket with two calls — [`Request::Fund`]
//! and [`Request::Deposit`] — and an operator on another, through
//! `merchanttop`.

use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tracing::{debug, info};

use crate::control::Response;
use crate::market::{Accepted, Prices};
use crate::pricing::{self, Accept, Price, Rates, Source};
use crate::wallet::{Holding, TopUp, Wallet};

/// The whole of `merchant.yaml`.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct MerchantFile {
    /// Where the market endpoints are served over HTTP.
    pub listen: String,
    /// Local socket for [`Request::Fund`] and [`Request::Deposit`]. Only
    /// `tollgated` should be able to open it.
    pub socket: String,
    /// Local socket for `merchanttop`: prices and the wallet at runtime.
    pub control: String,
    /// Whether to serve the market endpoints at all.
    pub market: bool,
    /// This node's own mint.
    pub mint: MintSection,
    /// What a Mbit sells for, for accepted mints without a price of their own.
    pub price: Option<Price>,
    /// Where BTC prices come from.
    pub rates: RatesSection,
    /// Mints whose tokens buy this node's capacity.
    pub accepts: Vec<Accept>,
    /// Where the money is kept.
    pub wallet: WalletSection,
}

/// This node's own mint, as `merchantd` sees it.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct MintSection {
    /// The URL peers reach it on: what a buyer's vouchers are issued by.
    pub url: String,
    /// `mintd`'s private listener, where quotes are paid on creation.
    pub private: String,
    /// The unit its vouchers denominate in.
    pub unit: String,
    /// The most one sale may be for.
    pub max_amount: u64,
}

/// Where BTC prices come from.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct RatesSection {
    /// Tried in order; `{CUR}`/`{cur}` are templated per currency.
    pub sources: Vec<Source>,
    /// How often, in seconds.
    pub refresh_seconds: u64,
}

/// Where the money is kept.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct WalletSection {
    /// The wallet database. The file *is* the balance. Empty picks the state
    /// directory.
    pub file: String,
    /// The wallet's own secret, created on first start. Empty picks the state
    /// directory.
    pub seed_file: String,
    /// A mint that sells its paper for Lightning, for topping the wallet up.
    /// Empty means it cannot be topped up.
    pub mint: String,
    /// The unit of that mint.
    pub unit: String,
}

impl Default for MerchantFile {
    fn default() -> Self {
        Self {
            listen: "0.0.0.0:3340".into(),
            socket: String::new(),
            control: String::new(),
            market: true,
            mint: MintSection::default(),
            price: None,
            rates: RatesSection::default(),
            accepts: Vec::new(),
            wallet: WalletSection::default(),
        }
    }
}

impl Default for MintSection {
    fn default() -> Self {
        Self {
            url: "http://127.0.0.1:3338".into(),
            private: "http://127.0.0.1:3337".into(),
            unit: "byte".into(),
            max_amount: 1 << 40,
        }
    }
}

impl Default for RatesSection {
    fn default() -> Self {
        Self {
            sources: pricing::default_sources(),
            refresh_seconds: 300,
        }
    }
}

impl Default for WalletSection {
    fn default() -> Self {
        Self {
            file: String::new(),
            seed_file: String::new(),
            mint: "https://mint.minibits.cash/Bitcoin".into(),
            unit: "sat".into(),
        }
    }
}

impl MerchantFile {
    /// Read `merchant.yaml`.
    pub fn load(path: &Path) -> Result<Self> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        serde_yaml::from_str(&text).with_context(|| format!("parse {}", path.display()))
    }

    /// Where `tollgated` reaches this daemon.
    pub fn socket_path(&self) -> PathBuf {
        or_default(&self.socket, default_socket_path)
    }

    /// Where `merchanttop` reaches it.
    pub fn control_path(&self) -> PathBuf {
        or_default(&self.control, default_control_path)
    }

    /// Where the wallet lives.
    pub fn wallet_path(&self) -> PathBuf {
        or_default(&self.wallet.file, crate::wallet::default_path)
    }

    /// Where the wallet's secret lives.
    pub fn wallet_seed_path(&self) -> PathBuf {
        or_default(&self.wallet.seed_file, || {
            crate::config::state_file("wallet.seed")
        })
    }
}

fn or_default(path: &str, default: impl FnOnce() -> PathBuf) -> PathBuf {
    if path.is_empty() {
        default()
    } else {
        path.into()
    }
}

/// Where `merchantd` serves [`Request::Fund`] unless told otherwise.
pub fn default_socket_path() -> PathBuf {
    std::env::temp_dir().join("merchantd.sock")
}

/// Where `merchantd` serves its control socket unless told otherwise.
pub fn default_control_path() -> PathBuf {
    std::env::temp_dir().join("merchantd-control.sock")
}

/// What `merchantd` knows and holds, shared by its sockets and its market.
#[derive(Debug, Clone)]
pub struct Merchant {
    /// The price table the market sells by, recomputed from the prices below
    /// and the rates whenever either changes.
    pub prices: Prices,
    /// BTC in each fiat currency, as last fetched.
    pub rates: Rates,
    pricing: Arc<RwLock<(Option<Price>, Vec<Accept>)>>,
    /// Where the money is.
    pub wallet: Wallet,
    /// Where the wallet is topped up, if anywhere: a mint and its unit.
    pub money: Option<(String, String)>,
}

impl Merchant {
    /// A merchant selling at `price` for `accepts`, holding `wallet`.
    pub fn new(
        price: Option<Price>,
        accepts: Vec<Accept>,
        wallet: Wallet,
        money: Option<(String, String)>,
    ) -> Self {
        let merchant = Self {
            prices: Prices::default(),
            rates: Rates::default(),
            pricing: Arc::new(RwLock::new((price, accepts))),
            wallet,
            money,
        };
        merchant.reprice();
        merchant
    }

    /// Recompute the table the market sells by.
    pub fn reprice(&self) {
        let (price, accepts) = &*self.pricing.read().expect("not poisoned");
        let table = pricing::table(price.as_ref(), accepts, &self.rates);
        self.prices.replace(table);
    }

    /// The currencies a BTC price is needed for.
    pub fn currencies_needed(&self) -> Vec<String> {
        let (price, accepts) = &*self.pricing.read().expect("not poisoned");
        pricing::currencies_needed(price.as_ref(), accepts)
    }

    /// Change the default price, or one accepted mint's own.
    pub fn set_price(&self, mint: Option<&str>, price: Price) -> Result<()> {
        {
            let mut pricing = self.pricing.write().expect("not poisoned");
            match mint {
                None => pricing.0 = Some(price),
                Some(mint) => {
                    let entry = pricing
                        .1
                        .iter_mut()
                        .find(|a| a.mint.trim_end_matches('/') == mint.trim_end_matches('/'))
                        .ok_or_else(|| anyhow!("{mint} is not an accepted mint"))?;
                    entry.price = Some(price);
                }
            }
        }
        self.reprice();
        Ok(())
    }

    /// What there is to show about it.
    pub async fn snapshot(&self) -> Snapshot {
        let (price, accepts) = self.pricing.read().expect("not poisoned").clone();
        Snapshot {
            price,
            accepts: accepts.clone(),
            table: self.prices.listed(),
            rates: self.rates.all(),
            holdings: self.wallet.balances().await,
            money_mint: self.money.as_ref().map(|(mint, unit)| Accepted {
                mint: mint.clone(),
                unit: unit.clone(),
                bytes_per_unit: 0,
            }),
        }
    }
}

/// What `merchantd` publishes on its control socket.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Snapshot {
    /// The default price.
    pub price: Option<Price>,
    /// The accepted mints, with their own prices where they have one.
    pub accepts: Vec<Accept>,
    /// What a unit of each accepted mint's tokens buys right now.
    pub table: Vec<Accepted>,
    /// BTC per fiat currency, as last fetched.
    pub rates: std::collections::BTreeMap<String, f64>,
    /// What is held, money first.
    pub holdings: Vec<Holding>,
    /// Where the wallet is topped up.
    pub money_mint: Option<Accepted>,
}

/// A line of JSON asking `merchantd` for something.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "command", content = "params", rename_all = "snake_case")]
pub enum Request {
    /// Vouchers of `mint` to fund a channel with. `tollgated`'s socket only.
    Fund {
        /// The mint whose paper the peer takes.
        mint: String,
        /// Its unit.
        unit: String,
        /// Exactly how much.
        amount: u64,
    },
    /// Something of value `tollgated` received and does not keep: proceeds of
    /// a channel settled in another mint, or change. `tollgated`'s socket only.
    Deposit {
        /// The token.
        token: String,
    },
    /// Everything there is to show. The control socket's default.
    Snapshot,
    /// Change the default price, or one accepted mint's.
    SetPrice {
        /// The mint, or none for the default.
        #[serde(default)]
        mint: Option<String>,
        /// The new price.
        price: Price,
    },
    /// Buy money, and hand back the invoice that pays for it.
    TopUp {
        /// How much.
        amount: u64,
    },
    /// Claim a top-up that was paid but not collected.
    Claim,
}

/// Which socket a request arrived on, and so what it may ask.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Socket {
    /// `tollgated`'s: [`Request::Fund`] and [`Request::Deposit`].
    Funding,
    /// The operator's: everything else.
    Control,
}

/// Serve `socket` until the process ends.
pub async fn serve(path: &Path, merchant: Merchant, socket: Socket) -> Result<()> {
    let _ = std::fs::remove_file(path);
    let listener = UnixListener::bind(path)
        .with_context(|| format!("bind {} at {}", socket_name(socket), path.display()))?;
    info!(path = %path.display(), socket = socket_name(socket), "serving");
    loop {
        let (stream, _) = match listener.accept().await {
            Ok(pair) => pair,
            Err(e) => {
                debug!(error = %e, "a connection failed to accept");
                continue;
            }
        };
        let merchant = merchant.clone();
        tokio::spawn(async move {
            if let Err(e) = answer(stream, merchant, socket).await {
                debug!(error = %format!("{e:#}"), "a request failed");
            }
        });
    }
}

fn socket_name(socket: Socket) -> &'static str {
    match socket {
        Socket::Funding => "funding socket",
        Socket::Control => "control socket",
    }
}

/// Read one line, write one answer. A caller on the control socket that says
/// nothing gets the snapshot.
async fn answer(stream: tokio::net::UnixStream, merchant: Merchant, socket: Socket) -> Result<()> {
    let (rx, mut tx) = stream.into_split();
    let mut line = String::new();
    let _ = tokio::time::timeout(
        std::time::Duration::from_millis(150),
        BufReader::new(rx).read_line(&mut line),
    )
    .await;
    let request = match line.trim() {
        "" => Ok(Request::Snapshot),
        text => serde_json::from_str::<Request>(text),
    };
    let response = match request {
        Ok(request) => run(request, &merchant, socket).await,
        Err(e) => Response::Error {
            message: format!("not a request merchantd understands: {e}"),
        },
    };
    let mut body = serde_json::to_vec(&response)?;
    body.push(b'\n');
    tx.write_all(&body).await?;
    tx.shutdown().await?;
    Ok(())
}

fn ok(data: impl Serialize) -> Response {
    match serde_json::to_value(data) {
        Ok(data) => Response::Ok { data },
        Err(e) => Response::Error {
            message: e.to_string(),
        },
    }
}

fn error(e: impl std::fmt::Display) -> Response {
    Response::Error {
        message: e.to_string(),
    }
}

async fn run(request: Request, merchant: &Merchant, socket: Socket) -> Response {
    let funding = matches!(request, Request::Fund { .. } | Request::Deposit { .. });
    if funding != (socket == Socket::Funding) {
        return error(format!("not a request for the {}", socket_name(socket)));
    }
    match request {
        Request::Fund { mint, unit, amount } => {
            // The need is tollgated's; whether it is worth paying for is this
            // daemon's call. Today every upstream that gives its vouchers away
            // is worth it, and buying from one that sells is still to come.
            match crate::wallet::fund_from(&merchant.wallet, &mint, &unit, amount).await {
                Ok(token) => {
                    info!(%mint, %unit, amount, "funded a channel");
                    ok(serde_json::json!({ "token": token }))
                }
                Err(e) => error(format!("{e:#}")),
            }
        }
        Request::Deposit { token } => match merchant.wallet.deposit(&token).await {
            Ok(held) => {
                info!(mint = %held.mint, unit = %held.unit, amount = held.amount, "deposited");
                ok(held)
            }
            Err(e) => error(format!("{e:#}")),
        },
        Request::Snapshot => ok(merchant.snapshot().await),
        Request::SetPrice { mint, price } => {
            match merchant.set_price(mint.as_deref(), price.clone()) {
                Ok(()) => {
                    info!(mint = mint.as_deref().unwrap_or("default"), unit = %price.unit, per_mbit = price.per_mbit, "a price was changed");
                    ok(merchant.snapshot().await)
                }
                Err(e) => error(format!("{e:#}")),
            }
        }
        Request::TopUp { amount } => match &merchant.money {
            Some((mint, unit)) => top_up(merchant.wallet.clone(), mint, unit, amount).await,
            None => error("no wallet.mint is configured to top up at"),
        },
        Request::Claim => match &merchant.money {
            Some((mint, unit)) => match merchant.wallet.claim_pending(mint, unit).await {
                Ok(claimed) => {
                    ok(serde_json::json!({ "claimed": claimed, "unit": unit, "mint": mint }))
                }
                Err(e) => error(format!("{e:#}")),
            },
            None => error("no wallet.mint is configured to claim at"),
        },
    }
}

/// Buy money, and collect it in the background once the invoice is paid.
async fn top_up(wallet: Wallet, mint: &str, unit: &str, amount: u64) -> Response {
    let top_up = match wallet.top_up(mint, unit, amount).await {
        Ok(top_up) => top_up,
        Err(e) => return error(format!("{e:#}")),
    };
    tokio::spawn({
        let top_up: TopUp = top_up.clone();
        async move {
            // Backing off: a public mint rate-limits a status poll a second,
            // and a person paying an invoice takes tens of seconds anyway.
            let mut wait = std::time::Duration::from_secs(2);
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(600);
            while std::time::Instant::now() < deadline {
                tokio::time::sleep(wait).await;
                wait = (wait * 2).min(std::time::Duration::from_secs(30));
                if let Ok(minted) = wallet.collect(&top_up).await {
                    info!(minted, unit = %top_up.unit, "top-up collected");
                    return;
                }
            }
            tracing::warn!(quote = %top_up.quote, "gave up waiting for a top-up; if it was paid, claim it");
        }
    });
    ok(top_up)
}

/// `tollgated`'s end of the funding socket.
#[derive(Debug, Clone)]
pub struct MerchantClient {
    path: PathBuf,
}

impl MerchantClient {
    /// Reach `merchantd` at `path`.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// One request, one answer, blocking.
    fn call(&self, request: &Request) -> Result<serde_json::Value> {
        use std::io::{Read, Write};
        let mut stream = std::os::unix::net::UnixStream::connect(&self.path)
            .with_context(|| format!("reach merchantd at {}", self.path.display()))?;
        let mut line = serde_json::to_vec(request)?;
        line.push(b'\n');
        stream.write_all(&line)?;
        stream.shutdown(std::net::Shutdown::Write)?;
        let mut body = Vec::new();
        stream.read_to_end(&mut body)?;
        match serde_json::from_slice::<Response>(&body).context("read merchantd's answer")? {
            Response::Ok { data } => Ok(data),
            Response::Error { message } => Err(anyhow!("merchantd: {message}")),
        }
    }
}

impl crate::channel::Funding for MerchantClient {
    fn vouchers(&self, mint: &str, unit: &str, amount: u64) -> Result<String> {
        let data = self.call(&Request::Fund {
            mint: mint.to_owned(),
            unit: unit.to_owned(),
            amount,
        })?;
        data["token"]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| anyhow!("merchantd funded nothing: {data}"))
    }

    fn deposit(&self, token: &str) -> Result<()> {
        self.call(&Request::Deposit {
            token: token.to_owned(),
        })
        .map(|_| ())
    }
}

/// Ask `merchantd`'s control socket for something, from `merchanttop`.
pub async fn send(path: &Path, request: &Request) -> Result<Response> {
    let body = crate::control::exchange(path, Some(serde_json::to_string(request)?)).await?;
    serde_json::from_str(&body).with_context(|| format!("parse the answer: {body}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_file_sells_nothing_and_listens_where_the_docs_say() {
        let file: MerchantFile = serde_yaml::from_str("{}").expect("parse");
        assert_eq!(file.listen, "0.0.0.0:3340");
        assert!(file.accepts.is_empty());
        assert!(file.price.is_none());
        assert_eq!(file.mint.private, "http://127.0.0.1:3337");
    }

    #[test]
    fn a_price_and_per_issuer_prices_parse() {
        let file: MerchantFile = serde_yaml::from_str(
            "price: { unit: usd, per_mbit: 0.00001 }\n\
             accepts:\n\
             - { mint: 'https://a', unit: sat, price: { unit: sat, per_mbit: 0.8 } }\n\
             - { mint: 'https://b', unit: usd }\n",
        )
        .expect("parse");
        assert_eq!(file.accepts.len(), 2);
        assert_eq!(
            file.accepts[0].price.as_ref().map(|p| p.per_mbit),
            Some(0.8)
        );
        assert!(file.accepts[1].price.is_none());
    }

    #[test]
    fn the_configs_the_packages_install_are_valid() {
        for (name, text) in [
            (
                "openwrt",
                include_str!("../../../packaging/openwrt-ipk/files/etc/tollgate/merchant.yaml"),
            ),
            (
                "macos",
                include_str!("../../../packaging/macos/merchant.yaml"),
            ),
        ] {
            let file: MerchantFile = serde_yaml::from_str(text)
                .unwrap_or_else(|e| panic!("the {name} merchant config does not parse: {e}"));
            assert!(Path::new(&file.wallet.file).is_absolute(), "{name}");
            assert!(Path::new(&file.wallet.seed_file).is_absolute(), "{name}");
        }
    }

    #[tokio::test]
    async fn setting_a_price_reprices_the_table() {
        let dir = crate::tempdir::Dir::new();
        let wallet = Wallet::open(dir.path().join("w.sqlite"), [1; 64], "byte")
            .await
            .expect("wallet");
        let merchant = Merchant::new(
            None,
            vec![Accept {
                mint: "https://a".into(),
                unit: "sat".into(),
                price: None,
            }],
            wallet,
            None,
        );
        assert!(
            merchant.prices.listed().is_empty(),
            "no price, nothing sold"
        );

        merchant
            .set_price(
                None,
                Price {
                    unit: "sat".into(),
                    per_mbit: 1.0,
                },
            )
            .expect("set");
        assert_eq!(
            merchant.prices.bytes_per_unit("https://a", "sat"),
            Some(125_000)
        );

        assert!(
            merchant
                .set_price(
                    Some("https://nope"),
                    Price {
                        unit: "sat".into(),
                        per_mbit: 1.0
                    }
                )
                .is_err()
        );
    }
}
