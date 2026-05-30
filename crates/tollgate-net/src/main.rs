//! `tollgate` — the TollGate node binary.
//!
//! First deployment target: plain IP network in **bootstrap-only mode**. Peers
//! pay with ordinary Cashu tokens and get metered access. Spilman channels and
//! FIPS integration are layered on later.

mod adapter;
mod config;
mod driver;
mod server;
mod wallet;

use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;

#[derive(Parser, Debug)]
#[command(
    name = "tollgate",
    version,
    about = "Sell metered network access with Cashu micropayments"
)]
struct Cli {
    /// Path to a config file. Searched in order if omitted:
    /// ./tollgate.yaml, ~/.config/tollgate/tollgate.yaml, /etc/tollgate/tollgate.yaml
    #[arg(short, long)]
    config: Option<PathBuf>,
    /// Override the listen address (default: 127.0.0.1:4747).
    #[arg(long)]
    listen: Option<String>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let cli = Cli::parse();
    let mut cfg = config::Config::load(cli.config.as_deref())?;
    if let Some(listen) = cli.listen {
        cfg.listen = listen;
    }

    let identity = Arc::new(config::Identity::load_or_generate(&cfg)?);
    tracing::info!(pubkey = %identity.pubkey_hex(), listen = %cfg.listen, "starting tollgate node");

    let wallet = wallet::BootstrapWallet::new(cfg.mints.clone());
    let adapter = adapter::IpAdapter::new();
    if let Err(e) = adapter.init() {
        tracing::warn!(err = %e, "firewall init failed; access may not be enforced (need root?)");
    }
    let driver = driver::Driver::new(wallet, adapter, identity);

    server::serve(&cfg.listen, driver).await
}
