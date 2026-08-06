//! What the node will tell a local tool about itself, and the one thing it
//! will take instructions about.
//!
//! A Unix socket serving a JSON snapshot, one per connection. The node
//! republishes the snapshot on every tick and the socket hands out whatever is
//! current, so a reader never blocks the event loop and the loop never waits on
//! a reader.
//!
//! A caller that says nothing gets the snapshot, which is what every reader
//! did before there was anything to say. A caller that sends a line of JSON
//! first gets an answer to it instead — see [`Request`]. The only thing that
//! can be changed this way is the market price: it is the one number an
//! operator has a reason to move while the node runs, and moving it does not
//! touch a session, a grant or a channel.
//!
//! Local-only by construction: a Unix socket has no port to expose, and
//! nothing here is reachable by a peer.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use arc_swap::ArcSwap;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tollgate_core::access::AccessLevel;
use tollgate_core::session::{Phase, Sessions};
use tracing::debug;

use crate::adapter::ResourceAdapter;
use crate::market::Price;

/// Where a control socket might live, best first.
///
/// Three places, for three ways of running a node. `/run` is where a service
/// manager puts it — systemd's `RuntimeDirectory=`, and the path the container
/// images use — and it is the only one of the three that is not writable by an
/// ordinary user, which is what makes it a good signal rather than a guess.
/// `XDG_RUNTIME_DIR` is where a node run by a person on their own machine
/// belongs. `/tmp` is the fallback that exists everywhere.
///
/// The daemon creates the first of these it can, and a reader looks for the
/// first that is already there. That asymmetry is the point: a tool should find
/// a running node without being told where it put its socket.
fn socket_candidates() -> Vec<PathBuf> {
    let mut paths = vec![PathBuf::from("/run/tollgate.sock")];
    if let Ok(xdg) = std::env::var("XDG_RUNTIME_DIR")
        && !xdg.is_empty()
    {
        paths.push(PathBuf::from(format!("{xdg}/tollgate.sock")));
    }
    paths.push(PathBuf::from("/tmp/tollgated.sock"));
    paths
}

/// Where this node should put its control socket unless told otherwise.
///
/// The first candidate whose directory we can actually write to. A daemon that
/// picked a path it could not create would fail at startup over something an
/// operator never asked for.
pub fn default_socket_path() -> PathBuf {
    for path in socket_candidates() {
        let Some(dir) = path.parent() else {
            continue;
        };
        // Writability is asked of the directory rather than assumed from the
        // user id: a container runs as root and a laptop does not, and both are
        // ordinary ways to run this.
        if dir
            .metadata()
            .map(|m| !m.permissions().readonly())
            .unwrap_or(false)
            && std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(false)
                .open(dir.join(".tollgate-write-test"))
                .map(|_| {
                    let _ = std::fs::remove_file(dir.join(".tollgate-write-test"));
                })
                .is_ok()
        {
            return path;
        }
    }
    PathBuf::from("/tmp/tollgated.sock")
}

/// Where a running node's control socket already is.
///
/// Returns the first candidate that exists, so `tolltop` with no arguments
/// finds a node started with no arguments — and finds one inside a container,
/// where the socket is in `/run` because that is where a service manager puts
/// it.
pub fn find_socket() -> Result<PathBuf> {
    let candidates = socket_candidates();
    for path in &candidates {
        // Existence, not connectability: a socket that is there but not
        // answering is a node that is starting or has just died, and saying so
        // is more useful than moving on to a stale path somewhere else.
        if path.exists() {
            return Ok(path.clone());
        }
    }

    let looked: Vec<String> = candidates.iter().map(|p| p.display().to_string()).collect();
    anyhow::bail!(
        "no control socket found; looked in {}. Is a node running, and did it \
         put its socket somewhere else? Pass --socket if so.",
        looked.join(", ")
    )
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
    /// What this node sells its own capacity for, in bytes per sat.
    ///
    /// Filled in where the snapshot is served rather than where it is built:
    /// the price belongs to the market, and the node's own state machine has
    /// no opinion about money. A reader wants both in one answer all the same.
    #[serde(default)]
    pub bytes_per_sat: u64,
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
        // Filled in when the snapshot is served: what the node charges is the
        // market's business, not the session layer's.
        bytes_per_sat: 0,
        peers,
    }
}

/// First four bytes in hex — enough to correlate, short enough to read.
fn short(bytes: &[u8]) -> String {
    hex::encode(&bytes[..4.min(bytes.len())])
}

/// How long to wait for a request before assuming there is not going to be one.
///
/// Short: a caller with something to say has already written it by the time the
/// connection is accepted. This is only the ceiling on how long a caller that
/// wants the snapshot and says so by staying quiet has to wait for it.
const REQUEST_WAIT: std::time::Duration = std::time::Duration::from_millis(150);

