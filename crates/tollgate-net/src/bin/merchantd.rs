//! The merchant daemon: the node's commercial side.
//!
//! Prices this node's capacity, sells it on the market endpoints — taking
//! payment into its wallet and having `mintd` issue what it sold — buys what
//! `tollgated` needs to fund its channels, and holds everything of value the
//! node owns. Reads `merchant.yaml`; see `docs/design/core/tollgate-daemons.md`.

use std::io::IsTerminal;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use tollgate_net::market::{self, Issuer};
use tollgate_net::merchant::{self, Merchant, MerchantFile, Socket};
use tollgate_net::pricing;
use tollgate_net::wallet::Wallet;
use tracing::info;
use tracing_subscriber::EnvFilter;

#[derive(Parser, Debug)]
#[command(name = "merchantd", about = "A TollGate node's merchant")]
struct Args {
    /// Configuration file. Defaults are used if absent.
    #[arg(short, long)]
    config: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .with_ansi(std::io::stderr().is_terminal())
        .init();

    let args = Args::parse();
    let file = match &args.config {
        Some(path) => MerchantFile::load(path)?,
        None => serde_yaml::from_str("{}").expect("the empty document is valid"),
    };

    // The wallet's own secret: this daemon has no identity of its own to
    // derive one from, and the money should not be recoverable from anything
    // `tollgated` holds.
    let seed = tollgate_net::mint::load_or_create_secret(&file.wallet_seed_path(), 64)?;
    let seed: [u8; 64] = seed.try_into().expect("64 bytes");
    let wallet = Wallet::open(file.wallet_path(), seed, file.mint.unit.clone())
        .await
        .context("open the wallet")?;
    info!(wallet = %file.wallet_path().display(), "holding");

    let money = (!file.wallet.mint.is_empty())
        .then(|| (file.wallet.mint.clone(), file.wallet.unit.clone()));
    let merchant = Merchant::new(file.price.clone(), file.accepts.clone(), wallet, money);

    // Rates only for currencies a price actually needs converting through: a
    // sat price paid in sats fetches nothing.
    let currencies = merchant.currencies_needed();
    if !currencies.is_empty() {
        let (rates, repricer) = (merchant.rates.clone(), merchant.clone());
        tokio::spawn(pricing::keep_fetching(
            rates,
            file.rates.sources.clone(),
            currencies,
            Duration::from_secs(file.rates.refresh_seconds.max(10)),
            move || repricer.reprice(),
        ));
    }
    for row in merchant.prices.listed() {
        info!(mint = %row.mint, unit = %row.unit, bytes_per_unit = row.bytes_per_unit, "selling for");
    }

    if file.market {
        let router = market::router(
            Issuer::new(&file.mint.private, &file.mint.unit),
            file.mint.unit.clone(),
            file.mint.url.clone(),
            merchant.prices.clone(),
            merchant.wallet.clone(),
            file.mint.max_amount.max(1),
        );
        let listener = tokio::net::TcpListener::bind(&file.listen)
            .await
            .with_context(|| format!("bind the market on {}", file.listen))?;
        info!(listen = %file.listen, issuing_at = %file.mint.private, "market serving");
        tokio::spawn(async move {
            if let Err(e) = axum::serve(listener, router).await {
                tracing::error!(error = %e, "the market stopped");
            }
        });
    }

    let control = {
        let merchant = merchant.clone();
        let path = file.control_path();
        tokio::spawn(async move { merchant::serve(&path, merchant, Socket::Control).await })
    };
    let funding = {
        let path = file.socket_path();
        tokio::spawn(async move { merchant::serve(&path, merchant, Socket::Funding).await })
    };

    tokio::select! {
        r = control => r.context("the control socket")?.context("the control socket"),
        r = funding => r.context("the funding socket")?.context("the funding socket"),
        _ = interrupted() => Ok(()),
    }
}

/// Resolves on the first interrupt or termination signal.
async fn interrupted() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut terminate) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = terminate.recv() => {}
                }
            }
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
