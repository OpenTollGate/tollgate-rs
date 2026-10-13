//! Read a running `tollgated`'s control socket and print, for each peer, what
//! this node sells it and what it buys from it.
//!
//! This is what `tolltop` does, without the screen: a program that watches a
//! node needs only [`control::find_socket`] and [`control::fetch`].
//!
//! ```text
//! cargo run -p tollgate-net --example read_snapshot                     # the one running instance
//! cargo run -p tollgate-net --example read_snapshot -- --instance ip    # a named instance
//! cargo run -p tollgate-net --example read_snapshot -- --socket PATH    # a socket somewhere else
//! ```

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use tollgate_net::control;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let mut instance = None;
    let mut socket = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--instance" => instance = Some(args.next().context("--instance needs a name")?),
            "--socket" => {
                socket = Some(PathBuf::from(args.next().context("--socket needs a path")?))
            }
            _ => bail!("usage: read_snapshot [--instance NAME | --socket PATH]"),
        }
    }

    // Without a path, look where every instance keeps its socket:
    // `tollgate-<instance>/control.sock` in the runtime directory.
    let path = match socket {
        Some(path) => path,
        None => control::find_socket(instance.as_deref())?,
    };
    let snapshot = control::fetch(&path)
        .await
        .with_context(|| format!("read {}", path.display()))?;

    println!(
        "node {}  unit {}  mint {}  up {}s  ({})",
        &snapshot.pubkey[..16.min(snapshot.pubkey.len())],
        snapshot.unit,
        snapshot.mint_url,
        snapshot.uptime_ms / 1000,
        path.display()
    );
    if snapshot.peers.is_empty() {
        println!("no peers");
    }
    for peer in &snapshot.peers {
        println!(
            "peer {}  {}  access {}",
            &peer.pubkey[..16.min(peer.pubkey.len())],
            peer.phase,
            peer.access
        );
        // What this node sells the peer: the peer's budget here, its
        // deadline, the rate it reserved, and the rate it is carried at.
        println!(
            "  sold:   budget {} {}, {}s left, reserved {}/s, carried at {}/s",
            peer.budget,
            snapshot.unit,
            peer.budget_expires_in_ms / 1000,
            peer.reserved_rate,
            peer.shaped_rate
        );
        // What this node buys from the peer, by its own count, and the last
        // Balance the peer sent, which is information only.
        println!(
            "  bought: budget {} {}, {}s left, reserved {}/s",
            peer.bought_budget,
            snapshot.unit,
            peer.bought_expires_in_ms / 1000,
            peer.bought_reserved_rate
        );
        if let Some(balance) = &peer.reported_balance {
            println!(
                "  balance from the peer: {} {}, {}s left, reserved {}/s ({}s ago)",
                balance.remaining,
                snapshot.unit,
                balance.expires_in_ms / 1000,
                balance.reserved_rate,
                balance.age_ms / 1000
            );
        }
        println!(
            "  moved:  {} to the peer, {} from it",
            peer.to_payer, peer.from_payer
        );
    }
    Ok(())
}
