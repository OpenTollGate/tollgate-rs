//! Driving an external enforcer: a separate program that owns the traffic it
//! controls.
//!
//! The enforcer owns its traffic — a firewall, a proxy, a tunnel — and listens
//! on a Unix socket, by default `enforcer.sock` in the instance's runtime
//! directory. This connects to it, tells it which payer holds which subjects
//! and at what rate each payer is carried, and takes back what it carried. The
//! protocol is `docs/design/core/tollgate-enforcer-protocol.md`; the messages
//! are [`tollgate_protocol::enforcer`].
//!
//! # What the enforcer is told
//!
//! One [`Bind`] per payer, carrying every subject it holds, and one [`Set`]
//! carrying its rate: `0` closed, a number open and shaped, `null` open and
//! unshaped. That rate is the one core shapes the payer to, which is `0`
//! exactly when [`AccessLevel::carried`] says the payer is shut out — so
//! [`set_access`](Enforcer::set_access) has nothing to add, and sends
//! nothing.
//!
//! What a payer is bound to is what this node genuinely has about it, in the
//! one form its identity fixes: under [`PeerIdentity::Pubkey`] the key itself,
//! 32 bytes, which the mesh proved; under [`PeerIdentity::Address`] the address
//! the control connection came from, 16 bytes, which nothing did. Anything else
//! the enforcer recognizes it derives for itself.
//!
//! # Failing closed
//!
//! An enforcer starts closed and closes again whenever the connection drops,
//! so every connection begins with the full state: a `bind` and a `set` for
//! every payer. While there is no connection — or before a `hello` this node
//! accepts — [`selling`](Enforcer::selling) is false, and the node sells
//! nothing. The counters are rebased across connections, so the totals core
//! sees never go backwards.
//!
//! # The hello is a check
//!
//! The enforcer's `hello` states the identity it was built for and the unit it
//! counts in. Neither is negotiated: both are this instance's configuration,
//! `enforcer.identity` and `mint.unit`. [`External::connect`] waits for a
//! `hello` and refuses to start behind one that differs in either, naming
//! both; an enforcer that reconnects saying something different is refused
//! the same way, and the node goes on selling nothing.

use std::collections::HashMap;
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Result, bail};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::sync::{mpsc, oneshot};
use tollgate_core::access::AccessLevel;
use tollgate_core::meter::Counters;
use tollgate_protocol::enforcer::{
    self, Bind, Binding, EnforcerMessage, Hello, MAX_BINDINGS, PROTOCOL_VERSION, Remove, Set,
    Subject,
};
use tollgate_protocol::{FrameReader, MAX_FRAME_LEN, PubKey};
use tracing::{debug, error, info, warn};

use super::Enforcer;
use crate::wire::PeerIdentity;

/// How long to wait between attempts to reach the enforcer.
const RECONNECT: Duration = Duration::from_secs(1);

/// How long an enforcer that has accepted the connection has to say `hello`.
///
/// Nothing is sold until it does, so an enforcer that never speaks costs
/// nothing but time; this only decides how soon the connection is tried again.
const HELLO_TIMEOUT: Duration = Duration::from_secs(10);

/// Messages that may wait for the enforcer to read them.
///
/// An enforcer that stops reading is one this node can no longer steer. Past
/// this the connection is dropped, which closes the enforcer — the safe
/// failure — and the next connection starts from the full state.
const OUTBOX: usize = 4_096;

/// How long one write to the enforcer may take before it is taken to have
/// stopped reading, and the connection is closed.
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);

/// What this instance is, which every `hello` is checked against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Expected {
    /// `enforcer.identity`.
    pub identity: PeerIdentity,
    /// `mint.unit`.
    pub unit: String,
}

/// Why a `hello` was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// An enforcer protocol version this node does not speak. The connection
    /// is closed and tried again.
    Version(u8),
    /// The enforcer was built for another identity than `enforcer.identity`.
    Identity {
        /// What the config says.
        config: PeerIdentity,
        /// What the enforcer said.
        hello: PeerIdentity,
    },
    /// The enforcer counts in another unit than this instance's `mint.unit`.
    Unit {
        /// What the config says.
        config: String,
        /// What the enforcer said.
        hello: String,
    },
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Version(v) => write!(
                f,
                "the enforcer speaks enforcer protocol version {v}, and this node speaks \
                 {PROTOCOL_VERSION}"
            ),
            Self::Identity { config, hello } => write!(
                f,
                "enforcer.identity is {config}, and the enforcer says {hello}; the two \
                 ends of a pairing must agree on what a subject is"
            ),
            Self::Unit { config, hello } => write!(
                f,
                "mint.unit is {config:?}, and the enforcer counts in {hello:?}; every \
                 rate and count between them would be read in the wrong unit"
            ),
        }
    }
}

