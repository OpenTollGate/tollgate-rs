//! Driving a gate: an enforcement program this node does not contain.
//!
//! The gate owns a data plane — a firewall, a proxy, a tunnel — and listens on
//! a Unix socket. This adapter connects to it, tells it which payer holds which
//! subjects and at what rate each payer is carried, and takes back what it
//! carried. The protocol is `docs/design/core/tollgate-gate-protocol.md`; the
//! messages are [`tollgate_protocol::gate`].
//!
//! # What the gate is told
//!
//! One [`Bind`] per payer, carrying every subject it holds, and one [`Set`]
//! carrying its rate: `0` closed, a number open and shaped, `null` open and
//! unshaped. That rate is the one core shapes the payer to, which is `0`
//! exactly when [`AccessLevel::carried`] says the payer is shut out — so
//! [`set_access`](ResourceAdapter::set_access) has nothing to add, and sends
//! nothing.
//!
//! What a payer is bound to is what this node genuinely has about it: under
//! [`Identify::Fips`] the key itself, which the mesh authenticated; under
//! [`Identify::Claimed`] the address the control connection came from, which
//! nothing did. Anything else a gate matches it derives for itself.
//!
//! # Failing closed
//!
//! A gate starts closed and closes again whenever the connection drops, so
//! every connection begins with the full state: a `bind` and a `set` for every
//! payer. While there is no connection — or before the first `hello` this node
//! accepts — [`selling`](ResourceAdapter::selling) is false, and the node sells
//! nothing. The counters are rebased across connections, so the totals core
//! sees never go backwards.
//!
//! # Identify
//!
//! The gate's `hello` names the Identify mode this node must run, because a
//! gate that matches a `pubkey` is trusting that key to have been checked.
//! [`External::connect`] waits for that `hello` and refuses a gate whose
//! demand contradicts itself, the operator's pin, or — on a reconnect — the
//! mode already running.

use std::collections::{HashMap, HashSet};
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
use tollgate_protocol::gate::{
    self, Bind, Binding, GATE_PROTOCOL_VERSION, GateMessage, Hello, MAX_BINDINGS, Remove, Set,
    Subject, SubjectKind,
};
use tollgate_protocol::{FrameReader, MAX_FRAME_LEN, PubKey};
use tracing::{debug, error, info, warn};

use super::ResourceAdapter;
use crate::wire::Identify;

/// How long to wait between attempts to reach the gate.
const RECONNECT: Duration = Duration::from_secs(1);

/// How long a gate that has accepted the connection has to say `hello`.
///
/// Nothing is sold until it does, so a gate that never speaks costs nothing
/// but time; this only decides how soon the connection is tried again.
const HELLO_TIMEOUT: Duration = Duration::from_secs(10);

/// Messages that may wait for the gate to read them.
///
/// A gate that stops reading is a gate this node can no longer steer. Past this
/// the connection is dropped, which closes the gate — the safe failure — and
/// the next connection starts from the full state.
const OUTBOX: usize = 4_096;

/// How long one write to the gate may take before the gate is taken to have
/// stopped reading, and the connection is closed.
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);

/// Why a `hello` was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// A gate protocol version this node does not speak. The connection is
    /// closed and tried again.
    Version(u8),
    /// A gate whose demand cannot be met safely. Refused at startup; on a
    /// reconnect the gate is refused and this node keeps selling nothing.
    Mismatch(String),
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Version(v) => write!(
                f,
                "the gate speaks gate protocol version {v}, and this node speaks {GATE_PROTOCOL_VERSION}"
            ),
            Self::Mismatch(why) => f.write_str(why),
        }
    }
}

/// The Identify mode a `hello` names.
fn mode(identify: gate::Identify) -> Identify {
    match identify {
        gate::Identify::Claimed => Identify::Claimed,
        gate::Identify::Fips => Identify::Fips,
    }
}

