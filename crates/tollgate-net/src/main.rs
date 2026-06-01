//! `tollgate` — the TollGate node binary.
//!
//! First deployment target: plain IP network in **bootstrap-only mode**. Peers
//! pay with ordinary Cashu tokens and get metered access. Spilman channels and
//! FIPS integration are layered on later.

mod adapter;
mod client;
mod config;
mod driver;
mod server;
mod wallet;

use std::path::PathBuf;
use std::sync::Arc;

use clap::{Parser, Subcommand};

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
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Run the node: listen for peers and sell metered access. (Default.)
    Serve {
        /// Override the listen address (default: 127.0.0.1:4747).
        #[arg(long)]
        listen: Option<String>,
    },
    /// Probe a peer: send our Announce and report the peer's identity.
    Connect {
        /// Peer HTTP origin, e.g. http://gateway:4747
        #[arg(long)]
        peer: String,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let cli = Cli::parse();
    let cfg = config::Config::load(cli.config.as_deref())?;
    let identity = Arc::new(config::Identity::load_or_generate(&cfg)?);

    match cli.command.unwrap_or(Command::Serve { listen: None }) {
        Command::Serve { listen } => serve(cfg, identity, listen).await,
        Command::Connect { peer } => connect(&cfg, &identity, &peer).await,
    }
}

async fn serve(
    mut cfg: config::Config,
    identity: Arc<config::Identity>,
    listen: Option<String>,
) -> anyhow::Result<()> {
    if let Some(listen) = listen {
        cfg.listen = listen;
    }
    tracing::info!(pubkey = %identity.pubkey_hex(), listen = %cfg.listen, "starting tollgate node");

    let wallet = wallet::BootstrapWallet::new(cfg.mints.clone());
    let adapter = adapter::IpAdapter::new();
    if let Err(e) = adapter.init(cfg.firewall.installs_forward_chain()) {
        tracing::warn!(err = %e, "firewall init failed; access may not be enforced (need root?)");
    }

    // v1: one price for all peers, taken from the first configured product.
    let price = cfg
        .products
        .first()
        .map(|p| tollgate_core::Price {
            per_second: p.price_per_second,
            per_unit: p.price_per_unit,
        })
        .unwrap_or_default();

    let driver = driver::Driver::new(wallet, adapter, identity, price, cfg.unit.clone());
    driver.spawn_metering(std::time::Duration::from_secs(5));

    server::serve(&cfg.listen, driver).await
}

async fn connect(
    cfg: &config::Config,
    identity: &config::Identity,
    peer: &str,
) -> anyhow::Result<()> {
    tracing::info!(pubkey = %identity.pubkey_hex(), %peer, "probing peer");
    let detected = client::detect(peer, identity, &cfg.unit).await?;
    tracing::info!(
        peer_pubkey = %detected.pubkey_hex,
        peer_unit = %detected.unit,
        peer_version = detected.version,
        "detected peer"
    );
    // Machine-readable line for test harnesses to grep.
    println!(
        "DETECTED peer={} unit={} version={}",
        detected.pubkey_hex, detected.unit, detected.version
    );
    Ok(())
}