/// Decide whether this node can run behind an enforcer that said `hello`.
///
/// Nothing is negotiated: the identity and the unit are this instance's, and
/// the `hello` either agrees with them or is refused. A reconnecting enforcer
/// is checked against the same, so it cannot change either.
pub fn check_hello(hello: &Hello, expected: &Expected) -> Result<(), Refusal> {
    if hello.version != PROTOCOL_VERSION {
        return Err(Refusal::Version(hello.version));
    }
    if hello.identity != expected.identity {
        return Err(Refusal::Identity {
            config: expected.identity,
            hello: hello.identity,
        });
    }
    // Compared as plain text: `byte` and `bytes` are different units.
    if hello.unit != expected.unit {
        return Err(Refusal::Unit {
            config: expected.unit.clone(),
            hello: hello.unit.clone(),
        });
    }
    Ok(())
}

/// Why a delegated binding was turned down.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DelegateError {
    /// No enforcer has said `hello` yet, so nothing is known of what it
    /// accepts.
    #[error("no enforcer has said hello yet")]
    NoEnforcer,
    /// The enforcer takes no third party's word.
    #[error("the enforcer refuses delegated bindings")]
    Refused,
    /// The payer is not one this node tracks.
    #[error("no such payer")]
    UnknownPayer,
    /// The payer already holds as many subjects as one `bind` carries.
    #[error("the payer already holds {MAX_BINDINGS} subjects")]
    TooMany,
}

/// One payer, as this enforcer knows it.
#[derive(Debug, Clone)]
struct Peer {
    /// When it arrived, relative to the others.
    seq: u64,
    /// Where its control connection came from.
    addr: IpAddr,
    /// Subjects a local trusted client added for it.
    delegated: Vec<Subject>,
    rate: u64,
    demand: u64,
    /// What earlier connections carried for it.
    base: Counters,
    /// What the current connection has reported, cumulative on it.
    current: Counters,
    /// The enforcer refused one of its subjects: it is sold nothing.
    conflicted: bool,
}

impl Peer {
    fn total(&self) -> Counters {
        Counters {
            delivered: self.base.delivered.saturating_add(self.current.delivered),
            received: self.base.received.saturating_add(self.current.received),
        }
    }
}

/// A live connection to the enforcer.
#[derive(Debug)]
struct Link {
    /// Which connection this is, so a connection that has ended cannot touch
    /// the state of the one after it.
    id: u64,
    out: mpsc::Sender<EnforcerMessage>,
    /// Closes the socket at once when this node drops the link, rather than
    /// once whatever is queued has drained: the enforcer only closes when it
    /// sees the connection go.
    close: Arc<tokio::sync::Notify>,
}

#[derive(Debug)]
struct State {
    /// Who a peer is, which fixes the form of what it is bound to.
    identity: PeerIdentity,
    peers: HashMap<PubKey, Peer>,
    link: Option<Link>,
    /// The last `hello` accepted. Kept across connections, for what it says
    /// about delegated bindings.
    hello: Option<Hello>,
    next_id: u64,
    next_seq: u64,
}

impl State {
    fn new(identity: PeerIdentity) -> Self {
        Self {
            identity,
            peers: HashMap::new(),
            link: None,
            hello: None,
            next_id: 0,
            next_seq: 0,
        }
    }

    /// Everything `peer` holds that the current enforcer may be sent.
    fn bindings(&self, peer: PubKey) -> Vec<Binding> {
        let (Some(hello), Some(entry)) = (self.hello.as_ref(), self.peers.get(&peer)) else {
            return Vec::new();
        };
        // What this node itself has, in the one form the identity fixes: the
        // key the mesh proved, or the address nothing did.
        let direct = match self.identity {
            PeerIdentity::Pubkey => Subject::pubkey(&peer),
            PeerIdentity::Address => Subject::address(entry.addr),
        };
        std::iter::once(Binding {
            subject: direct,
            delegated: false,
        })
        .chain(entry.delegated.iter().map(|s| Binding {
            subject: s.clone(),
            delegated: true,
        }))
        // Sending what the enforcer refuses is a protocol error, so it is
        // left out rather than sent: delegated subjects, should a reconnected
        // enforcer no longer take them.
        .filter(|b| hello.accepts(b))
        .take(MAX_BINDINGS)
        .collect()
    }

