//! The TollGate daemon.
//!
//! Reads a YAML configuration, brings up the control and data planes, and runs
//! until interrupted. `--demand` drives the traffic generator, which is what
//! makes the buying algorithm do anything visible.

use std::io::IsTerminal;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use tollgate_net::channel::{SpilmanChannels, SpilmanConfig};
use tollgate_net::config::{File, ForwardingMode};
use tollgate_net::mint::{self, MintConfig};
use tollgate_net::node::Node;
use tracing::info;
use tracing_subscriber::EnvFilter;

#[derive(Parser, Debug)]
#[command(name = "tollgated", about = "A TollGate node")]
struct Args {
    /// Configuration file. Defaults are used if absent.
    #[arg(short, long)]
    config: Option<PathBuf>,

    /// Print the identity that would be used, then exit.
    ///
    /// Peers are dialed by public key, so bringing up a pair means knowing each
    /// other's keys before either starts.
    #[arg(long)]
    show_identity: bool,

    /// Traffic to generate towards every peer, in bytes per second.
    ///
    /// This is the demand the buying algorithm reacts to.
    #[arg(long, default_value_t = 0)]
    demand: u64,

    /// Step demand up by this many bytes per second every `--ramp-interval`,
    /// so a demo can show the purchased rate climbing to meet it.
    #[arg(long, default_value_t = 0)]
    ramp: u64,

    /// How often to apply `--ramp`.
    #[arg(long, default_value_t = 5)]
    ramp_interval: u64,

    /// Report throughput and what has been bought, this often, in seconds.
    /// Zero turns the report off.
    #[arg(long, default_value_t = 1)]
    report: u64,

    /// Where to serve the control socket that `tolltop` reads.
    #[arg(long)]
    control_socket: Option<PathBuf>,
}

