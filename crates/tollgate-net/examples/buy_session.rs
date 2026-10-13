//! A buyer session, run through the library: what a program embeds to buy
//! access from a gateway on somebody else's behalf.
//!
//! So that it runs on its own, the example also starts the gateway it buys
//! from, in the same process: a node of its own ([`Node`]) with a mint that
//! gives its vouchers away, and a control socket. The session is
//! [`client::run`]. Every second the example reads the gateway's control
//! socket and prints what the gateway holds for the session: the rate it
//! carries it at, the budget left and the rate it reserved.
//!
//! ```text
//! cargo run -p tollgate-net --example buy_session                  # time at a speed
//! cargo run -p tollgate-net --example buy_session -- --pay-per-use # pay for what you use
//! cargo run -p tollgate-net --example buy_session -- --seconds 30
//! ```
//!
//! Nothing here moves real money: the gateway's mint gives vouchers to anyone
//! who asks for them (`auto_accept`), as the test topologies' mints do.

use std::net::{SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tokio::sync::watch;
use tollgate_net::Identity;
use tollgate_net::channel::{SpilmanChannels, SpilmanConfig};
use tollgate_net::client::{self, Buying, ClientSpec, Moved};
use tollgate_net::config::File;
use tollgate_net::control;
use tollgate_net::enforcer::Loopback;
use tollgate_net::mint::{self, IssueLimit, MintConfig};
use tollgate_net::node::Node;
use tollgate_net::wallet::{Wallet, WalletFunding};

/// The speed the session asks for, in bytes per second.
const RATE: u64 = 1_000_000;

/// With `--pay-per-use`: the budget the session holds with the gateway.
const PAY_PER_USE_BUDGET: u64 = 5_000_000;

/// With `--pay-per-use`: what the pretend device moves through the gateway
/// each second, in bytes.
const DEVICE_MOVES: u64 = 400_000;

struct Args {
    pay_per_use: bool,
    seconds: u64,
}

fn parse_args() -> Result<Args> {
    let mut args = Args {
        pay_per_use: false,
        seconds: 15,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--pay-per-use" => args.pay_per_use = true,
            "--seconds" => {
                args.seconds = it
                    .next()
                    .context("--seconds needs a value")?
                    .parse()
                    .context("--seconds")?;
            }
            _ => bail!("usage: buy_session [--pay-per-use] [--seconds N]"),
        }
    }
    Ok(args)
}

/// A free TCP port on loopback.
fn free_port() -> Result<SocketAddr> {
    Ok(TcpListener::bind("127.0.0.1:0")?.local_addr()?)
}

/// What the session needs to know about the gateway, and what the example
/// needs to watch it.
struct Gateway {
    pubkey: String,
    endpoint: String,
    mint_url: String,
    /// The gateway's enforcer: it counts what the device moves.
    enforcer: Arc<Loopback>,
    /// The gateway's control socket.
    control: PathBuf,
}