    /// Queue a message for the enforcer, if there is one.
    ///
    /// An enforcer that has stopped reading is dropped: the connection closes,
    /// the enforcer closes with it, and the next one starts from the full
    /// state.
    fn send(&mut self, msg: EnforcerMessage) {
        let Some(link) = &self.link else {
            return;
        };
        if let Err(e) = link.out.try_send(msg) {
            warn!(error = %e, "the enforcer is not reading; dropping the connection");
            self.drop_link();
        }
    }

    fn bind(&mut self, peer: PubKey) {
        let bindings = self.bindings(peer);
        self.send(EnforcerMessage::Bind(Bind { peer, bindings }));
    }

    fn set(&mut self, peer: PubKey) {
        let Some(entry) = self.peers.get(&peer) else {
            return;
        };
        let rate = wire_rate(entry.rate);
        self.send(EnforcerMessage::Set(Set { peer, rate }));
    }

    /// Stop selling, and carry what the connection counted into the base the
    /// next one counts from.
    fn drop_link(&mut self) {
        let Some(link) = self.link.take() else {
            return;
        };
        link.close.notify_one();
        for peer in self.peers.values_mut() {
            peer.base = peer.total();
            peer.current = Counters::default();
        }
    }

    /// [`Self::drop_link`], if `id` is still the live connection.
    fn drop_link_if(&mut self, id: u64) {
        if self.link.as_ref().is_some_and(|l| l.id == id) {
            self.drop_link();
        }
    }

    /// A subject as an operator reads it: an address under `address`, hex
    /// otherwise.
    fn describe(&self, subject: &Subject) -> String {
        match (self.identity, <[u8; 16]>::try_from(subject.as_bytes())) {
            (PeerIdentity::Address, Ok(octets)) => {
                std::net::Ipv6Addr::from(octets).to_canonical().to_string()
            }
            _ => subject.to_string(),
        }
    }
}

/// A rate as the enforcer wants it: a number, or `null` for unshaped.
///
/// Core says `u64::MAX` for a peer it does not meter, and the protocol has a
/// word for that which is not a very large number.
fn wire_rate(rate: u64) -> Option<u64> {
    (rate != u64::MAX).then_some(rate)
}

/// Drives an external enforcer over its Unix socket.
#[derive(Debug, Clone)]
pub struct External {
    state: Arc<Mutex<State>>,
}

impl External {
    /// Reach the enforcer at `socket`, and wait for a `hello` that agrees with
    /// `expected`: this instance's identity and unit.
    ///
    /// Waits as long as the enforcer is unreachable or speaks a version this
    /// node does not, retrying about once a second; fails if its `hello`
    /// states another identity or unit, naming both.
    ///
    /// The connection is kept from then on: dropped, it is made again, and the
    /// enforcer is sent the full state.
    pub async fn connect(socket: impl Into<PathBuf>, expected: Expected) -> Result<Self> {
        let socket = socket.into();
        let enforcer = Self {
            state: Arc::new(Mutex::new(State::new(expected.identity))),
        };
        let (first_tx, first_rx) = oneshot::channel();
        tokio::spawn(maintain(
            socket.clone(),
            Arc::clone(&enforcer.state),
            expected,
            first_tx,
        ));
        match first_rx.await {
            Ok(Ok(())) => Ok(enforcer),
            Ok(Err(refusal)) => bail!(
                "refusing to start behind the enforcer at {}: {refusal}",
                socket.display()
            ),
            Err(_) => bail!("the enforcer connection stopped before a hello"),
        }
    }

