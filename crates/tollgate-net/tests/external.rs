//! `forwarding.mode: external` against a stub gate.
//!
//! The stub is this test: it listens where the adapter connects, says `hello`,
//! and asserts on what `tollgated`'s side sends it — or sends what a real gate
//! would, and what a broken one might. The protocol is
//! `docs/design/core/tollgate-gate-protocol.md`.
#![cfg(unix)]

use std::net::{IpAddr, Ipv4Addr};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, UnixListener, UnixStream};
use tollgate_core::buyer::BuyerPolicy;
use tollgate_core::config::{GrantPolicy, NodePolicy, PeerPolicy};
use tollgate_core::meter::Counters as MeterCounters;
use tollgate_net::adapter::{DelegateError, External, Loopback, ResourceAdapter};
use tollgate_net::channel::LocalChannels;
use tollgate_net::identity::Identity;
use tollgate_net::node::{Node, NodeConfig, PeerConfig};
use tollgate_net::wire::Identify;
use tollgate_protocol::gate::{
    self, Binding, Conflict, Counters, GATE_PROTOCOL_VERSION, GateMessage, Hello, Subject,
    SubjectKind,
};
use tollgate_protocol::{FrameReader, PubKey};

const PATIENCE: Duration = Duration::from_secs(60);

// ---------------------------------------------------------------------------
// The stub gate
// ---------------------------------------------------------------------------

/// A socket path of this test's own. Short, because a Unix socket path has a
/// length limit and a temporary directory on macOS is already long.
fn socket_path() -> PathBuf {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("tg-gate-{}-{n}.sock", std::process::id()));
    let _ = std::fs::remove_file(&path);
    path
}

struct Stub {
    listener: UnixListener,
    path: PathBuf,
}

impl Stub {
    fn new() -> Self {
        let path = socket_path();
        let listener = UnixListener::bind(&path).expect("bind the gate socket");
        Self { listener, path }
    }

    async fn accept(&self) -> Conn {
        let (stream, _) = tokio::time::timeout(PATIENCE, self.listener.accept())
            .await
            .expect("tollgated to connect to the gate")
            .expect("accept");
        Conn {
            stream,
            reader: FrameReader::new(),
        }
    }
}

impl Drop for Stub {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

struct Conn {
    stream: UnixStream,
    reader: FrameReader,
}

impl Conn {
    async fn send(&mut self, msg: GateMessage) {
        let mut out = Vec::new();
        gate::encode_frame(&msg, &mut out).expect("encode");
        self.stream.write_all(&out).await.expect("write");
    }

    async fn hello(&mut self, hello: Hello) {
        self.send(GateMessage::Hello(hello)).await;
    }

    /// The next message, or `None` once `tollgated` has closed the connection.
    async fn recv(&mut self) -> Option<GateMessage> {
        let mut buf = [0u8; 4096];
        loop {
            if let Some(msg) = self.reader.next_gate_message() {
                return Some(msg.expect("tollgated sent a malformed message"));
            }
            let n = tokio::time::timeout(PATIENCE, self.stream.read(&mut buf))
                .await
                .expect("a message from tollgated")
                .unwrap_or(0);
            if n == 0 {
                return None;
            }
            self.reader.push(&buf[..n]);
        }
    }

    /// Skip messages until one matches.
    async fn until(&mut self, what: &str, mut f: impl FnMut(&GateMessage) -> bool) -> GateMessage {
        loop {
            match self.recv().await {
                Some(msg) if f(&msg) => return msg,
                Some(_) => {}
                None => panic!("the connection closed while waiting for {what}"),
            }
        }
    }