fn name(mode: Identify) -> &'static str {
    match mode {
        Identify::Claimed => "claimed",
        Identify::Fips => "fips",
    }
}

/// Decide whether this node can run behind a gate that said `hello`.
///
/// `pin` is `forwarding.identify`, if the operator wrote one; `running` is the
/// mode this node already runs, on a reconnect. Returns the mode to run.
pub fn check_hello(
    hello: &Hello,
    pin: Option<Identify>,
    running: Option<Identify>,
) -> Result<Identify, Refusal> {
    if hello.version != GATE_PROTOCOL_VERSION {
        return Err(Refusal::Version(hello.version));
    }
    let wanted = mode(hello.identify);
    let pubkey = hello.matches(SubjectKind::Pubkey);

    // A gate that matches a key trusts it to be the key at the other end of the
    // traffic, which only the FIPS check makes true.
    if pubkey && wanted == Identify::Claimed {
        return Err(Refusal::Mismatch(
            "the gate matches pubkey subjects but requires identify: claimed; a key \
             this node takes on its word must not open anything"
                .into(),
        ));
    }
    if wanted == Identify::Fips && !pubkey {
        return Err(Refusal::Mismatch(
            "the gate requires identify: fips but does not match pubkey subjects, \
             which is all this node binds under fips"
                .into(),
        ));
    }
    if let Some(pin) = pin
        && pin != wanted
    {
        return Err(Refusal::Mismatch(format!(
            "forwarding.identify is {}, and the gate requires {}",
            name(pin),
            name(wanted)
        )));
    }
    if let Some(running) = running
        && running != wanted
    {
        return Err(Refusal::Mismatch(format!(
            "this node runs identify: {}, and the reconnected gate requires {}; live \
             sessions were identified under the old mode",
            name(running),
            name(wanted)
        )));
    }
    Ok(wanted)
}

/// Why a delegated binding was turned down.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DelegateError {
    /// No gate has said `hello` yet, so nothing is known of what it accepts.
    #[error("no gate has said hello yet")]
    NoGate,
    /// The gate takes no third party's word.
    #[error("the gate refuses delegated bindings")]
    Refused,
    /// The gate does not match subjects of this kind.
    #[error("the gate does not match {0:?} subjects")]
    KindNotMatched(SubjectKind),
    /// The payer is not one this node tracks.
    #[error("no such payer")]
    UnknownPayer,
    /// The payer already holds as many subjects as one `bind` carries.
    #[error("the payer already holds {MAX_BINDINGS} subjects")]
    TooMany,
}

/// One payer, as this adapter knows it.
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
    /// The gate refused one of its subjects: it is sold nothing.
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

/// A live connection to the gate.
#[derive(Debug)]
struct Link {
    /// Which connection this is, so a connection that has ended cannot touch
    /// the state of the one after it.
    id: u64,
    out: mpsc::Sender<GateMessage>,
    /// Closes the socket at once when this node drops the link, rather than
    /// once whatever is queued has drained: the gate only closes when it sees
    /// the connection go.
    close: Arc<tokio::sync::Notify>,
}

#[derive(Debug, Default)]
struct State {
    peers: HashMap<PubKey, Peer>,
    link: Option<Link>,
    /// The last `hello` accepted. Kept across connections, for what it says
    /// about delegated bindings.
    hello: Option<Hello>,
    /// The mode this node runs, fixed by the first `hello` accepted.
    mode: Option<Identify>,
    next_id: u64,
    next_seq: u64,
    /// Payers already warned about for holding nothing the gate matches.
    unbindable: HashSet<PubKey>,
}

