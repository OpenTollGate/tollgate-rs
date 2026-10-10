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
use tollgate_net::config::{EnforcerKind, File};
use tollgate_net::node::Node;
use tracing::info;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::{FmtContext, FormatEvent, FormatFields};
use tracing_subscriber::registry::LookupSpan;

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

    /// This instance's name: letters, digits and `-`. Wins over `instance` in
    /// the file; unset there too, it is `default`. It names the runtime
    /// directory, `/run/tollgate-<instance>/`, that holds the instance's
    /// sockets, and every log line.
    #[arg(long)]
    instance: Option<String>,
}

/// Every log line, prefixed with the instance's name: one machine may run
/// several, and their logs often land in one place.
struct Instanced<F> {
    name: String,
    inner: F,
}

impl<S, N, F> FormatEvent<S, N> for Instanced<F>
where
    S: tracing::Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
    F: FormatEvent<S, N>,
{
    fn format_event(
        &self,
        ctx: &FmtContext<'_, S, N>,
        mut writer: Writer<'_>,
        event: &tracing::Event<'_>,
    ) -> std::fmt::Result {
        write!(writer, "[{}] ", self.name)?;
        self.inner.format_event(ctx, writer, event)
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    let file = match &args.config {
        Some(path) => File::load(path)?,
        None => serde_yaml::from_str("{}").expect("the empty document is valid"),
    };
    let instance = file.instance(args.instance.as_deref())?;

    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        // Colour only for a human at a terminal. Redirected to a file or a
        // pipe, escape codes land in the middle of every field and make the
        // output unreadable to anything that tries to parse it.
        .with_ansi(std::io::stderr().is_terminal())
        .event_format(Instanced {
            name: instance.clone(),
            inner: tracing_subscriber::fmt::format(),
        })
        .init();

    let config = file.resolve().context("resolve configuration")?;

    if args.show_identity {
        println!("pubkey:     {}", hex::encode(config.identity.pubkey().0));
        println!("secret_key: {}", config.identity.secret_hex());
        return Ok(());
    }

    // Created if it is missing and can be: it holds this instance's sockets.
    let runtime_dir = tollgate_net::instance::runtime_dir(&instance);
    info!(
        %instance,
        runtime_dir = %runtime_dir.display(),
        pubkey = %hex::encode(config.identity.pubkey().0),
        "starting"
    );

    // `pubkey` means the network proved each peer's key, and today only FIPS
    // does. The `fips` enforcer reaches the FIPS node itself; any other kind
    // has to find one here, or this node would be believing announced keys.
    if config.peer_identity == tollgate_net::wire::PeerIdentity::Pubkey
        && file.enforcer.kind != EnforcerKind::Fips
        && !tollgate_net::fips::running()
    {
        anyhow::bail!(
            "enforcer.identity is pubkey, but this node runs no FIPS, the only network \
             that proves a peer's key today; use identity: address, or run FIPS"
        );
    }

    // The mint is `mintd`, its own daemon: this node only advertises it and
    // settles at it. Nothing it issues is decided here.
    info!(url = %config.mint_url, local = %config.mint_local, "mint at");

    // What funds this node's channels: `merchantd`, which holds the money.
    // This node holds none, and asks for exactly what each channel needs, and
    // hands it whatever a settlement in a mint it keeps brings in.
    let merchant = file.merchant.socket_path();
    info!(merchant = %merchant.display(), "funding from");
    // Not fatal: merchantd may simply start after this. A deposit that finds
    // nobody there fails, and is retried with the settlement it came from.
    if file.vouchers.keeps_any(&config.mint_url) && !merchant.exists() {
        tracing::warn!(
            merchant = %merchant.display(),
            "an accepted mint is set to keep, and merchantd is not there to keep it in"
        );
    }

    // A byte source on the mesh. Off unless asked for: it is an instrument, and
    // an unauthenticated one, so a node that was never told to serve it should
    // not be.
    if file.speedtest.enabled {
        let (listen, settings) = file
            .speedtest
            .resolve()
            .context("resolve the speedtest configuration")?;
        tokio::spawn(async move {
            if let Err(e) =
                tollgate_net::speedtest::serve(settings, listen, std::future::pending()).await
            {
                tracing::error!(error = %format!("{e:#}"), "the speedtest stopped");
            }
        });
        info!(
            %listen,
            streams = file.speedtest.streams,
            "speedtest serving"
        );
    }

    let channels = Arc::new(
        SpilmanChannels::new(SpilmanConfig {
            mint_url: config.mint_url.clone(),
            mint_local: config.mint_local.clone(),
            unit: config.policy.unit.clone(),
            accepted_mints: config.policy.accepted_mints.clone(),
            burned_mints: file.vouchers.burned(),
            secret_key_hex: config.identity.secret_hex(),
            funding: Arc::new(tollgate_net::merchant::MerchantClient::new(merchant)),
            ttl_seconds: config.channel_ttl_seconds,
        })
        .context("build the channel backend")?,
    );

    // Which thing actually delivers. The loopback shaper carries a socket of
    // its own and forwards nobody's traffic; the kernel enforcer gates and
    // shapes the real forwarding path.
    let enforcer: Arc<dyn tollgate_net::enforcer::Enforcer> = match file.enforcer.kind {
        EnforcerKind::Loopback => {
            let loopback = Arc::new(tollgate_net::enforcer::Loopback::new());

            // The loopback data plane belongs to the loopback enforcer, so it is
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
        EnforcerKind::Ip => {
            let interface = file.enforcer.interface.clone();
            let enforcer = tollgate_net::enforcer::Ip::new(&interface).with_context(|| {
                format!("set up nftables and tc on {interface}; CAP_NET_ADMIN is required")
            })?;
            info!(%interface, "gating and shaping the kernel forwarding path");
            Arc::new(enforcer)
        }
        #[cfg(not(target_os = "linux"))]
        EnforcerKind::Ip => {
            anyhow::bail!("enforcer.kind: ip needs Linux")
        }
        #[cfg(unix)]
        EnforcerKind::Fips => {
            let socket = if file.enforcer.fips_socket.is_empty() {
                tollgate_net::enforcer::Fips::default_socket_path()
            } else {
                file.enforcer.fips_socket.clone().into()
            };
            let enforcer = tollgate_net::enforcer::Fips::new(&socket, config.policy.minimum_flow)
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
            Arc::new(enforcer)
        }
        #[cfg(not(unix))]
        EnforcerKind::Fips => {
            anyhow::bail!("enforcer.kind: fips needs a Unix control socket")
        }
        // A program of its own enforces, and this node tells it who has paid
        // for what. Nothing listens until it has said hello: an enforcer built
        // for another identity, or counting in another unit, is a reason not
        // to start at all.
        #[cfg(unix)]
        EnforcerKind::External => {
            let socket = file.enforcer.socket_path(&runtime_dir);
            info!(socket = %socket.display(), "waiting for the enforcer to say hello");
            let enforcer = tollgate_net::enforcer::External::connect(
                &socket,
                tollgate_net::enforcer::Expected {
                    identity: config.peer_identity,
                    unit: config.policy.unit.clone(),
                },
            )
            .await?;
            info!(
                socket = %socket.display(),
                identity = %config.peer_identity,
                "enforcing through the external enforcer"
            );
            Arc::new(enforcer)
        }
        #[cfg(not(unix))]
        EnforcerKind::External => {
            anyhow::bail!("enforcer.kind: external needs a Unix socket")
        }
    };

    let node = Node::new(&config, channels, enforcer.clone());

    // Anything watching this node reads here. A Unix socket rather than a port:
    // it is operational state for a local tool, not something a peer acts on.
    {
        let published = node.published();
        let path = file.control_socket_path(&runtime_dir);
        info!(control = %path.display(), "control socket");
        tokio::spawn(async move {
            if let Err(e) =
                tollgate_net::control::serve(&path, published, std::future::pending()).await
            {
                tracing::warn!(error = %format!("{e:#}"), "the control socket stopped");
            }
        });
    }

    // Demand is what we want to pull from a peer, which is what the buyer
    // reacts to; the shaper decides how much of it arrives.
    //
    // The flag wins over the file so that a run can be told to want something
    // else without editing what the service starts with — but the file is where
    // a node that should keep a link paid for says so, because the alternative
    // is editing a launchd plist or an init script to buy anything at all.
    let demand = if args.demand > 0 {
        args.demand
    } else {
        file.buying.demand
    };
    if demand > 0 || args.ramp > 0 {
        info!(demand, "wanting");
        let enforcer = Arc::clone(&enforcer);
        let (base, ramp, interval) = (demand, args.ramp, args.ramp_interval.max(1));
        tokio::spawn(async move {
            let mut steps = 0u64;
            loop {
                let demand = base.saturating_add(ramp.saturating_mul(steps));
                for peer in enforcer.peers() {
                    enforcer.set_demand(peer, demand);
                }
                tokio::time::sleep(Duration::from_secs(interval)).await;
                if ramp > 0 {
                    steps += 1;
                }
            }
        });
    }

    if args.report > 0 {
        let enforcer = Arc::clone(&enforcer);
        let period = Duration::from_secs(args.report);
        tokio::spawn(async move {
            let mut last: std::collections::HashMap<_, (u64, u64)> = Default::default();
            loop {
                tokio::time::sleep(period).await;
                for peer in enforcer.peers() {
                    let now = enforcer.counters(peer);
                    let (was_delivered, was_received) = last
                        .insert(peer, (now.to_payer, now.from_payer))
                        .unwrap_or((0, 0));

                    let secs = period.as_secs().max(1);
                    info!(
                        peer = %peer,
                        shaped = enforcer.shaping_rate(peer),
                        demand = enforcer.demand(peer),
                        down = (now.from_payer - was_received) / secs,
                        up = (now.to_payer - was_delivered) / secs,
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