    /// Add a subject a local trusted client vouches for to `payer`'s
    /// bindings.
    ///
    /// Turned down here, rather than sent, when the enforcer refuses delegated
    /// bindings: sending one would be a protocol error. The payer must be one
    /// this node tracks. On success the payer's next `bind` carries the
    /// subject, flagged delegated, its bytes untouched.
    pub fn delegate(&self, payer: PubKey, subject: Subject) -> Result<(), DelegateError> {
        let mut state = self.state.lock().expect("not poisoned");
        let hello = state.hello.as_ref().ok_or(DelegateError::NoEnforcer)?;
        if !hello.delegated {
            return Err(DelegateError::Refused);
        }
        let entry = state
            .peers
            .get_mut(&payer)
            .ok_or(DelegateError::UnknownPayer)?;
        if entry.delegated.contains(&subject) {
            return Ok(());
        }
        // One of the eight is the payer's own: seven delegated at most.
        if entry.delegated.len() + 1 >= MAX_BINDINGS {
            return Err(DelegateError::TooMany);
        }
        entry.delegated.push(subject);
        state.bind(payer);
        Ok(())
    }

    /// Whether an enforcer this node accepted is connected now.
    pub fn connected(&self) -> bool {
        self.state.lock().expect("not poisoned").link.is_some()
    }
}

/// Keep a connection to the enforcer for as long as the node runs.
///
/// `first` hears the outcome of the first `hello` — accepted, or refused for a
/// mismatch — and the task ends if it is refused, since the node is then not
/// going to start.
async fn maintain(
    socket: PathBuf,
    state: Arc<Mutex<State>>,
    expected: Expected,
    first: oneshot::Sender<Result<(), Refusal>>,
) {
    let mut first = Some(first);
    let mut reported = false;
    let mut refused = false;
    loop {
        match UnixStream::connect(&socket).await {
            Ok(stream) => {
                reported = false;
                match session(stream, &state, &expected, &mut first).await {
                    Ok(()) => refused = false,
                    Err(Ended::Refused(refusal)) => {
                        if let Some(first) = first.take() {
                            let _ = first.send(Err(refusal));
                            return;
                        }
                        // Once, not every second it is tried again.
                        if !refused {
                            error!(
                                socket = %socket.display(),
                                "refusing the enforcer: {refusal}; selling nothing until it is fixed"
                            );
                            refused = true;
                        }
                    }
                    Err(Ended::Error(e)) => {
                        refused = false;
                        warn!(
                            socket = %socket.display(),
                            error = format!("{e:#}"),
                            "the enforcer connection closed; selling nothing until it is back"
                        );
                    }
                }
            }
            Err(e) => {
                // Once per outage: this retries every second.
                if !reported {
                    warn!(
                        socket = %socket.display(),
                        error = %e,
                        "the enforcer is unreachable; selling nothing until it answers"
                    );
                    reported = true;
                }
            }
        }
        if first.as_ref().is_some_and(|f| f.is_closed()) {
            return;
        }
        tokio::time::sleep(RECONNECT).await;
    }
}

/// How a connection to the enforcer ended.
enum Ended {
    /// The enforcer's `hello` was refused.
    Refused(Refusal),
    /// Anything else: the enforcer went away, or broke the protocol.
    Error(anyhow::Error),
}

impl From<anyhow::Error> for Ended {
    fn from(e: anyhow::Error) -> Self {
        Self::Error(e)
    }
}

