//! The raw-TCP control plane: TollGate messages, length-prefixed.
//!
//! Peerings are adjacent by construction — hop-by-hop payment means the
//! counterparty is one link away — so there are no proxies or middleboxes in
//! between to justify dressing the protocol up as HTTP. Read two bytes, read
//! the body, hand it to the CBOR decoder.
//!
//! The transport carries no security burden. On FIPS a Noise IK handshake has
//! already authenticated and encrypted the link; on plain IP the operator wraps
//! the connection as it sees fit.

use anyhow::{Context, Result, bail};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tollgate_protocol::{FrameReader, MAX_FRAME_LEN, Message, PubKey, encode_frame};
use tracing::{debug, warn};

/// What the transport tells the node about.
#[derive(Debug)]
pub enum Wire {
    /// A link came up. The sender writes messages back to that peer.
    PeerUp {
        /// Who is on the other end.
        peer: PubKey,
        /// Outbound queue for this link.
        tx: mpsc::Sender<Message>,
    },
    /// A message arrived.
    Message {
        /// Who sent it.
        peer: PubKey,
        /// What they sent.
        msg: Message,
    },
    /// The link went away, cleanly or otherwise.
    PeerDown {
        /// Who was on the other end.
        peer: PubKey,
    },
}

/// Accept control-plane connections forever.
///
/// A listener does not know who is calling until they say so, so it reads the
/// first frame — which the protocol requires to be an Announce — and takes the
/// peer's identity from it.
pub async fn listen(listener: TcpListener, node: mpsc::Sender<Wire>) -> Result<()> {
    loop {
        let (stream, addr) = listener.accept().await.context("accept")?;
        let node = node.clone();
        tokio::spawn(async move {
            if let Err(e) = accept_one(stream, node).await {
                debug!(%addr, error = %e, "control connection ended");
            }
        });
    }
}

async fn accept_one(mut stream: TcpStream, node: mpsc::Sender<Wire>) -> Result<()> {
    let (peer, pending, reader) = read_announce(&mut stream).await?;
    run(stream, peer, pending, reader, node).await
}

/// Read until the Announce turns up, and take the peer's identity from it.
///
/// Returns everything else that had already arrived, plus the reader itself.
/// Both matter: a peer sends Announce and Offer back to back, so they usually
/// land in one segment — and anything decoded here, or still sitting in the
/// reader's buffer, would be silently lost if identification started a fresh
/// reader of its own.
async fn read_announce(stream: &mut TcpStream) -> Result<(PubKey, Vec<Message>, FrameReader)> {
    let mut reader = FrameReader::new();
    let mut buf = [0u8; 4096];
    let mut peer = None;
    let mut pending = Vec::new();

    loop {
        let n = stream.read(&mut buf).await?;
        if n == 0 {
            bail!("peer closed before announcing itself");
        }
        reader.push(&buf[..n]);

        while let Some(msg) = reader.next_message() {
            let msg = msg.context("a message arriving before identification did not decode")?;
            match (&msg, peer) {
                // The Announce is still a message core has to see — it carries
                // the version and unit that decide whether to keep talking.
                (Message::Announce(a), None) => peer = Some(a.pubkey),
                (other, None) => bail!(
                    "expected Announce as the first message, got {:?}",
                    other.msg_type()
                ),
                _ => {}
            }
            // Anything behind the Announce is ordinary traffic; it just arrived
            // before we knew whose it was.
            pending.push(msg);
        }

        if let Some(peer) = peer {
            return Ok((peer, pending, reader));
        }
    }
}

/// Open a control-plane connection to a peer whose identity we already know
/// from configuration.
pub async fn dial(addr: &str, peer: PubKey, node: mpsc::Sender<Wire>) -> Result<()> {
    let stream = TcpStream::connect(addr)
        .await
        .with_context(|| format!("dial {addr}"))?;
    run(stream, peer, Vec::new(), FrameReader::new(), node).await
}

/// Pump one established connection until either side stops.
async fn run(
    stream: TcpStream,
    peer: PubKey,
    pending: Vec<Message>,
    reader: FrameReader,
    node: mpsc::Sender<Wire>,
) -> Result<()> {
    stream.set_nodelay(true).ok();
    let (mut rx_half, mut tx_half) = stream.into_split();

    // Depth enough to absorb a burst of setup messages without making core
    // wait, but bounded: a peer that will not read should not be able to make
    // us buffer without limit.
    let (tx, mut outbox) = mpsc::channel::<Message>(64);
    node.send(Wire::PeerUp { peer, tx }).await.ok();

    // Anything read before the peer was identified still has to reach core,
    // in the order it arrived.
    for msg in pending {
        node.send(Wire::Message { peer, msg }).await.ok();
    }

    let writer = tokio::spawn(async move {
        let mut buf = Vec::with_capacity(512);
        while let Some(msg) = outbox.recv().await {
            buf.clear();
            if let Err(e) = encode_frame(&msg, &mut buf) {
                warn!(error = %e, "dropping a message that would not encode");
                continue;
            }
            if tx_half.write_all(&buf).await.is_err() {
                break;
            }
        }
        let _ = tx_half.shutdown().await;
    });

    let result = read_loop(&mut rx_half, reader, peer, &node).await;

    node.send(Wire::PeerDown { peer }).await.ok();
    writer.abort();
    result
}

/// Read frames until the peer stops.
///
/// Takes the reader by value rather than making its own: on an accepted
/// connection it already holds whatever arrived behind the Announce.
async fn read_loop(
    rx: &mut tokio::net::tcp::OwnedReadHalf,
    mut reader: FrameReader,
    peer: PubKey,
    node: &mpsc::Sender<Wire>,
) -> Result<()> {
    let mut buf = vec![0u8; 8 * 1024];

    loop {
        let n = rx.read(&mut buf).await?;
        if n == 0 {
            // A bare FIN is an unclean disconnect: an orderly teardown sends
            // Disconnect first. Either way the cleanup is the same.
            return Ok(());
        }
        reader.push(&buf[..n]);

        while let Some(msg) = reader.next_message() {
            match msg {
                Ok(msg) => {
                    if node.send(Wire::Message { peer, msg }).await.is_err() {
                        return Ok(());
                    }
                }
                // One malformed frame does not poison the ones behind it: the
                // length prefix was intact, so the stream is still in sync.
                Err(e) => warn!(%peer, error = %e, "discarding a malformed message"),
            }
        }

        if reader.pending() > MAX_FRAME_LEN * 2 {
            bail!("peer is buffering more than two maximum frames without completing one");
        }
    }
}
