//! `tollgate` — the TollGate node binary.
//!
//! The first deployment target is a plain IP network in **bootstrap-only mode**:
//! peers pay with ordinary Cashu tokens and get metered access. Spilman payment
//! channels and FIPS mesh integration are layered on later; see `docs/design/`.

mod adapter;
mod config;
mod driver;
mod server;
mod wallet;

use std::path::PathBuf;

use clap::Parser;

#[derive(Parser, Debug)]
#[command(name = "tollgate", version, about = "Sell metered network access with Cashu micropayments")]
struct Cli {
    /// Path to a config file. If omitted, the standard locations are searched.
    #[arg(short, long)]
    config: Option<PathBuf>,
    /// Override the listen address for the HTTP/WS transport.
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

    let identity = config::Identity::load_or_generate(&cfg)?;
    tracing::info!(pubkey = %identity.pubkey_hex(), listen = %cfg.listen, "starting tollgate node");

    // IP deployment: access control + metering via the host OS firewall.
    let _adapter = adapter::IpAdapter::new();
    // Bootstrap-only wallet (plain Cashu tokens). Spilman channels come later.
    let _wallet = wallet::BootstrapWallet::new(cfg.mints.clone());
    // The driver bridges transport/adapter/wallet <-> the core Session.
    let _driver = driver::Driver::new();

    server::serve(&cfg.listen).await
}