/// One connection, from `hello` to whichever end closes it.
async fn session(
    stream: UnixStream,
    state: &Arc<Mutex<State>>,
    expected: &Expected,
    first: &mut Option<oneshot::Sender<Result<(), Refusal>>>,
) -> Result<(), Ended> {
    let (mut rx, mut tx) = stream.into_split();
    let mut reader = FrameReader::new();

    let hello = tokio::time::timeout(HELLO_TIMEOUT, read_hello(&mut rx, &mut reader))
        .await
        .map_err(|_| anyhow::anyhow!("the enforcer said nothing for {HELLO_TIMEOUT:?}"))??;

    match check_hello(&hello, expected) {
        Ok(()) => {}
        // Not a reason to refuse to start: the connection is closed and tried
        // again, in case the enforcer is being upgraded under us.
        Err(refusal @ Refusal::Version(_)) => {
            return Err(Ended::Error(anyhow::anyhow!("{refusal}")));
        }
        Err(refusal) => return Err(Ended::Refused(refusal)),
    }

    // Install the link and queue the full state under one lock, so a change
    // made meanwhile lands either in the snapshot or after it — never
    // between.
    let close = Arc::new(tokio::sync::Notify::new());
    let mut outbox;
    let id = {
        let mut state = state.lock().expect("not poisoned");
        // Room for the full state on top of the ordinary margin, so the
        // snapshot can never fill the queue by itself.
        let (out, rx) = mpsc::channel(OUTBOX + 2 * state.peers.len());
        outbox = rx;
        state.hello = Some(hello.clone());
        state.next_id += 1;
        let id = state.next_id;
        state.link = Some(Link {
            id,
            out,
            close: Arc::clone(&close),
        });
        // A conflict belongs to the connection that reported it: the enforcer
        // that refused a binding has forgotten it, and reports it again on
        // this one. In the order the payers arrived, so the payer that held a
        // subject first is bound first again, and the same one is refused.
        let mut peers: Vec<(u64, PubKey)> = state
            .peers
            .iter_mut()
            .map(|(key, entry)| {
                entry.conflicted = false;
                (entry.seq, *key)
            })
            .collect();
        peers.sort_unstable();
        for (_, peer) in peers {
            state.bind(peer);
            state.set(peer);
        }
        id
    };
    info!(
        identity = %hello.identity,
        unit = %hello.unit,
        delegated = hello.delegated,
        "the enforcer said hello; selling"
    );
    if let Some(first) = first.take() {
        let _ = first.send(Ok(()));
    }

    let writer = async {
        let mut buf = Vec::with_capacity(512);
        while let Some(msg) = outbox.recv().await {
            buf.clear();
            enforcer::encode_frame(&msg, &mut buf)
                .map_err(|e| anyhow::anyhow!("a message for the enforcer would not encode: {e}"))?;
            tokio::time::timeout(WRITE_TIMEOUT, tx.write_all(&buf))
                .await
                .map_err(|_| anyhow::anyhow!("the enforcer stopped reading"))??;
        }
        // The sender went: this node dropped the connection itself.
        anyhow::Ok(())
    };
    let reading = read_loop(&mut rx, reader, state, id);

    let result = tokio::select! {
        r = writer => r,
        r = reading => r,
        () = close.notified() => Err(anyhow::anyhow!("this node dropped the connection")),
    };

    state.lock().expect("not poisoned").drop_link_if(id);
    result.map_err(Ended::Error)
}

/// Read until the enforcer's `hello`. Anything else first is a protocol error.
async fn read_hello(
    rx: &mut tokio::net::unix::OwnedReadHalf,
    reader: &mut FrameReader,
) -> Result<Hello> {
    let mut buf = [0u8; 4096];
    loop {
        if let Some(msg) = reader.next_enforcer_message() {
            return match msg {
                Ok(EnforcerMessage::Hello(hello)) => Ok(hello),
                Ok(other) => bail!("the enforcer sent {:?} before hello", other.msg_type()),
                Err(e) => bail!("the enforcer's first message did not decode: {e}"),
            };
        }
        let n = rx.read(&mut buf).await?;
        if n == 0 {
            bail!("the enforcer closed before saying hello");
        }
        reader.push(&buf[..n]);
    }
}

/// Take counters and conflicts from the enforcer until it stops, or breaks
/// the protocol.
async fn read_loop(
    rx: &mut tokio::net::unix::OwnedReadHalf,
    mut reader: FrameReader,
    state: &Arc<Mutex<State>>,
    id: u64,
) -> Result<()> {
    let mut buf = vec![0u8; 8 * 1024];
    loop {
        while let Some(msg) = reader.next_enforcer_message() {
            let msg = msg
                .map_err(|e| anyhow::anyhow!("a message from the enforcer did not decode: {e}"))?;
            let mut state = state.lock().expect("not poisoned");
            // A connection this node has already dropped speaks for nobody.
            if state.link.as_ref().is_none_or(|l| l.id != id) {
                return Ok(());
            }
            match msg {
                EnforcerMessage::Counters(c) => {
                    // A payer removed a moment ago may still be reported.
                    let Some(entry) = state.peers.get_mut(&c.peer) else {
                        continue;
                    };
                    let reported = Counters {
                        delivered: c.delivered,
                        received: c.received,
                    };
                    // Cumulative on this connection. One that goes backwards
                    // is the enforcer's mistake, and core must never see it.
                    if reported.delivered < entry.current.delivered
                        || reported.received < entry.current.received
                    {
                        warn!(peer = %c.peer, "the enforcer's counters went backwards; keeping the higher reading");
                    }
                    entry.current = Counters {
                        delivered: reported.delivered.max(entry.current.delivered),
                        received: reported.received.max(entry.current.received),
                    };
                }
                EnforcerMessage::Conflict(c) => {
                    let subject = state.describe(&c.subject);
                    let Some(entry) = state.peers.get_mut(&c.peer) else {
                        continue;
                    };
                    if !entry.conflicted {
                        error!(
                            peer = %c.peer,
                            %subject,
                            "the enforcer refused a subject to this payer because another \
                             payer holds it; selling it nothing"
                        );
                    }
                    entry.conflicted = true;
                }
                EnforcerMessage::Hello(_) => bail!("the enforcer said hello twice"),
                other => bail!(
                    "the enforcer sent {:?}, which only this node sends",
                    other.msg_type()
                ),
            }
        }

        let n = rx.read(&mut buf).await?;
        if n == 0 {
            bail!("the enforcer closed the connection");
        }
        reader.push(&buf[..n]);
        if reader.pending() > MAX_FRAME_LEN * 2 {
            bail!("the enforcer is buffering more than two maximum frames without completing one");
        }
    }
}