impl State {
    /// Everything `peer` holds that the current gate matches.
    fn bindings(&mut self, peer: PubKey) -> Vec<Binding> {
        let (Some(hello), Some(mode), Some(entry)) =
            (self.hello.as_ref(), self.mode, self.peers.get(&peer))
        else {
            return Vec::new();
        };
        // What this node itself has: the key the mesh checked, or the address
        // nothing did.
        let direct = match mode {
            Identify::Fips => Subject::pubkey_of(&peer),
            Identify::Claimed => Subject::from(entry.addr),
        };
        let all = std::iter::once(Binding {
            subject: direct,
            delegated: false,
        })
        .chain(entry.delegated.iter().map(|s| Binding {
            subject: s.clone(),
            delegated: true,
        }));
        // Sending a gate a kind it did not list is a protocol error, so what
        // it cannot match is left out rather than sent.
        let bindings: Vec<Binding> = all
            .filter(|b| hello.accepts(b))
            .take(MAX_BINDINGS)
            .collect();
        if bindings.is_empty() && self.unbindable.insert(peer) {
            warn!(
                %peer,
                addr = %entry.addr,
                "the gate matches nothing this node knows about the payer; it stays closed"
            );
        }
        bindings
    }

    /// Queue a message for the gate, if there is one.
    ///
    /// A gate that has stopped reading is dropped: the connection closes, the
    /// gate closes with it, and the next one starts from the full state.
    fn send(&mut self, msg: GateMessage) {
        let Some(link) = &self.link else {
            return;
        };
        if let Err(e) = link.out.try_send(msg) {
            warn!(error = %e, "the gate is not reading; dropping the connection");
            self.drop_link();
        }
    }

    fn bind(&mut self, peer: PubKey) {
        let bindings = self.bindings(peer);
        self.send(GateMessage::Bind(Bind { peer, bindings }));
    }

    fn set(&mut self, peer: PubKey) {
        let Some(entry) = self.peers.get(&peer) else {
            return;
        };
        let rate = wire_rate(entry.rate);
        self.send(GateMessage::Set(Set { peer, rate }));
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
}

/// A rate as the gate wants it: a number, or `null` for unshaped.
///
/// Core says `u64::MAX` for a peer it does not meter, and the gate has a word
/// for that which is not a very large number.
fn wire_rate(rate: u64) -> Option<u64> {
    (rate != u64::MAX).then_some(rate)
}

/// Drives a gate over its Unix socket.
#[derive(Debug, Clone)]
pub struct External {
    state: Arc<Mutex<State>>,
}

impl External {
    /// Reach the gate at `socket`, and wait for a `hello` this node can run
    /// behind.
    ///
    /// Returns the adapter and the Identify mode the gate requires, which is
    /// the mode the node must run. Waits as long as the gate is unreachable or
    /// speaks a version this node does not, retrying about once a second;
    /// fails if the gate's `hello` contradicts itself or `pin`, the
    /// operator's `forwarding.identify`.
    ///
    /// The connection is kept from then on: dropped, it is made again, and the
    /// gate is sent the full state.
    pub async fn connect(
        socket: impl Into<PathBuf>,
        pin: Option<Identify>,
    ) -> Result<(Self, Identify)> {
        let socket = socket.into();
        let adapter = Self {
            state: Arc::new(Mutex::new(State::default())),
        };
        let (first_tx, first_rx) = oneshot::channel();
        tokio::spawn(maintain(
            socket.clone(),
            Arc::clone(&adapter.state),
            pin,
            first_tx,
        ));
        match first_rx.await {
            Ok(Ok(mode)) => Ok((adapter, mode)),
            Ok(Err(refusal)) => bail!(
                "refusing to run behind the gate at {}: {refusal}",
                socket.display()
            ),
            Err(_) => bail!("the gate connection stopped before a hello"),
        }
    }