    /// Whether `tollgated` closes the connection, reading and discarding
    /// anything it sends first.
    async fn closed_by_tollgated(&mut self) -> bool {
        let mut buf = [0u8; 4096];
        loop {
            match tokio::time::timeout(Duration::from_secs(10), self.stream.read(&mut buf)).await {
                Ok(Ok(0)) | Ok(Err(_)) => return true,
                Ok(Ok(_)) => {}
                Err(_) => return false,
            }
        }
    }
}

fn lan_hello() -> Hello {
    Hello {
        version: GATE_PROTOCOL_VERSION,
        kinds: vec![SubjectKind::Ipv4, SubjectKind::Ipv6],
        identify: gate::Identify::Claimed,
        delegated: false,
        opaque_kinds: vec![],
    }
}

fn fips_hello() -> Hello {
    Hello {
        version: GATE_PROTOCOL_VERSION,
        kinds: vec![SubjectKind::Pubkey],
        identify: gate::Identify::Fips,
        delegated: false,
        opaque_kinds: vec![],
    }
}

/// Start the adapter against `stub`, answering its connection with `hello`.
async fn connected(stub: &Stub, hello: Hello) -> (External, Identify, Conn) {
    let connecting = tokio::spawn(External::connect(stub.path.clone(), None));
    let mut conn = stub.accept().await;
    conn.hello(hello).await;
    let (adapter, mode) = connecting.await.expect("join").expect("connect");
    (adapter, mode, conn)
}

fn key(seed: u8) -> PubKey {
    Identity::from_hex(&format!("{seed:02x}").repeat(32))
        .expect("a valid key")
        .pubkey()
}

fn lan(last: u8) -> IpAddr {
    IpAddr::V4(Ipv4Addr::new(192, 168, 1, last))
}

async fn wait_for(label: &str, mut check: impl FnMut() -> bool) {
    let deadline = Instant::now() + PATIENCE;
    while Instant::now() < deadline {
        if check() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("timed out waiting for {label}");
}

fn is_set(msg: &GateMessage, peer: PubKey, rate: Option<u64>) -> bool {
    matches!(msg, GateMessage::Set(s) if s.peer == peer && s.rate == rate)
}

// ---------------------------------------------------------------------------
// The adapter against the stub
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_payer_is_bound_to_its_address_and_starts_closed() {
    let stub = Stub::new();
    let (adapter, mode, mut conn) = connected(&stub, lan_hello()).await;
    assert_eq!(mode, Identify::Claimed, "the gate's hello decides the mode");

    let payer = key(1);
    adapter.register(payer, lan(23));

    // The subject is what this node saw, and the payer's rate is zero until
    // core says otherwise.
    assert_eq!(
        conn.recv().await,
        Some(GateMessage::Bind(gate::Bind {
            peer: payer,
            bindings: vec![Binding {
                subject: Subject::Ipv4([192, 168, 1, 23]),
                delegated: false,
            }],
        }))
    );
    assert!(is_set(&conn.recv().await.unwrap(), payer, Some(0)));

    // Open, shaped, closed again; unmetered is open and unshaped.
    adapter.set_shaping_rate(payer, 3_276_800);
    assert!(is_set(&conn.recv().await.unwrap(), payer, Some(3_276_800)));
    adapter.set_shaping_rate(payer, 0);
    assert!(is_set(&conn.recv().await.unwrap(), payer, Some(0)));
    adapter.set_shaping_rate(payer, u64::MAX);
    assert!(is_set(&conn.recv().await.unwrap(), payer, None));

    // An access level says nothing a rate does not.
    adapter.set_access(payer, tollgate_core::access::AccessLevel::Active);
    adapter.remove(payer);
    assert_eq!(
        conn.recv().await,
        Some(GateMessage::Remove(gate::Remove { peer: payer }))
    );
}

#[tokio::test]
async fn under_fips_the_key_itself_is_bound() {
    let stub = Stub::new();
    let (adapter, mode, mut conn) = connected(&stub, fips_hello()).await;
    assert_eq!(mode, Identify::Fips);

    let payer = key(2);
    adapter.register(payer, "fd00::1".parse().unwrap());
    let GateMessage::Bind(bind) = conn.recv().await.unwrap() else {
        panic!("expected a bind");
    };
    // Never the address: under fips it is a hash of the key, and the gate
    // derives it itself.
    assert_eq!(
        bind.bindings,
        vec![Binding {
            subject: Subject::Pubkey(payer.0[1..].try_into().unwrap()),
            delegated: false,
        }]
    );
}

#[tokio::test]
async fn a_subject_the_gate_does_not_match_is_never_sent() {
    // Sending one would be a protocol error. An IPv4 payer behind an
    // IPv6-only gate is bound to nothing, which leaves it closed.
    let stub = Stub::new();
    let mut hello = lan_hello();
    hello.kinds = vec![SubjectKind::Ipv6];
    let (adapter, _, mut conn) = connected(&stub, hello).await;

    let payer = key(3);
    adapter.register(payer, lan(5));
    assert_eq!(
        conn.recv().await,
        Some(GateMessage::Bind(gate::Bind {
            peer: payer,
            bindings: vec![],
        }))
    );
}

#[tokio::test]
async fn counters_reach_the_adapter_and_are_rebased_across_reconnects() {
    let stub = Stub::new();
    let (adapter, _, mut conn) = connected(&stub, lan_hello()).await;
    let payer = key(4);
    adapter.register(payer, lan(4));
    adapter.set_shaping_rate(payer, 5_000);
    conn.until("the rate", |m| is_set(m, payer, Some(5_000)))
        .await;

    conn.send(GateMessage::Counters(Counters {
        peer: payer,
        delivered: 100,
        received: 10,
    }))
    .await;
    let probe = adapter.clone();
    wait_for("the first counters", move || {
        probe.counters(payer)
            == MeterCounters {
                delivered: 100,
                received: 10,
            }
    })
    .await;

    // The gate goes away: nothing is sold, and nothing it counted is lost.
    drop(conn);
    let probe = adapter.clone();
    wait_for("the adapter to notice", move || !probe.selling(payer)).await;
    assert_eq!(adapter.counters(payer).delivered, 100);

    // It comes back from closed, so it is told everything again.
    let mut conn = stub.accept().await;
    conn.hello(lan_hello()).await;
    assert!(matches!(
        conn.recv().await,
        Some(GateMessage::Bind(ref b)) if b.peer == payer && b.bindings.len() == 1
    ));
    assert!(is_set(&conn.recv().await.unwrap(), payer, Some(5_000)));
    assert!(adapter.selling(payer));

    // Its counts start again from zero; the totals do not.
    conn.send(GateMessage::Counters(Counters {
        peer: payer,
        delivered: 30,
        received: 3,
    }))
    .await;
    let probe = adapter.clone();
    wait_for("the rebased counters", move || {
        probe.counters(payer)
            == MeterCounters {
                delivered: 130,
                received: 13,
            }
    })
    .await;

    // A reading that goes backwards on one connection is ignored.
    conn.send(GateMessage::Counters(Counters {
        peer: payer,
        delivered: 1,
        received: 1,
    }))
    .await;
    conn.send(GateMessage::Counters(Counters {
        peer: key(99),
        delivered: 1,
        received: 1,
    }))
    .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(adapter.counters(payer).delivered, 130);
}

#[tokio::test]
async fn nothing_is_ready_before_the_gate_says_hello() {
    let stub = Stub::new();
    let connecting = tokio::spawn(External::connect(stub.path.clone(), None));
    let _conn = stub.accept().await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        !connecting.is_finished(),
        "the node must not start before the gate names the Identify mode"
    );
    connecting.abort();
}

#[tokio::test]
async fn a_gate_that_is_not_there_yet_is_waited_for() {
    let path = socket_path();
    let connecting = tokio::spawn(External::connect(path.clone(), None));
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    assert!(!connecting.is_finished());

    let listener = UnixListener::bind(&path).expect("bind");
    let (stream, _) = listener.accept().await.expect("accept");
    let mut conn = Conn {
        stream,
        reader: FrameReader::new(),
    };
    conn.hello(lan_hello()).await;
    let (_adapter, mode) = connecting.await.expect("join").expect("connect");
    assert_eq!(mode, Identify::Claimed);
    let _ = std::fs::remove_file(&path);
}

// ---------------------------------------------------------------------------
// Identify coupling
// ---------------------------------------------------------------------------

/// The outcome of starting against a gate that says `hello`, with `pin`.
async fn start_against(hello: Hello, pin: Option<Identify>) -> anyhow::Result<Identify> {
    let stub = Stub::new();
    let connecting = tokio::spawn(External::connect(stub.path.clone(), pin));
    let mut conn = stub.accept().await;
    conn.hello(hello).await;
    connecting.await.expect("join").map(|(_, mode)| mode)
}

#[tokio::test]
async fn a_gate_asking_for_the_other_mode_than_the_pin_refuses_to_start() {
    let err = start_against(fips_hello(), Some(Identify::Claimed))
        .await
        .expect_err("a pinned claimed node behind a fips gate");
    let text = format!("{err:#}");
    assert!(
        text.contains("claimed") && text.contains("fips"),
        "names both: {text}"
    );

    assert!(
        start_against(lan_hello(), Some(Identify::Fips))
            .await
            .is_err()
    );
    // And a pin the gate agrees with is no obstacle.
    assert_eq!(
        start_against(fips_hello(), Some(Identify::Fips))
            .await
            .unwrap(),
        Identify::Fips
    );
}

#[tokio::test]
async fn a_hello_that_contradicts_itself_refuses_to_start() {
    let mut claimed_key = fips_hello();
    claimed_key.identify = gate::Identify::Claimed;
    assert!(start_against(claimed_key, None).await.is_err());

    let mut fips_without_key = lan_hello();
    fips_without_key.identify = gate::Identify::Fips;
    assert!(start_against(fips_without_key, None).await.is_err());
}

#[tokio::test]
async fn a_reconnecting_gate_that_changes_the_mode_is_refused() {
    let stub = Stub::new();
    let (adapter, _, conn) = connected(&stub, lan_hello()).await;
    let payer = key(5);
    adapter.register(payer, lan(5));
    drop(conn);

    // The same socket, now asking for fips: refused, and nothing is sold.
    let mut conn = stub.accept().await;
    conn.hello(fips_hello()).await;
    assert!(conn.closed_by_tollgated().await, "refused and closed");
    assert!(!adapter.selling(payer));

    // A gate that asks for the mode the node runs is taken back.
    let mut conn = stub.accept().await;
    conn.hello(lan_hello()).await;
    conn.until("the full state", |m| is_set(m, payer, Some(0)))
        .await;
    assert!(adapter.selling(payer));
}

#[tokio::test]
async fn another_gate_protocol_version_is_closed_and_tried_again() {
    let stub = Stub::new();
    let connecting = tokio::spawn(External::connect(stub.path.clone(), None));
    let mut conn = stub.accept().await;
    let mut hello = lan_hello();
    hello.version = 2;
    conn.hello(hello).await;
    assert!(conn.closed_by_tollgated().await);
    assert!(!connecting.is_finished(), "not a reason to refuse to start");

    let mut conn = stub.accept().await;
    conn.hello(lan_hello()).await;
    connecting.await.expect("join").expect("connect");
}

// ---------------------------------------------------------------------------
// Delegated bindings, conflicts, protocol errors
// ---------------------------------------------------------------------------

#[tokio::test]
async fn delegated_bindings_are_refused_when_the_gate_refuses_them() {
    let stub = Stub::new();
    let (adapter, _, mut conn) = connected(&stub, lan_hello()).await;
    let payer = key(6);
    adapter.register(payer, lan(6));
    conn.until("the payer's set", |m| is_set(m, payer, Some(0)))
        .await;

    assert_eq!(
        adapter.delegate(payer, Subject::Ipv4([192, 168, 1, 40])),
        Err(DelegateError::Refused)
    );
    // Turned down here, so nothing reaches the gate: the next thing it hears
    // is the rate below, not a bind.
    adapter.set_shaping_rate(payer, 7);
    assert!(is_set(&conn.recv().await.unwrap(), payer, Some(7)));
}

#[tokio::test]
async fn a_delegated_binding_reaches_a_gate_that_accepts_them_flagged() {
    let stub = Stub::new();
    let mut hello = lan_hello();
    hello.delegated = true;
    let (adapter, _, mut conn) = connected(&stub, hello).await;
    let payer = key(7);
    adapter.register(payer, lan(7));
    conn.until("the payer's set", |m| is_set(m, payer, Some(0)))
        .await;

    assert_eq!(
        adapter.delegate(key(8), Subject::Ipv4([192, 168, 1, 41])),
        Err(DelegateError::UnknownPayer)
    );
    assert_eq!(
        adapter.delegate(payer, Subject::Mac([1; 6])),
        Err(DelegateError::KindNotMatched(SubjectKind::Mac))
    );
    adapter
        .delegate(payer, Subject::Ipv4([192, 168, 1, 40]))
        .expect("delegate");
    assert_eq!(
        conn.recv().await,
        Some(GateMessage::Bind(gate::Bind {
            peer: payer,
            bindings: vec![
                Binding {
                    subject: Subject::Ipv4([192, 168, 1, 7]),
                    delegated: false,
                },
                Binding {
                    subject: Subject::Ipv4([192, 168, 1, 40]),
                    delegated: true,
                },
            ],
        }))
    );
}

#[tokio::test]
async fn a_payer_holds_at_most_one_binds_worth_of_subjects() {
    let stub = Stub::new();
    let mut hello = lan_hello();
    hello.delegated = true;
    let (adapter, _, mut conn) = connected(&stub, hello).await;
    let payer = key(12);
    adapter.register(payer, lan(12));
    conn.until("the payer's set", |m| is_set(m, payer, Some(0)))
        .await;

    // Its own, and seven more.
    for i in 0..7 {
        adapter
            .delegate(payer, Subject::Ipv4([10, 0, 0, i]))
            .expect("room left");
    }
    let GateMessage::Bind(bind) = conn
        .until(
            "the last bind",
            |m| matches!(m, GateMessage::Bind(b) if b.bindings.len() == gate::MAX_BINDINGS),
        )
        .await
    else {
        unreachable!()
    };
    assert_eq!(bind.bindings.iter().filter(|b| b.delegated).count(), 7);
    assert_eq!(
        adapter.delegate(payer, Subject::Ipv4([10, 0, 0, 99])),
        Err(DelegateError::TooMany)
    );
}

#[tokio::test]
async fn a_conflict_stops_sales_to_that_payer_only() {
    let stub = Stub::new();
    let (adapter, _, mut conn) = connected(&stub, lan_hello()).await;
    let (first, second) = (key(9), key(10));
    adapter.register(first, lan(23));
    adapter.register(second, lan(23));
    conn.until("the second payer's set", |m| is_set(m, second, Some(0)))
        .await;

    conn.send(GateMessage::Conflict(Conflict {
        peer: second,
        subject: Subject::Ipv4([192, 168, 1, 23]),
    }))
    .await;
    let probe = adapter.clone();
    wait_for("the conflict", move || !probe.selling(second)).await;
    assert!(adapter.selling(first), "the first payer keeps the subject");
}

/// Whether `tollgated` closes the connection after the gate sends `bytes`,
/// framed, once `hello` is done — and stops selling until the next one.
async fn closes_on(frame: Vec<u8>) -> bool {
    let stub = Stub::new();
    let (adapter, _, mut conn) = connected(&stub, lan_hello()).await;
    let payer = key(11);
    adapter.register(payer, lan(11));
    conn.stream.write_all(&frame).await.expect("write");
    let closed = conn.closed_by_tollgated().await;
    if closed {
        let probe = adapter.clone();
        wait_for("sales to stop", move || !probe.selling(payer)).await;
    }
    closed
}

fn framed(body: &[u8]) -> Vec<u8> {
    let mut out = (body.len() as u16).to_le_bytes().to_vec();
    out.extend_from_slice(body);
    out
}

fn framed_msg(msg: GateMessage) -> Vec<u8> {
    let mut out = Vec::new();
    gate::encode_frame(&msg, &mut out).expect("encode");
    out
}

#[tokio::test]
async fn protocol_errors_close_the_connection() {
    // Not CBOR at all.
    assert!(closes_on(framed(&[0xff, 0x00, 0x13])).await);
    // A wire-protocol message on the gate socket.
    let mut wire = Vec::new();
    tollgate_protocol::encode_frame(
        &tollgate_protocol::Message::Disconnect(tollgate_protocol::Disconnect {
            reason: tollgate_protocol::ReasonCode::Other,
        }),
        &mut wire,
    )
    .expect("encode");
    assert!(closes_on(wire).await);
    // A message only tollgated sends.
    assert!(
        closes_on(framed_msg(GateMessage::Set(gate::Set {
            peer: key(1),
            rate: Some(1),
        })))
        .await
    );
    // A second hello.
    assert!(closes_on(framed_msg(GateMessage::Hello(lan_hello()))).await);
}

#[tokio::test]
async fn anything_before_hello_is_a_protocol_error() {
    let stub = Stub::new();
    let connecting = tokio::spawn(External::connect(stub.path.clone(), None));
    let mut conn = stub.accept().await;
    conn.send(GateMessage::Counters(Counters {
        peer: key(1),
        delivered: 1,
        received: 1,
    }))
    .await;
    assert!(conn.closed_by_tollgated().await);

    let mut conn = stub.accept().await;
    conn.hello(lan_hello()).await;
    connecting.await.expect("join").expect("connect");
}

// ---------------------------------------------------------------------------
// A node behind the gate
// ---------------------------------------------------------------------------

fn policy(mint: &str) -> NodePolicy {
    NodePolicy {
        unit: "byte".into(),
        accepted_mints: vec![mint.into()],
        received_multiplier: 0,
        // No allowance: an unpaid payer is closed, not trickled.
        minimum_flow: 0,
        grants: GrantPolicy {
            min_window_ms: 200,
            max_window_ms: 30_000,
            max_rate: None,
        },
        initial_channel_capacity: 10_000_000_000,
        min_channel_capacity: 1,
        max_channel_capacity: 1 << 34,
        capacity_growth_pct: 200,
        safety_margin_floor_ms: 60_000,
        stale_timeout_ms: 0,
        rollover_threshold_pct: 80,
    }
}

fn config(identity: Identity, listen: std::net::SocketAddr, peers: Vec<PeerConfig>) -> NodeConfig {
    NodeConfig {
        identity,
        policy: policy("https://unused.example/mint"),
        buyer: BuyerPolicy {
            window_ms: 1_000,
            renew_lead_ms: 300,
            // A refusal names a ceiling of zero while the gate is down; test
            // again soon rather than after the default ten seconds.
            cap_hold_ms: 1_000,
            ..BuyerPolicy::default()
        },
        listen,
        identify: Identify::Claimed,
        mint_url: "http://127.0.0.1/unused".into(),
        mint_local: "http://127.0.0.1/unused".into(),
        connector: None,
        channel_ttl_seconds: 3_600,
        peers,
    }
}

/// The provider behind the gate, and a payer buying from it.
struct Pair {
    provider: PubKey,
    published: tollgate_net::control::Published,
    payer: PubKey,
    payer_adapter: Arc<Loopback>,
}

impl Pair {
    /// The provider's view of the payer, if it has a session.
    fn session(&self) -> Option<tollgate_net::control::PeerSnapshot> {
        let payer = hex::encode(self.payer.0);
        self.published
            .load()
            .peers
            .iter()
            .find(|p| p.pubkey == payer)
            .cloned()
    }
}

async fn run_node(
    identity: Identity,
    peers: Vec<PeerConfig>,
    adapter: Arc<dyn ResourceAdapter>,
) -> (std::net::SocketAddr, tollgate_net::control::Published) {
    let control = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .await
        .expect("bind");
    let listen = control.local_addr().expect("addr");
    let config = config(identity.clone(), listen, peers);
    let node = Node::new(&config, Arc::new(LocalChannels::new(identity)), adapter);
    let published = node.published();
    tokio::spawn(async move {
        let _ = node.run_on(control, config, std::future::pending()).await;
    });
    (listen, published)
}

/// Start a provider behind `adapter`, and a payer dialling it.
async fn pair(adapter: External) -> Pair {
    let identity = Identity::generate();
    let provider = identity.pubkey();
    let (listen, published) = run_node(identity, vec![], Arc::new(adapter)).await;

    // The payer: an ordinary node on the loopback adapter. Its data plane is
    // not started; demand is told, not measured.
    let identity = Identity::generate();
    let payer = identity.pubkey();
    let payer_adapter = Arc::new(Loopback::new());
    run_node(
        identity,
        vec![PeerConfig {
            pubkey: provider,
            endpoint: Some(listen.to_string()),
            policy: PeerPolicy::default(),
        }],
        payer_adapter.clone(),
    )
    .await;

    Pair {
        provider,
        published,
        payer,
        payer_adapter,
    }
}

/// The first rate the gate hears for `payer`, and everything up to it.
async fn first_set(conn: &mut Conn, payer: PubKey) -> (Vec<GateMessage>, Option<u64>) {
    let mut before = Vec::new();
    loop {
        let msg = conn.recv().await.expect("the connection to stay up");
        if let GateMessage::Set(s) = &msg
            && s.peer == payer
        {
            return (before, s.rate);
        }
        before.push(msg);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn access_follows_payment_and_counters_reach_the_ledger() {
    let stub = Stub::new();
    let (adapter, _, mut conn) = connected(&stub, lan_hello()).await;
    let p = pair(adapter).await;

    // Bound before anything is sold, to the address it came from, and closed.
    let (before, rate) = first_set(&mut conn, p.payer).await;
    assert!(
        before.contains(&GateMessage::Bind(gate::Bind {
            peer: p.payer,
            bindings: vec![Binding {
                subject: Subject::Ipv4([127, 0, 0, 1]),
                delegated: false,
            }],
        })),
        "bound before its first set: {before:?}"
    );
    assert_eq!(rate, Some(0), "the gate starts closed");

    // It pays, and the gate opens at the rate it bought: 125% of 2 MB/s.
    p.payer_adapter.set_demand(p.provider, 2_000_000);
    conn.until("the payer to be opened at what it bought", |m| {
        is_set(m, p.payer, Some(2_500_000))
    })
    .await;

    // What the gate carried is what the grant is drawn down by.
    conn.send(GateMessage::Counters(Counters {
        peer: p.payer,
        delivered: 1_000_000,
        received: 50_000,
    }))
    .await;
    wait_for("the counters to reach the ledger", || {
        p.session()
            .is_some_and(|s| s.delivered == 1_000_000 && s.received == 50_000 && s.consumed > 0)
    })
    .await;

    // It stops paying, and the gate closes again.
    p.payer_adapter.set_demand(p.provider, 0);
    conn.until("the payer to be closed when its grant lapses", |m| {
        is_set(m, p.payer, Some(0))
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn nothing_is_sold_while_the_gate_is_down() {
    let stub = Stub::new();
    let (adapter, _, conn) = connected(&stub, lan_hello()).await;
    // The gate goes before the payer arrives.
    drop(conn);
    let probe = adapter.clone();
    wait_for("the adapter to notice", move || !probe.connected()).await;

    let p = pair(adapter.clone()).await;
    p.payer_adapter.set_demand(p.provider, 2_000_000);
    wait_for("the payer's session", || p.session().is_some()).await;
    tokio::time::sleep(Duration::from_secs(3)).await;
    let s = p.session().expect("the session is kept");
    assert_eq!(s.authorized, 0, "nothing bought while the gate is down");
    assert!(
        s.incoming_channels.is_empty(),
        "no new channel taken while the gate is down"
    );

    // The gate comes back: it is told everything, the channel is taken, and
    // the payer buys.
    let mut conn = stub.accept().await;
    conn.hello(lan_hello()).await;
    let (_, rate) = first_set(&mut conn, p.payer).await;
    assert_eq!(rate, Some(0));
    conn.until("the payer to be opened", |m| {
        is_set(m, p.payer, Some(2_500_000))
    })
    .await;

    // Down again, mid-session: sales stop, the session stays.
    drop(conn);
    let probe = adapter.clone();
    wait_for("the adapter to notice", move || !probe.connected()).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let bought = p.session().expect("kept").authorized;
    tokio::time::sleep(Duration::from_secs(3)).await;
    let s = p.session().expect("the session is kept through an outage");
    assert_eq!(
        s.authorized, bought,
        "nothing bought while the gate is down"
    );

    // And picks up where it was once the gate is back.
    let mut conn = stub.accept().await;
    conn.hello(lan_hello()).await;
    conn.until("the payer to be opened again", |m| {
        is_set(m, p.payer, Some(2_500_000))
    })
    .await;
    wait_for("a purchase after the outage", || {
        p.session().is_some_and(|s| s.authorized > bought)
    })
    .await;
}