impl Enforcer for External {
    /// Bind the payer to what this node knows of it, and send its rate — `0`
    /// until core says otherwise. The enforcer hears of it before its Offer.
    fn register(&self, peer: PubKey, addr: IpAddr) {
        let mut state = self.state.lock().expect("not poisoned");
        match state.peers.get_mut(&peer) {
            Some(entry) if entry.addr == addr => return,
            Some(entry) => {
                entry.addr = addr;
                // A new subject gets a fresh answer from the enforcer.
                entry.conflicted = false;
            }
            None => {
                state.next_seq += 1;
                let seq = state.next_seq;
                state.peers.insert(
                    peer,
                    Peer {
                        seq,
                        addr,
                        delegated: Vec::new(),
                        rate: 0,
                        demand: 0,
                        base: Counters::default(),
                        current: Counters::default(),
                        conflicted: false,
                    },
                );
            }
        }
        debug!(%peer, %addr, "payer bound at the enforcer");
        state.bind(peer);
        state.set(peer);
    }

    /// Nothing to send: the rate already says whether the payer is carried.
    fn set_access(&self, _peer: PubKey, _access: AccessLevel) {}

    fn set_shaping_rate(&self, peer: PubKey, rate: u64) {
        let mut state = self.state.lock().expect("not poisoned");
        let Some(entry) = state.peers.get_mut(&peer) else {
            return;
        };
        if entry.rate == rate {
            return;
        }
        entry.rate = rate;
        state.set(peer);
    }

    fn counters(&self, peer: PubKey) -> Counters {
        self.state
            .lock()
            .expect("not poisoned")
            .peers
            .get(&peer)
            .map(Peer::total)
            .unwrap_or_default()
    }

    /// Told rather than measured, as for the FIPS enforcer: an external one
    /// reports what it carried, not what anyone wanted.
    fn demand(&self, peer: PubKey) -> u64 {
        self.state
            .lock()
            .expect("not poisoned")
            .peers
            .get(&peer)
            .map(|p| p.demand)
            .unwrap_or(0)
    }

    fn set_demand(&self, peer: PubKey, rate: u64) {
        if let Some(entry) = self
            .state
            .lock()
            .expect("not poisoned")
            .peers
            .get_mut(&peer)
        {
            entry.demand = rate;
        }
    }

    fn shaping_rate(&self, peer: PubKey) -> u64 {
        self.state
            .lock()
            .expect("not poisoned")
            .peers
            .get(&peer)
            .map(|p| p.rate)
            .unwrap_or(0)
    }

    fn peers(&self) -> Vec<PubKey> {
        self.state
            .lock()
            .expect("not poisoned")
            .peers
            .keys()
            .copied()
            .collect()
    }

    fn remove(&self, peer: PubKey) {
        let mut state = self.state.lock().expect("not poisoned");
        if state.peers.remove(&peer).is_none() {
            return;
        }
        state.send(EnforcerMessage::Remove(Remove { peer }));
    }