/// A line of JSON asking the node for something.
///
/// Shaped like the FIPS control socket's — a command and its parameters — so
/// an operator driving both is not learning two conventions.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "command", content = "params", rename_all = "snake_case")]
pub enum Request {
    /// Everything the node is doing. The same thing a caller gets by saying
    /// nothing at all.
    Snapshot,
    /// What the node sells its capacity for.
    ShowPrice,
    /// Change what the node sells its capacity for.
    SetPrice {
        /// Units of capacity one sat buys. Zero closes the market.
        bytes_per_sat: u64,
    },
}

/// What came back.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Response {
    /// The command was carried out.
    Ok {
        /// Whatever it produced.
        data: serde_json::Value,
    },
    /// It was not.
    Error {
        /// Why.
        message: String,
    },
}

/// Serve the snapshot, and the few commands, until `shutdown`.
///
/// One exchange per connection, then close: a tool that wants live data
/// reconnects, which keeps this side stateless and means a stalled reader can
/// never hold anything up.
pub async fn serve(
    path: &Path,
    published: Published,
    price: Price,
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
                let (stream, _) = match accepted {
                    Ok(pair) => pair,
                    Err(e) => {
                        debug!(error = %e, "control connection failed to accept");
                        continue;
                    }
                };
                let published = Arc::clone(&published);
                let price = price.clone();
                tokio::spawn(async move {
                    let _ = answer(stream, published, price).await;
                });
            }
            _ = &mut shutdown => break,
        }
    }

    let _ = std::fs::remove_file(path);
    Ok(())
}

/// Read at most one request and write exactly one answer.
///
/// The request is optional: a caller that closes its writing half without
/// sending anything is asking for the snapshot, which is what every reader of
/// this socket did before it could be asked anything else.
async fn answer(stream: tokio::net::UnixStream, published: Published, price: Price) -> Result<()> {
    let (rx, mut tx) = stream.into_split();
    let mut line = String::new();

    // Bounded, because "says nothing" has two shapes. A caller that closes its
    // writing half ends the read immediately; one that simply connects and
    // waits to be told something — which is what `nc` does, and what every
    // reader of this socket did before it could be asked anything — would
    // otherwise leave both sides waiting on each other until a timeout kills
    // the connection and the reader gets nothing at all.
    let _ = tokio::time::timeout(REQUEST_WAIT, BufReader::new(rx).read_line(&mut line)).await;

    let response = match line.trim() {
        "" => Response::Ok {
            data: serde_json::to_value(current(&published, &price))?,
        },
        text => match serde_json::from_str::<Request>(text) {
            Ok(request) => run(request, &published, &price),
            Err(e) => Response::Error {
                message: format!("not a request this node understands: {e}"),
            },
        },
    };

    // A caller that said nothing gets the snapshot bare, exactly as before.
    // One that asked gets the answer wrapped, so that a failure is a failure
    // rather than a suspiciously empty snapshot.
    let mut body = match (&response, line.trim().is_empty()) {
        (Response::Ok { data }, true) => serde_json::to_vec(data)?,
        _ => serde_json::to_vec(&response)?,
    };
    body.push(b'\n');
    tx.write_all(&body).await?;
    tx.shutdown().await?;
    Ok(())
}

fn run(request: Request, published: &Published, price: &Price) -> Response {
    match request {
        Request::Snapshot => match serde_json::to_value(current(published, price)) {
            Ok(data) => Response::Ok { data },
            Err(e) => Response::Error {
                message: e.to_string(),
            },
        },
        Request::ShowPrice => Response::Ok {
            data: serde_json::json!({ "bytes_per_sat": price.bytes_per_sat() }),
        },
        Request::SetPrice { bytes_per_sat } => {
            price.set(bytes_per_sat);
            // Worth a line in the log: it changes what the node charges, and
            // the next operator to read the logs will want to know when.
            tracing::info!(bytes_per_sat, "the market price was changed");
            Response::Ok {
                data: serde_json::json!({ "bytes_per_sat": price.bytes_per_sat() }),
            }
        }
    }
}

/// The published snapshot, with the price the market is holding right now.
fn current(published: &Published, price: &Price) -> Snapshot {
    let mut snapshot = (**published.load()).clone();
    snapshot.bytes_per_sat = price.bytes_per_sat();
    snapshot
}

/// Read one snapshot from a node's control socket.
pub async fn fetch(path: &Path) -> Result<Snapshot> {
    let body = exchange(path, None).await?;
    serde_json::from_str(&body).context("parse the snapshot")
}