/// Derive the mint's keyset seed from the node's identity.
///
/// Deterministic, so a restart keeps issuing against the same keys and the
/// vouchers a peer already holds stay redeemable. Hashed rather than used
/// directly so the mint's key material is not the node's signing key.
fn mint_seed(secret_hex: &str) -> Result<Vec<u8>> {
    use sha2::{Digest, Sha256};
    let secret = hex::decode(secret_hex).context("identity key is not hex")?;
    let mut hasher = Sha256::new();
    hasher.update(b"tollgate-mint-seed");
    hasher.update(&secret);
    Ok(hasher.finalize().to_vec())
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        // Colour only for a human at a terminal. Redirected to a file or a
        // pipe, escape codes land in the middle of every field and make the
        // output unreadable to anything that tries to parse it.
        .with_ansi(std::io::stderr().is_terminal())
        .init();

    let args = Args::parse();

    let file = match &args.config {
        Some(path) => File::load(path)?,
        None => serde_yaml::from_str("{}").expect("the empty document is valid"),
    };
    let config = file.resolve().context("resolve configuration")?;

    if args.show_identity {
        println!("pubkey:     {}", hex::encode(config.identity.pubkey().0));
        println!("secret_key: {}", config.identity.secret_hex());
        return Ok(());
    }

    info!(
        pubkey = %hex::encode(config.identity.pubkey().0),
        "starting"
    );

    // What this node sells its capacity for. Held here rather than inside the
    // mint because three things share it: the mint prices its quotes with it,
    // the market publishes it, and the control socket changes it while the node
    // runs.
    let price = tollgate_net::market::Price::new(file.market.bytes_per_sat);

    // The mint comes up first. A peer funds its channel against *our* mint, so
    // nothing can be paid for until it is serving.
    let mint = Arc::new(
        mint::build(&MintConfig {
            url: config.mint_url.clone(),
            unit: config.policy.unit.clone(),
            // Derived from the identity so a restart keeps issuing against the
            // same keys, and two nodes never share a keyset.
            seed: mint_seed(&config.identity.secret_hex())?,
            max_amount: config.policy.initial_channel_capacity.max(1),
            price: price.clone(),
        })
        .await
        .context("bring up this node's mint")?,
    );

    {
        let mint = Arc::clone(&mint);
        let listen = config.mint_listen;
        let unit = config.policy.unit.clone();
        let price = price.clone();
        tokio::spawn(async move {
            if let Err(e) = mint::serve(mint, unit, price, listen, std::future::pending()).await {
                tracing::error!(error = %e, "the mint stopped");
            }
        });
    }
    info!(
        url = %config.mint_url,
        listen = %config.mint_listen,
        unit = %config.policy.unit,
        "mint serving"
    );

    info!(
        bytes_per_sat = file.market.bytes_per_sat,
        "selling capacity"
    );

    // The mint prices its quotes and will not issue until they are paid — but
    // what pays them is a simulated Lightning backend that settles its own
    // invoices. So the price is real and the payment is not, which makes this
    // a give-away to anyone who can reach it.
    if !config.mint_listen.ip().is_loopback() {
        tracing::warn!(
            listen = %config.mint_listen,
            "quotes are settled by a simulated wallet, so vouchers are effectively free to anyone who can reach this"
        );
    }

    let channels = Arc::new(
        SpilmanChannels::new(SpilmanConfig {
            mint: Arc::clone(&mint),
            mint_url: config.mint_url.clone(),
            unit: config.policy.unit.clone(),
            accepted_mints: config.policy.accepted_mints.clone(),
            secret_key_hex: config.identity.secret_hex(),
        })
        .context("build the channel backend")?,
    );

    // Which thing actually delivers. The loopback shaper carries a socket of
    // its own and forwards nobody's traffic; the kernel adapter gates and
    // shapes the real forwarding path.
    let adapter: Arc<dyn tollgate_net::adapter::ResourceAdapter> = match file.forwarding.mode {
        ForwardingMode::Loopback => {
            let loopback = Arc::new(tollgate_net::adapter::Loopback::new());

            // The loopback data plane belongs to the loopback adapter, so it is
            // started here rather than by the node.
            let data = tokio::net::TcpListener::bind(config.data_listen())
                .await
                .with_context(|| format!("bind the data plane on {}", config.data_listen()))?;
            tokio::spawn(tollgate_net::dataplane::listen(data, loopback.clone()));
            for peer in &config.peers {
                // Only toward a peer we dial. One that dials us brings its own
                // data-plane connection with it.
                if let Some(endpoint) = &peer.endpoint {
                    tollgate_net::dataplane::keep_dialing(
                        endpoint.clone(),
                        config.identity.pubkey(),
                        peer.pubkey,
                        loopback.clone(),
                    );
                }
            }
            info!(listen = %config.data_listen(), "loopback data plane");
            loopback
        }
        #[cfg(target_os = "linux")]
        ForwardingMode::Nftables => {
            let interface = file.forwarding.interface.clone();
            let adapter = tollgate_net::adapter::Nftables::new(&interface).with_context(|| {
                format!("set up nftables and tc on {interface}; CAP_NET_ADMIN is required")
            })?;
            info!(%interface, "gating and shaping the kernel forwarding path");
            Arc::new(adapter)
        }
        #[cfg(not(target_os = "linux"))]
        ForwardingMode::Nftables => {
            anyhow::bail!("forwarding.mode: nftables needs Linux")
        }
        #[cfg(unix)]
        ForwardingMode::Fips => {
            let socket = if file.forwarding.fips_socket.is_empty() {
                tollgate_net::adapter::Fips::default_socket_path()
            } else {
                file.forwarding.fips_socket.clone().into()
            };
            let adapter = tollgate_net::adapter::Fips::new(&socket, config.policy.minimum_flow)
                .with_context(|| {
                    format!(
                        "reach the FIPS control socket at {}; is fipsd running?",
                        socket.display()
                    )
                })?;
            info!(
                socket = %socket.display(),
                allowance = config.policy.minimum_flow,
                "setting transit policy on the FIPS node"
            );
            Arc::new(adapter)
        }
        #[cfg(not(unix))]
        ForwardingMode::Fips => {
            anyhow::bail!("forwarding.mode: fips needs a Unix control socket")
        }
    };

    let node = Node::new(&config, channels, adapter.clone());

    // Anything watching this node reads here. A Unix socket rather than a port:
    // it is operational state for a local tool, not something a peer acts on.
    {
        let published = node.published();
        let path = args
            .control_socket
            .clone()
            .unwrap_or_else(tollgate_net::control::default_socket_path);
        let price = price.clone();
        tokio::spawn(async move {
            if let Err(e) =
                tollgate_net::control::serve(&path, published, price, std::future::pending()).await
            {
                tracing::warn!(error = %format!("{e:#}"), "the control socket stopped");
            }
        });
    }

    // The traffic generator. Demand is what we want to pull from a peer, which
    // is what the buyer reacts to; the shaper decides how much of it arrives.
    if args.demand > 0 || args.ramp > 0 {
        let adapter = Arc::clone(&adapter);
        let (base, ramp, interval) = (args.demand, args.ramp, args.ramp_interval.max(1));
        tokio::spawn(async move {
            let mut steps = 0u64;
            loop {
                let demand = base.saturating_add(ramp.saturating_mul(steps));
                for peer in adapter.peers() {
                    adapter.set_demand(peer, demand);
                }
                tokio::time::sleep(Duration::from_secs(interval)).await;
                if ramp > 0 {
                    steps += 1;
                }
            }
        });
    }

    if args.report > 0 {
        let adapter = Arc::clone(&adapter);
        let period = Duration::from_secs(args.report);
        tokio::spawn(async move {
            let mut last: std::collections::HashMap<_, (u64, u64)> = Default::default();
            loop {
                tokio::time::sleep(period).await;
                for peer in adapter.peers() {
                    let now = adapter.counters(peer);
                    let (was_delivered, was_received) = last
                        .insert(peer, (now.delivered, now.received))
                        .unwrap_or((0, 0));

                    let secs = period.as_secs().max(1);
                    info!(
                        peer = %peer,
                        shaped = adapter.shaping_rate(peer),
                        demand = adapter.demand(peer),
                        down = (now.received - was_received) / secs,
                        up = (now.delivered - was_delivered) / secs,
                        "link"
                    );
                }
            }
        });
    }

    node.run(config, interrupted()).await
}

/// Resolves on the first interrupt or termination signal.
///
/// SIGTERM as well as Ctrl-C, because a container is stopped with the former
/// and a node that ignored it would be killed mid-session.
async fn interrupted() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut terminate = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, "no SIGTERM handler; Ctrl-C only");
                let _ = tokio::signal::ctrl_c().await;
                return;
            }
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = terminate.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