    /// Only with an enforcer connected that this node accepted, and never to a
    /// payer the enforcer refused a subject to.
    fn selling(&self, peer: PubKey) -> bool {
        let state = self.state.lock().expect("not poisoned");
        state.link.is_some() && !state.peers.get(&peer).is_some_and(|p| p.conflicted)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hello(identity: PeerIdentity, unit: &str) -> Hello {
        Hello {
            version: PROTOCOL_VERSION,
            identity,
            delegated: false,
            unit: unit.into(),
        }
    }

    fn expected(identity: PeerIdentity, unit: &str) -> Expected {
        Expected {
            identity,
            unit: unit.into(),
        }
    }

    #[test]
    fn a_hello_that_agrees_with_the_instance_is_accepted() {
        for identity in [PeerIdentity::Address, PeerIdentity::Pubkey] {
            assert_eq!(
                check_hello(&hello(identity, "byte"), &expected(identity, "byte")),
                Ok(())
            );
        }
        assert_eq!(
            check_hello(
                &hello(PeerIdentity::Address, "ml"),
                &expected(PeerIdentity::Address, "ml")
            ),
            Ok(())
        );
    }

    #[test]
    fn another_identity_is_refused_naming_both() {
        let refusal = check_hello(
            &hello(PeerIdentity::Pubkey, "byte"),
            &expected(PeerIdentity::Address, "byte"),
        )
        .expect_err("a key-matching enforcer behind an address instance");
        assert_eq!(
            refusal,
            Refusal::Identity {
                config: PeerIdentity::Address,
                hello: PeerIdentity::Pubkey
            }
        );
        let text = refusal.to_string();
        assert!(text.contains("enforcer.identity is address"), "{text}");
        assert!(text.contains("says pubkey"), "{text}");
    }

    #[test]
    fn another_unit_is_refused_naming_both() {
        // A tap counting litres for an instance selling millilitres.
        let refusal = check_hello(
            &hello(PeerIdentity::Address, "l"),
            &expected(PeerIdentity::Address, "ml"),
        )
        .expect_err("units differ");
        assert_eq!(
            refusal,
            Refusal::Unit {
                config: "ml".into(),
                hello: "l".into()
            }
        );
        let text = refusal.to_string();
        assert!(text.contains("\"ml\"") && text.contains("\"l\""), "{text}");

        // Plain text: nothing is normalised.
        assert!(
            check_hello(
                &hello(PeerIdentity::Address, "bytes"),
                &expected(PeerIdentity::Address, "byte")
            )
            .is_err()
        );
    }

    #[test]
    fn another_version_is_not_a_mismatch_but_a_retry() {
        let mut h = hello(PeerIdentity::Address, "byte");
        h.version = 2;
        assert_eq!(
            check_hello(&h, &expected(PeerIdentity::Address, "byte")),
            Err(Refusal::Version(2))
        );
    }

    #[test]
    fn a_payer_is_bound_in_the_one_form_its_identity_fixes() {
        let peer = PubKey([2; 33]);
        let addr: IpAddr = "192.168.1.23".parse().unwrap();
        for (identity, len) in [(PeerIdentity::Pubkey, 32), (PeerIdentity::Address, 16)] {
            let mut state = State::new(identity);
            state.hello = Some(hello(identity, "byte"));
            state.peers.insert(
                peer,
                Peer {
                    seq: 1,
                    addr,
                    delegated: vec![Subject::new(*b"tap-3").unwrap()],
                    rate: 0,
                    demand: 0,
                    base: Counters::default(),
                    current: Counters::default(),
                    conflicted: false,
                },
            );
            // The delegated subject is left out: this enforcer refuses them.
            let bindings = state.bindings(peer);
            assert_eq!(bindings.len(), 1, "{identity}");
            assert_eq!(bindings[0].subject.as_bytes().len(), len, "{identity}");
            assert!(!bindings[0].delegated);
        }
    }

    #[test]
    fn an_address_subject_reads_as_an_address() {
        let state = State::new(PeerIdentity::Address);
        let subject = Subject::address("192.168.1.23".parse().unwrap());
        assert_eq!(state.describe(&subject), "192.168.1.23");
    }

    #[test]
    fn an_unmetered_payer_is_open_and_unshaped() {
        assert_eq!(wire_rate(u64::MAX), None);
        assert_eq!(wire_rate(0), Some(0));
        assert_eq!(wire_rate(3_276_800), Some(3_276_800));
    }
}