/// Send one request to a node's control socket and return what it answered.
pub async fn send(path: &Path, request: &Request) -> Result<Response> {
    let body = exchange(path, Some(serde_json::to_string(request)?)).await?;
    serde_json::from_str(&body).with_context(|| format!("parse the answer: {body}"))
}

/// One connection, one optional request, one answer.
///
/// The writing half is closed either way, which is what tells a node that says
/// nothing that nothing is coming: the server reads a line, and a half-closed
/// socket ends that read rather than leaving both sides waiting on each other.
async fn exchange(path: &Path, request: Option<String>) -> Result<String> {
    use tokio::io::AsyncReadExt;

    let stream = tokio::net::UnixStream::connect(path)
        .await
        .with_context(|| format!("connect to {}", path.display()))?;
    let (mut rx, mut tx) = stream.into_split();

    if let Some(request) = request {
        tx.write_all(request.as_bytes())
            .await
            .context("write the request")?;
        tx.write_all(b"\n").await.context("write the request")?;
    }
    tx.shutdown().await.context("finish writing")?;

    let mut body = String::new();
    rx.read_to_string(&mut body)
        .await
        .context("read the answer")?;
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serve a socket in a temporary directory, and hand back the path.
    async fn serving(price: Price) -> (PathBuf, tokio::task::JoinHandle<()>) {
        let path = std::env::temp_dir().join(format!(
            "tollgate-control-test-{}.sock",
            std::process::id() as u64 + price.bytes_per_sat()
        ));
        let published: Published = Arc::new(ArcSwap::from_pointee(Snapshot {
            pubkey: "02aa".into(),
            unit: "byte".into(),
            ..Snapshot::default()
        }));

        let serve_path = path.clone();
        let task = tokio::spawn(async move {
            let _ = serve(&serve_path, published, price, std::future::pending::<()>()).await;
        });

        // Bound rather than fixed: binding a Unix socket is fast, but a loaded
        // machine is a loaded machine.
        for _ in 0..100 {
            if path.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        (path, task)
    }

    #[test]
    fn a_service_managed_socket_is_looked_for_first() {
        // /run is where a service manager puts it and is not writable by an
        // ordinary user, which is what makes finding one there meaningful.
        let candidates = socket_candidates();
        assert_eq!(
            candidates.first().unwrap(),
            &PathBuf::from("/run/tollgate.sock")
        );
        assert_eq!(
            candidates.last().unwrap(),
            &PathBuf::from("/tmp/tollgated.sock"),
            "the fallback that exists everywhere goes last"
        );
    }

    #[test]
    fn a_socket_that_is_not_anywhere_says_where_it_looked() {
        // The old failure was "/tmp/tollgated.sock: No such file or directory",
        // which told an operator nothing about the two other places a node
        // might have put it.
        let Err(e) = find_socket() else {
            // A node really is running on this machine; nothing to assert.
            return;
        };
        let message = format!("{e}");
        assert!(message.contains("/run/tollgate.sock"), "{message}");
        assert!(message.contains("--socket"), "{message}");
    }

    #[tokio::test]
    async fn a_caller_that_says_nothing_still_gets_the_snapshot() {
        // Every reader of this socket did exactly this before it could be asked
        // anything, and they must not have to change.
        let (path, task) = serving(Price::new(1_000_000)).await;

        let snapshot = fetch(&path).await.expect("fetch");
        assert_eq!(snapshot.pubkey, "02aa");
        assert_eq!(
            snapshot.bytes_per_sat, 1_000_000,
            "the price is served alongside what the node is doing"
        );

        task.abort();
    }

    #[tokio::test]
    async fn the_price_can_be_changed_through_the_socket() {
        let price = Price::new(2_000_000);
        let (path, task) = serving(price.clone()).await;

        let answer = send(
            &path,
            &Request::SetPrice {
                bytes_per_sat: 500_000,
            },
        )
        .await
        .expect("set the price");
        assert!(matches!(answer, Response::Ok { .. }));

        // The market holds the same number, not a copy of it: what the socket
        // changed is what the mint will quote against.
        assert_eq!(price.bytes_per_sat(), 500_000);
        assert_eq!(fetch(&path).await.expect("fetch").bytes_per_sat, 500_000);

        task.abort();
    }

    #[tokio::test]
    async fn a_request_that_makes_no_sense_is_refused_rather_than_guessed_at() {
        let (path, task) = serving(Price::new(3_000_000)).await;

        let body = exchange(&path, Some("{\"command\":\"drop_everything\"}".into()))
            .await
            .expect("exchange");
        let answer: Response = serde_json::from_str(&body).expect("an answer");
        assert!(
            matches!(answer, Response::Error { .. }),
            "an unknown command must not read as success: {body}"
        );

        task.abort();
    }
}
