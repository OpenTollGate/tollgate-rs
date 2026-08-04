//! What the node will tell a local tool about itself.
//!
//! A Unix socket serving a JSON snapshot, one per connection. The node
//! republishes the snapshot on every tick and the socket hands out whatever is
//! current, so a reader never blocks the event loop and the loop never waits on
//! a reader.
//!
//! Local-only by construction: a Unix socket has no port to expose, and the
//! snapshot carries operational state rather than anything a peer could act on.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use arc_swap::ArcSwap;
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;
use tokio::net::UnixListener;
use tollgate_core::access::AccessLevel;
use tollgate_core::session::{Phase, Sessions};
use tracing::debug;

use crate::adapter::ResourceAdapter;

/// Where the control socket lives unless the operator says otherwise.
pub fn default_socket_path() -> PathBuf {
    PathBuf::from("/tmp/tollgated.sock")
}

/// Everything one node is currently doing.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Snapshot {
    /// This node's public key, hex-encoded.
    pub pubkey: String,
    /// The unit it denominates in.
    pub unit: String,
    /// The mint it advertises.
    pub mint_url: String,
    /// How long it has been up, in milliseconds.
    pub uptime_ms: u64,
    /// One entry per peer, ordered by key so the display does not jump around.
    pub peers: Vec<PeerSnapshot>,
}

/// One peering, from this node's side.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerSnapshot {
    /// Their public key, hex-encoded.
    pub pubkey: String,
    /// Where they are in the opening sequence.
    pub phase: String,
    /// What we will deliver for them.
    pub access: String,

    // --- what they bought from us -----------------------------------------
    /// Units per second we are letting them draw. Their grant, or the minimum
    /// flow allowance if it has lapsed.
    pub shaped_rate: u64,
    /// Cumulative units they have paid for, across every channel.
    pub authorized: u64,
    /// Cumulative units drawn against them.
    pub consumed: u64,
    /// Milliseconds until the grant in force lapses. Zero if none is live.
    pub grant_expires_in_ms: u64,
    /// Channels they pay us on, and how full each is.
    pub incoming_channels: Vec<ChannelSnapshot>,

    // --- what we bought from them -----------------------------------------
    /// Units per second we last bought.
    pub bought_rate: u64,
    /// Units per second we want from them.
    pub demand: u64,
    /// Units per second we are pushing at them.
    pub upload_rate: u64,
    /// Their surcharge on what we push at them.
    pub received_multiplier: u16,
    /// The channel we are paying on, if one is open.
    pub outgoing_channel: Option<ChannelSnapshot>,
    /// Whether a replacement is funded and waiting behind it.
    pub rollover_ready: bool,

    // --- what actually moved ----------------------------------------------
    /// Cumulative units delivered to them.
    pub delivered: u64,
    /// Cumulative units received from them.
    pub received: u64,
}

/// One channel, and how much of it is spent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelSnapshot {
    /// Its identifier, hex-encoded and shortened.
    pub id: String,
    /// Units it can carry in total.
    pub capacity: u64,
    /// Units signed onto it so far.
    pub signed: u64,
}

/// The latest snapshot, republished by the node and read by the socket.
pub type Published = Arc<ArcSwap<Snapshot>>;

/// Build a snapshot of everything the node is doing right now.
pub fn snapshot(
    sessions: &Sessions,
    adapter: &dyn ResourceAdapter,
    pubkey: &str,
    mint_url: &str,
    uptime_ms: u64,
    now: tollgate_core::Millis,
) -> Snapshot {
    let policy = sessions.node_policy();

    let peers = sessions
        .peers()
        .map(|session| {
            let counters = adapter.counters(session.peer);
            let grant = &session.grant;

            PeerSnapshot {
                pubkey: hex::encode(session.peer.0),
                phase: match session.phase {
                    Phase::Opening => "opening",
                    Phase::Establishing => "establishing",
                    Phase::Established => "established",
                    Phase::Closing => "closing",
                }
                .into(),
                access: match session.access {
                    AccessLevel::None => "none",
                    AccessLevel::Active => "active",
                    AccessLevel::Free => "free",
                    AccessLevel::Suspended => "suspended",
                }
                .into(),

                shaped_rate: adapter.shaping_rate(session.peer),
                authorized: grant.authorized(),
                consumed: grant.consumed(),
                grant_expires_in_ms: if grant.is_live(now) {
                    grant.deadline().saturating_since(now)
                } else {
                    0
                },
                incoming_channels: grant
                    .channels()
                    .iter()
                    .map(|c| ChannelSnapshot {
                        id: short(&c.id.0),
                        capacity: c.capacity,
                        signed: c.signed,
                    })
                    .collect(),

                bought_rate: session.buyer.rate(),
                demand: session.demand,
                upload_rate: session.upload_rate,
                received_multiplier: session
                    .offer
                    .as_ref()
                    .map(|o| o.received_multiplier)
                    .unwrap_or(0),
                outgoing_channel: session.buyer.active().map(|c| ChannelSnapshot {
                    id: short(&c.id.0),
                    capacity: c.capacity,
                    signed: c.cumulative,
                }),
                rollover_ready: session.buyer.next_channel().is_some(),

                delivered: counters.delivered,
                received: counters.received,
            }
        })
        .collect();

    Snapshot {
        pubkey: pubkey.into(),
        unit: policy.unit.clone(),
        mint_url: mint_url.into(),
        uptime_ms,
        peers,
    }
}

/// First four bytes in hex — enough to correlate, short enough to read.
fn short(bytes: &[u8]) -> String {
    hex::encode(&bytes[..4.min(bytes.len())])
}

/// Serve the current snapshot to anything that connects, until `shutdown`.
///
/// One snapshot per connection, then close: a tool that wants live data
/// reconnects, which keeps this side stateless and means a stalled reader can
/// never hold anything up.
pub async fn serve(
    path: &Path,
    published: Published,
    shutdown: impl std::future::Future<Output = ()> + Send,
) -> Result<()> {
    // A socket left behind by a previous run would make bind fail. Removing it
    // is safe because binding is what proves nobody else is listening.
    let _ = std::fs::remove_file(path);
    let listener = UnixListener::bind(path)
        .with_context(|| format!("bind the control socket at {}", path.display()))?;

    let mut shutdown = std::pin::pin!(shutdown);
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (mut stream, _) = match accepted {
                    Ok(pair) => pair,
                    Err(e) => {
                        debug!(error = %e, "control connection failed to accept");
                        continue;
                    }
                };
                let current = published.load_full();
                tokio::spawn(async move {
                    if let Ok(mut body) = serde_json::to_vec(&*current) {
                        body.push(b'\n');
                        let _ = stream.write_all(&body).await;
                        let _ = stream.shutdown().await;
                    }
                });
            }
            _ = &mut shutdown => break,
        }
    }

    let _ = std::fs::remove_file(path);
    Ok(())
}

/// Read one snapshot from a node's control socket.
pub async fn fetch(path: &Path) -> Result<Snapshot> {
    use tokio::io::AsyncReadExt;

    let mut stream = tokio::net::UnixStream::connect(path)
        .await
        .with_context(|| format!("connect to {}", path.display()))?;
    let mut body = String::new();
    stream
        .read_to_string(&mut body)
        .await
        .context("read the snapshot")?;
    serde_json::from_str(&body).context("parse the snapshot")
}