    /// Add a subject a local trusted client vouches for to `payer`'s
    /// bindings.
    ///
    /// Turned down here, rather than sent, when the gate refuses delegated
    /// bindings or does not match the subject's kind: sending either would be
    /// a protocol error. The payer must be one this node tracks. On success
    /// the payer's next `bind` carries the subject, flagged delegated.
    pub fn delegate(&self, payer: PubKey, subject: Subject) -> Result<(), DelegateError> {
        let mut state = self.state.lock().expect("not poisoned");
        let hello = state.hello.as_ref().ok_or(DelegateError::NoGate)?;
        let binding = Binding {
            subject: subject.clone(),
            delegated: true,
        };
        if !hello.delegated {
            return Err(DelegateError::Refused);
        }
        if !hello.accepts(&binding) {
            return Err(DelegateError::KindNotMatched(subject.kind()));
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

    /// Whether a gate this node accepted is connected now.
    pub fn connected(&self) -> bool {
        self.state.lock().expect("not poisoned").link.is_some()
    }
}

/// Keep a connection to the gate for as long as the node runs.
///
/// `first` hears the outcome of the first `hello` — accepted, or refused for a
/// mismatch — and the task ends if it is refused, since the node is then not
/// going to start.
async fn maintain(
    socket: PathBuf,
    state: Arc<Mutex<State>>,
    pin: Option<Identify>,
    first: oneshot::Sender<Result<Identify, Refusal>>,
) {
    let mut first = Some(first);
    let mut reported = false;
    let mut refused = false;
    loop {
        match UnixStream::connect(&socket).await {
            Ok(stream) => {
                reported = false;
                match session(stream, &state, pin, &mut first).await {
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
                                "refusing the gate: {refusal}; selling nothing until it is fixed"
                            );
                            refused = true;
                        }
                    }
                    Err(Ended::Error(e)) => {
                        refused = false;
                        warn!(
                            socket = %socket.display(),
                            error = format!("{e:#}"),
                            "the gate connection closed; selling nothing until it is back"
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
                        "the gate is unreachable; selling nothing until it answers"
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

/// How a connection to the gate ended.
enum Ended {
    /// The gate's `hello` was refused.
    Refused(Refusal),
    /// Anything else: the gate went away, or broke the protocol.
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
    pin: Option<Identify>,
    first: &mut Option<oneshot::Sender<Result<Identify, Refusal>>>,
) -> Result<(), Ended> {
    let (mut rx, mut tx) = stream.into_split();
    let mut reader = FrameReader::new();

    let hello = tokio::time::timeout(HELLO_TIMEOUT, read_hello(&mut rx, &mut reader))
        .await
        .map_err(|_| anyhow::anyhow!("the gate said nothing for {HELLO_TIMEOUT:?}"))??;

    let running = state.lock().expect("not poisoned").mode;
    let mode = match check_hello(&hello, pin, running) {
        Ok(mode) => mode,
        // Not a reason to refuse to start: the connection is closed and tried
        // again, in case the gate is being upgraded under us.
        Err(refusal @ Refusal::Version(_)) => {
            return Err(Ended::Error(anyhow::anyhow!("{refusal}")));
        }
        Err(refusal) => return Err(Ended::Refused(refusal)),
    };

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
        state.mode = Some(mode);
        state.hello = Some(hello.clone());
        state.unbindable.clear();
        state.next_id += 1;
        let id = state.next_id;
        state.link = Some(Link {
            id,
            out,
            close: Arc::clone(&close),
        });
        // A conflict belongs to the connection that reported it: the gate that
        // refused a binding has forgotten it, and reports it again on this one.
        // In the order the payers arrived, so the payer that held a subject
        // first is bound first again, and the same one is refused.
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
        kinds = ?hello.kinds,
        identify = name(mode),
        delegated = hello.delegated,
        "the gate said hello; selling"
    );
    if let Some(first) = first.take() {
        let _ = first.send(Ok(mode));
    }

    let writer = async {
        let mut buf = Vec::with_capacity(512);
        while let Some(msg) = outbox.recv().await {
            buf.clear();
            gate::encode_frame(&msg, &mut buf)
                .map_err(|e| anyhow::anyhow!("a message for the gate would not encode: {e}"))?;
            tokio::time::timeout(WRITE_TIMEOUT, tx.write_all(&buf))
                .await
                .map_err(|_| anyhow::anyhow!("the gate stopped reading"))??;
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

impl State {
    /// [`Self::drop_link`], if `id` is still the live connection.
    fn drop_link_if(&mut self, id: u64) {
        if self.link.as_ref().is_some_and(|l| l.id == id) {
            self.drop_link();
        }
    }
}

/// Read until the gate's `hello`. Anything else first is a protocol error.
async fn read_hello(
    rx: &mut tokio::net::unix::OwnedReadHalf,
    reader: &mut FrameReader,
) -> Result<Hello> {
    let mut buf = [0u8; 4096];
    loop {
        if let Some(msg) = reader.next_gate_message() {
            return match msg {
                Ok(GateMessage::Hello(hello)) => Ok(hello),
                Ok(other) => bail!("the gate sent {:?} before hello", other.msg_type()),
                Err(e) => bail!("the gate's first message did not decode: {e}"),
            };
        }
        let n = rx.read(&mut buf).await?;
        if n == 0 {
            bail!("the gate closed before saying hello");
        }
        reader.push(&buf[..n]);
    }
}

/// Take counters and conflicts from the gate until it stops, or breaks the
/// protocol.
async fn read_loop(
    rx: &mut tokio::net::unix::OwnedReadHalf,
    mut reader: FrameReader,
    state: &Arc<Mutex<State>>,
    id: u64,
) -> Result<()> {
    let mut buf = vec![0u8; 8 * 1024];
    loop {
        while let Some(msg) = reader.next_gate_message() {
            let msg =
                msg.map_err(|e| anyhow::anyhow!("a message from the gate did not decode: {e}"))?;
            let mut state = state.lock().expect("not poisoned");
            // A connection this node has already dropped speaks for nobody.
            if state.link.as_ref().is_none_or(|l| l.id != id) {
                return Ok(());
            }
            match msg {
                GateMessage::Counters(c) => {
                    // A payer removed a moment ago may still be reported.
                    let Some(entry) = state.peers.get_mut(&c.peer) else {
                        continue;
                    };
                    let reported = Counters {
                        delivered: c.delivered,
                        received: c.received,
                    };
                    // Cumulative on this connection. One that goes backwards
                    // is the gate's mistake, and core must never see it.
                    if reported.delivered < entry.current.delivered
                        || reported.received < entry.current.received
                    {
                        warn!(peer = %c.peer, "the gate's counters went backwards; keeping the higher reading");
                    }
                    entry.current = Counters {
                        delivered: reported.delivered.max(entry.current.delivered),
                        received: reported.received.max(entry.current.received),
                    };
                }
                GateMessage::Conflict(c) => {
                    let Some(entry) = state.peers.get_mut(&c.peer) else {
                        continue;
                    };
                    if !entry.conflicted {
                        error!(
                            peer = %c.peer,
                            subject = %c.subject,
                            "the gate refused a subject to this payer because another payer \
                             holds it; selling it nothing"
                        );
                    }
                    entry.conflicted = true;
                }
                GateMessage::Hello(_) => bail!("the gate said hello twice"),
                other => bail!(
                    "the gate sent {:?}, which only this node sends",
                    other.msg_type()
                ),
            }
        }

        let n = rx.read(&mut buf).await?;
        if n == 0 {
            bail!("the gate closed the connection");
        }
        reader.push(&buf[..n]);
        if reader.pending() > MAX_FRAME_LEN * 2 {
            bail!("the gate is buffering more than two maximum frames without completing one");
        }
    }
}

impl ResourceAdapter for External {
    /// Bind the payer to what this node knows of it, and send its rate — `0`
    /// until core says otherwise. The gate hears of it before its Offer.
    fn register(&self, peer: PubKey, addr: IpAddr) {
        let mut state = self.state.lock().expect("not poisoned");
        match state.peers.get_mut(&peer) {
            Some(entry) if entry.addr == addr => return,
            Some(entry) => {
                entry.addr = addr;
                // A new subject gets a fresh answer from the gate.
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
        debug!(%peer, %addr, "payer bound at the gate");
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

    /// Told rather than measured, as for the FIPS adapter: a gate reports what
    /// it carried, not what anyone wanted.
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
        state.unbindable.remove(&peer);
        state.send(GateMessage::Remove(Remove { peer }));
    }

    /// Only with a gate connected that this node accepted, and never to a
    /// payer the gate refused a subject to.
    fn selling(&self, peer: PubKey) -> bool {
        let state = self.state.lock().expect("not poisoned");
        state.link.is_some() && !state.peers.get(&peer).is_some_and(|p| p.conflicted)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hello(kinds: &[SubjectKind], identify: gate::Identify) -> Hello {
        Hello {
            version: GATE_PROTOCOL_VERSION,
            kinds: kinds.to_vec(),
            identify,
            delegated: false,
            opaque_kinds: vec![],
        }
    }

    #[test]
    fn a_lan_gate_runs_the_node_claimed() {
        let h = hello(
            &[SubjectKind::Ipv4, SubjectKind::Ipv6],
            gate::Identify::Claimed,
        );
        assert_eq!(check_hello(&h, None, None), Ok(Identify::Claimed));
        assert_eq!(
            check_hello(&h, Some(Identify::Claimed), None),
            Ok(Identify::Claimed)
        );
    }

    #[test]
    fn a_fips_gate_runs_the_node_fips() {
        let h = hello(&[SubjectKind::Pubkey], gate::Identify::Fips);
        assert_eq!(check_hello(&h, None, None), Ok(Identify::Fips));
        assert_eq!(
            check_hello(&h, None, Some(Identify::Fips)),
            Ok(Identify::Fips)
        );
    }

    #[test]
    fn a_pin_the_gate_contradicts_is_refused() {
        let fips = hello(&[SubjectKind::Pubkey], gate::Identify::Fips);
        assert!(matches!(
            check_hello(&fips, Some(Identify::Claimed), None),
            Err(Refusal::Mismatch(_))
        ));
        let lan = hello(&[SubjectKind::Ipv4], gate::Identify::Claimed);
        assert!(matches!(
            check_hello(&lan, Some(Identify::Fips), None),
            Err(Refusal::Mismatch(_))
        ));
    }

    #[test]
    fn a_hello_that_contradicts_itself_is_refused() {
        // A key matched but believed on its word: the hole the check closes.
        let claimed_key = hello(&[SubjectKind::Pubkey], gate::Identify::Claimed);
        assert!(matches!(
            check_hello(&claimed_key, None, None),
            Err(Refusal::Mismatch(_))
        ));
        let fips_no_key = hello(&[SubjectKind::Ipv6], gate::Identify::Fips);
        assert!(matches!(
            check_hello(&fips_no_key, None, None),
            Err(Refusal::Mismatch(_))
        ));
    }

    #[test]
    fn a_reconnecting_gate_may_not_change_the_mode() {
        let fips = hello(&[SubjectKind::Pubkey], gate::Identify::Fips);
        assert!(matches!(
            check_hello(&fips, None, Some(Identify::Claimed)),
            Err(Refusal::Mismatch(_))
        ));
    }

    #[test]
    fn another_version_is_not_a_mismatch_but_a_retry() {
        let mut h = hello(&[SubjectKind::Ipv4], gate::Identify::Claimed);
        h.version = 2;
        assert_eq!(check_hello(&h, None, None), Err(Refusal::Version(2)));
    }

    #[test]
    fn an_unmetered_payer_is_open_and_unshaped() {
        assert_eq!(wire_rate(u64::MAX), None);
        assert_eq!(wire_rate(0), Some(0));
        assert_eq!(wire_rate(3_276_800), Some(3_276_800));
    }
}