/// Start a gateway: a mint that gives its vouchers away, a node that sells
/// over a loopback enforcer, and its control socket.
async fn start_gateway(dir: &Path) -> Result<Gateway> {
    // The mint. In a deployment this is `mintd`, a daemon of its own.
    let (public, private) = (free_port()?, free_port()?);
    let mint_url = format!("http://{public}");
    let mint = mint::build(&MintConfig {
        url: mint_url.clone(),
        unit: "byte".into(),
        seed: vec![21; 32],
        file: dir.join("gateway-mint.sqlite"),
        max_amount: 1 << 34,
    })
    .await
    .context("build the gateway's mint")?;
    tokio::spawn(mint::serve(
        Arc::new(mint),
        mint::Listeners {
            public,
            private,
            auto_accept: true,
            issue_limit: IssueLimit::default(),
        },
        Arc::default(),
        std::future::pending(),
    ));

    // The node's configuration, written as the YAML an operator writes. A
    // pay-per-use payer is carried at up to 2 MB/s.
    let identity = Identity::generate();
    let file: File = serde_yaml::from_str(&format!(
        r#"
identity: {{ secret_key: "{secret}" }}
mint: {{ url: "{mint_url}" }}
burst: {{ unreserved_rate: 2000000 }}
"#,
        secret = identity.secret_hex(),
    ))
    .context("parse the gateway's configuration")?;
    let config = file
        .resolve()
        .context("resolve the gateway's configuration")?;

    // What the gateway funds its own channels with. It funds none here: the
    // session is a buyer only, and says so.
    let wallet = Wallet::open(dir.join("gateway-wallet.sqlite"), [22; 64], "byte")
        .await
        .context("open the gateway's wallet")?;
    let channels = Arc::new(SpilmanChannels::new(SpilmanConfig {
        mint_url: config.mint_url.clone(),
        mint_local: config.mint_local.clone(),
        unit: config.policy.unit.clone(),
        accepted_mints: config.policy.accepted_mints.clone(),
        burned_mints: Vec::new(),
        secret_key_hex: config.identity.secret_hex(),
        funding: Arc::new(WalletFunding::new(wallet)),
        ttl_seconds: config.channel_ttl_seconds,
    })?);

    let enforcer = Arc::new(Loopback::new());
    let node = Node::new(&config, channels, enforcer.clone());

    // The control socket: the same JSON snapshot `tolltop` reads.
    let control = dir.join("gateway-control.sock");
    let published = node.published();
    let path = control.clone();
    tokio::spawn(async move { control::serve(&path, published, std::future::pending()).await });

    // Bind first, so the session has a port to dial.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let endpoint = listener.local_addr()?.to_string();
    tokio::spawn(node.run_on(listener, config, std::future::pending()));

    Ok(Gateway {
        pubkey: hex::encode(identity.pubkey().0),
        endpoint,
        mint_url,
        enforcer,
        control,
    })
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    let args = parse_args()?;
    let dir = std::env::temp_dir().join(format!("tollgate-buy-session-{}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    let result = run(&args, &dir).await;
    let _ = std::fs::remove_dir_all(&dir);
    if let Err(e) = result {
        eprintln!("buy_session: {e:#}");
        std::process::exit(1);
    }
    // The gateway, its mint and the channel settlements the session's end
    // started all run on in the background. Leave without waiting for them:
    // shutting the runtime down under them only makes them complain.
    std::process::exit(0);
}

async fn run(args: &Args, dir: &Path) -> Result<()> {
    let gateway = start_gateway(dir).await?;

    // How the session pays. Both are a watch: send on `buying_tx` to move a
    // running session between them.
    let buying = if args.pay_per_use {
        Buying::PayPerUse {
            budget: PAY_PER_USE_BUDGET,
            renew_lead_ms: 2_000,
        }
    } else {
        Buying::AtSpeed
    };
    let (_buying_tx, buying_rx) = watch::channel(buying);
    // The speed: reserved for time at a speed, the most the device may move
    // for pay per use. Send on `rate_tx` to change it; zero stops buying.
    let (_rate_tx, rate_rx) = watch::channel(RATE);
    // What the gateway carried for the device. The session carries none of
    // the device's traffic, so for pay per use the caller reports it.
    let (moved_tx, moved_rx) = watch::channel(Moved::default());

    // The session's own key: one per device, kept for its life.
    let session = Identity::generate();
    // Where its vouchers come from. Here a wallet that mints at the
    // gateway's mint; a real program hands over what the device paid.
    let funding = Arc::new(WalletFunding::new(
        Wallet::open(dir.join("session-wallet.sqlite"), [23; 64], "byte").await?,
    ));

    let spec = ClientSpec {
        secret_hex: session.secret_hex(),
        gateway: gateway.pubkey.clone(),
        endpoint: gateway.endpoint.clone(),
        gateway_mint: gateway.mint_url.clone(),
        channel_capacity: 1 << 28,
        buying: buying_rx,
        moved: Some(moved_rx),
    };
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let task = tokio::spawn(client::run(spec, funding, None, rate_rx, async {
        let _ = stop_rx.await;
    }));

    println!(
        "buying {} from gateway {}…",
        if args.pay_per_use {
            "pay per use"
        } else {
            "time at a speed"
        },
        &gateway.pubkey[..16]
    );
    println!(
        "{:>4}  {:>12}  {:>12}  {:>14}  {:>10}",
        "s", "carried B/s", "budget B", "reserved B/s", "deadline"
    );

    let me = hex::encode(session.pubkey().0);
    let mut moved = Moved::default();
    for second in 1..=args.seconds {
        tokio::time::sleep(Duration::from_secs(1)).await;

        // Pretend the device downloads through the gateway, and tell the
        // session what the gateway carried.
        if args.pay_per_use {
            gateway
                .enforcer
                .record_delivered(session.pubkey(), DEVICE_MOVES);
            moved.to_device += DEVICE_MOVES;
            moved_tx.send_replace(moved);
        }

        // What the gateway holds for the session, off its control socket.
        let snapshot = control::fetch(&gateway.control).await?;
        match snapshot.peers.iter().find(|p| p.pubkey == me) {
            Some(peer) => println!(
                "{second:>4}  {:>12}  {:>12}  {:>14}  {:>9}s",
                peer.shaped_rate,
                peer.budget,
                peer.reserved_rate,
                peer.budget_expires_in_ms / 1000
            ),
            None => println!("{second:>4}  (not connected yet)"),
        }
    }

    let _ = stop_tx.send(());
    task.await??;
    Ok(())
}
