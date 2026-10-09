//! `enforcer.kind: external` against a stub enforcer.
//!
//! The stub is this test: it listens where `tollgated` connects, says `hello`,
//! and asserts on what `tollgated`'s side sends it — or sends what a real
//! enforcer would, and what a broken one might. The protocol is
//! `docs/design/core/tollgate-enforcer-protocol.md`.
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
use tollgate_net::channel::LocalChannels;
use tollgate_net::enforcer::{DelegateError, Enforcer, Expected, External, Loopback};
use tollgate_net::identity::Identity;
use tollgate_net::node::{Node, NodeConfig, PeerConfig};
use tollgate_net::wire::PeerIdentity;
use tollgate_protocol::enforcer::{
    self, Binding, Conflict, Counters, EnforcerMessage, Hello, PROTOCOL_VERSION, Subject,
};
use tollgate_protocol::{FrameReader, PubKey};

const PATIENCE: Duration = Duration::from_secs(60);

// ---------------------------------------------------------------------------
// The stub enforcer
// ---------------------------------------------------------------------------

/// A socket path of this test's own. Short, because a Unix socket path has a
/// length limit and a temporary directory on macOS is already long.
fn socket_path() -> PathBuf {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("tg-enf-{}-{n}.sock", std::process::id()));
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
        let listener = UnixListener::bind(&path).expect("bind the enforcer socket");
        Self { listener, path }
    }

    async fn accept(&self) -> Conn {
        let (stream, _) = tokio::time::timeout(PATIENCE, self.listener.accept())
            .await
            .expect("tollgated to connect to the enforcer")
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
    async fn send(&mut self, msg: EnforcerMessage) {
        let mut out = Vec::new();
        enforcer::encode_frame(&msg, &mut out).expect("encode");
        self.stream.write_all(&out).await.expect("write");
    }

    async fn hello(&mut self, hello: Hello) {
        self.send(EnforcerMessage::Hello(hello)).await;
    }

    /// The next message, or `None` once `tollgated` has closed the connection.
    async fn recv(&mut self) -> Option<EnforcerMessage> {
        let mut buf = [0u8; 4096];
        loop {
            if let Some(msg) = self.reader.next_enforcer_message() {
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
    async fn until(
        &mut self,
        what: &str,
        mut f: impl FnMut(&EnforcerMessage) -> bool,
    ) -> EnforcerMessage {
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

/// An enforcer built for `address`, counting bytes: a LAN firewall.
fn lan_hello() -> Hello {
    Hello {
        version: PROTOCOL_VERSION,
        identity: PeerIdentity::Address,
        delegated: false,
        unit: "byte".into(),
    }
}

/// An enforcer built for `pubkey`, counting bytes: a FIPS exit proxy.
fn fips_hello() -> Hello {
    Hello {
        version: PROTOCOL_VERSION,
        identity: PeerIdentity::Pubkey,
        delegated: false,
        unit: "byte".into(),
    }
}

/// What an instance with `identity` and `unit` checks a hello against.
fn expect(identity: PeerIdentity, unit: &str) -> Expected {
    Expected {
        identity,
        unit: unit.into(),
    }
}

/// Start the enforcer against `stub`, configured as `hello` says, and answer
/// its connection with `hello`.
async fn connected(stub: &Stub, hello: Hello) -> (External, Conn) {
    let expected = expect(hello.identity, &hello.unit);
    let connecting = tokio::spawn(External::connect(stub.path.clone(), expected));
    let mut conn = stub.accept().await;
    conn.hello(hello).await;
    let enforcer = connecting.await.expect("join").expect("connect");
    (enforcer, conn)
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

fn is_set(msg: &EnforcerMessage, peer: PubKey, rate: Option<u64>) -> bool {
    matches!(msg, EnforcerMessage::Set(s) if s.peer == peer && s.rate == rate)
}

// ---------------------------------------------------------------------------
// The enforcer against the stub
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_payer_is_bound_to_its_address_and_starts_closed() {
    let stub = Stub::new();
    let (enforcer, mut conn) = connected(&stub, lan_hello()).await;

    let payer = key(1);
    enforcer.register(payer, lan(23));

    // The subject is what this node saw, in 16 bytes, and the payer's rate is
    // zero until core says otherwise.
    let mapped = [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xff, 0xff, 192, 168, 1, 23];
    assert_eq!(
        conn.recv().await,
        Some(EnforcerMessage::Bind(enforcer::Bind {
            peer: payer,
            bindings: vec![Binding {
                subject: Subject::new(mapped).unwrap(),
                delegated: false,
            }],
        }))
    );
    assert!(is_set(&conn.recv().await.unwrap(), payer, Some(0)));

    // Open, shaped, closed again; unmetered is open and unshaped.
    enforcer.set_shaping_rate(payer, 3_276_800);
    assert!(is_set(&conn.recv().await.unwrap(), payer, Some(3_276_800)));
    enforcer.set_shaping_rate(payer, 0);
    assert!(is_set(&conn.recv().await.unwrap(), payer, Some(0)));
    enforcer.set_shaping_rate(payer, u64::MAX);
    assert!(is_set(&conn.recv().await.unwrap(), payer, None));

    // An access level says nothing a rate does not.
    enforcer.set_access(payer, tollgate_core::access::AccessLevel::Active);
    enforcer.remove(payer);
    assert_eq!(
        conn.recv().await,
        Some(EnforcerMessage::Remove(enforcer::Remove { peer: payer }))
    );
}

#[tokio::test]
async fn under_pubkey_the_key_itself_is_bound() {
    let stub = Stub::new();
    let (enforcer, mut conn) = connected(&stub, fips_hello()).await;

    let payer = key(2);
    enforcer.register(payer, "fd00::1".parse().unwrap());
    let EnforcerMessage::Bind(bind) = conn.recv().await.unwrap() else {
        panic!("expected a bind");
    };
    // The 32-byte x-only key, never the address: the FIPS address is a hash
    // of the key, and the enforcer derives it itself.
    assert_eq!(
        bind.bindings,
        vec![Binding {
            subject: Subject::new(&payer.0[1..]).unwrap(),
            delegated: false,
        }]
    );
}

#[tokio::test]
async fn counters_reach_the_enforcer_and_are_rebased_across_reconnects() {
    let stub = Stub::new();
    let (enforcer, mut conn) = connected(&stub, lan_hello()).await;
    let payer = key(4);
    enforcer.register(payer, lan(4));
    enforcer.set_shaping_rate(payer, 5_000);
    conn.until("the rate", |m| is_set(m, payer, Some(5_000)))
        .await;

    conn.send(EnforcerMessage::Counters(Counters {
        peer: payer,
        delivered: 100,
        received: 10,
    }))
    .await;
    let probe = enforcer.clone();
    wait_for("the first counters", move || {
        probe.counters(payer)
            == MeterCounters {
                delivered: 100,
                received: 10,
            }
    })
    .await;

    // The enforcer goes away: nothing is sold, and nothing it counted is lost.
    drop(conn);
    let probe = enforcer.clone();
    wait_for("the enforcer to notice", move || !probe.selling(payer)).await;
    assert_eq!(enforcer.counters(payer).delivered, 100);

    // It comes back from closed, so it is told everything again.
    let mut conn = stub.accept().await;
    conn.hello(lan_hello()).await;
    assert!(matches!(
        conn.recv().await,
        Some(EnforcerMessage::Bind(ref b)) if b.peer == payer && b.bindings.len() == 1
    ));
    assert!(is_set(&conn.recv().await.unwrap(), payer, Some(5_000)));
    assert!(enforcer.selling(payer));

    // Its counts start again from zero; the totals do not.
    conn.send(EnforcerMessage::Counters(Counters {
        peer: payer,
        delivered: 30,
        received: 3,
    }))
    .await;
    let probe = enforcer.clone();
    wait_for("the rebased counters", move || {
        probe.counters(payer)
            == MeterCounters {
                delivered: 130,
                received: 13,
            }
    })
    .await;

    // A reading that goes backwards on one connection is ignored.
    conn.send(EnforcerMessage::Counters(Counters {
        peer: payer,
        delivered: 1,
        received: 1,
    }))
    .await;
    conn.send(EnforcerMessage::Counters(Counters {
        peer: key(99),
        delivered: 1,
        received: 1,
    }))
    .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(enforcer.counters(payer).delivered, 130);
}

#[tokio::test]
async fn nothing_is_ready_before_the_enforcer_says_hello() {
    let stub = Stub::new();
    let connecting = tokio::spawn(External::connect(
        stub.path.clone(),
        expect(PeerIdentity::Address, "byte"),
    ));
    let _conn = stub.accept().await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        !connecting.is_finished(),
        "the node must not start before the enforcer's hello is checked"
    );
    connecting.abort();
}

#[tokio::test]
async fn an_enforcer_that_is_not_there_yet_is_waited_for() {
    let path = socket_path();
    let connecting = tokio::spawn(External::connect(
        path.clone(),
        expect(PeerIdentity::Address, "byte"),
    ));
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    assert!(!connecting.is_finished());

    let listener = UnixListener::bind(&path).expect("bind");
    let (stream, _) = listener.accept().await.expect("accept");
    let mut conn = Conn {
        stream,
        reader: FrameReader::new(),
    };
    conn.hello(lan_hello()).await;
    connecting.await.expect("join").expect("connect");
    let _ = std::fs::remove_file(&path);
}

// ---------------------------------------------------------------------------
// The hello is a check: identity and unit
// ---------------------------------------------------------------------------

/// The outcome of starting an instance configured as `expected` against an
/// enforcer that says `hello`.
async fn start_against(expected: Expected, hello: Hello) -> anyhow::Result<External> {
    let stub = Stub::new();
    let connecting = tokio::spawn(External::connect(stub.path.clone(), expected));
    let mut conn = stub.accept().await;
    conn.hello(hello).await;
    connecting.await.expect("join")
}

#[tokio::test]
async fn an_enforcer_built_for_another_identity_refuses_to_start_naming_both() {
    // The hole the check closes: an enforcer that matches keys, paired by
    // mistake with an instance set to address.
    let err = start_against(expect(PeerIdentity::Address, "byte"), fips_hello())
        .await
        .expect_err("a pubkey enforcer behind an address instance");
    let text = format!("{err:#}");
    assert!(
        text.contains("enforcer.identity is address") && text.contains("says pubkey"),
        "names both: {text}"
    );

    let err = start_against(expect(PeerIdentity::Pubkey, "byte"), lan_hello())
        .await
        .expect_err("an address enforcer behind a pubkey instance");
    let text = format!("{err:#}");
    assert!(
        text.contains("enforcer.identity is pubkey") && text.contains("says address"),
        "names both: {text}"
    );

    // And one that agrees is no obstacle.
    start_against(expect(PeerIdentity::Pubkey, "byte"), fips_hello())
        .await
        .expect("the identities agree");
}

#[tokio::test]
async fn an_enforcer_counting_another_unit_refuses_to_start_naming_both() {
    // A tap controller counting litres, paired with an instance selling
    // millilitres.
    let mut litres = lan_hello();
    litres.unit = "l".into();
    let err = start_against(expect(PeerIdentity::Address, "ml"), litres)
        .await
        .expect_err("litres behind millilitres");
    let text = format!("{err:#}");
    assert!(
        text.contains("mint.unit is \"ml\"") && text.contains("\"l\""),
        "names both: {text}"
    );

    let mut millilitres = lan_hello();
    millilitres.unit = "ml".into();
    start_against(expect(PeerIdentity::Address, "ml"), millilitres)
        .await
        .expect("the units agree");
}

#[tokio::test]
async fn a_reconnecting_enforcer_may_change_neither_identity_nor_unit() {
    let stub = Stub::new();
    let (enforcer, conn) = connected(&stub, lan_hello()).await;
    let payer = key(5);
    enforcer.register(payer, lan(5));
    drop(conn);

    // The same socket, now built for pubkey: refused, and nothing is sold.
    let mut conn = stub.accept().await;
    conn.hello(fips_hello()).await;
    assert!(conn.closed_by_tollgated().await, "refused and closed");
    assert!(!enforcer.selling(payer));

    // Now counting in another unit: refused the same way.
    let mut conn = stub.accept().await;
    let mut kib = lan_hello();
    kib.unit = "kib".into();
    conn.hello(kib).await;
    assert!(conn.closed_by_tollgated().await, "refused and closed");
    assert!(!enforcer.selling(payer));

    // An enforcer that agrees with the instance is taken back.
    let mut conn = stub.accept().await;
    conn.hello(lan_hello()).await;
    conn.until("the full state", |m| is_set(m, payer, Some(0)))
        .await;
    assert!(enforcer.selling(payer));
}

#[tokio::test]
async fn another_enforcer_protocol_version_is_closed_and_tried_again() {
    let stub = Stub::new();
    let connecting = tokio::spawn(External::connect(
        stub.path.clone(),
        expect(PeerIdentity::Address, "byte"),
    ));
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
async fn delegated_bindings_are_refused_when_the_enforcer_refuses_them() {
    let stub = Stub::new();
    let (enforcer, mut conn) = connected(&stub, lan_hello()).await;
    let payer = key(6);
    enforcer.register(payer, lan(6));
    conn.until("the payer's set", |m| is_set(m, payer, Some(0)))
        .await;

    assert_eq!(
        enforcer.delegate(payer, Subject::address(lan(40))),
        Err(DelegateError::Refused)
    );
    // Turned down here, so nothing reaches the enforcer: the next thing it hears
    // is the rate below, not a bind.
    enforcer.set_shaping_rate(payer, 7);
    assert!(is_set(&conn.recv().await.unwrap(), payer, Some(7)));
}

#[tokio::test]
async fn a_delegated_binding_reaches_an_enforcer_that_accepts_them_flagged() {
    let stub = Stub::new();
    let mut hello = lan_hello();
    hello.delegated = true;
    let (enforcer, mut conn) = connected(&stub, hello).await;
    let payer = key(7);
    enforcer.register(payer, lan(7));
    conn.until("the payer's set", |m| is_set(m, payer, Some(0)))
        .await;

    assert_eq!(
        enforcer.delegate(key(8), Subject::address(lan(41))),
        Err(DelegateError::UnknownPayer)
    );
    // A delegated subject is whatever the client and the enforcer agreed on,
    // passed on untouched: here, the tap a customer stands at.
    let tap = Subject::new(*b"tap-3").unwrap();
    enforcer.delegate(payer, tap.clone()).expect("delegate");
    assert_eq!(
        conn.recv().await,
        Some(EnforcerMessage::Bind(enforcer::Bind {
            peer: payer,
            bindings: vec![
                Binding {
                    subject: Subject::address(lan(7)),
                    delegated: false,
                },
                Binding {
                    subject: tap,
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
    let (enforcer, mut conn) = connected(&stub, hello).await;
    let payer = key(12);
    enforcer.register(payer, lan(12));
    conn.until("the payer's set", |m| is_set(m, payer, Some(0)))
        .await;

    // Its own, and seven more.
    for i in 0..7 {
        enforcer
            .delegate(payer, Subject::new([i; 4]).unwrap())
            .expect("room left");
    }
    let EnforcerMessage::Bind(bind) = conn
        .until(
            "the last bind",
            |m| matches!(m, EnforcerMessage::Bind(b) if b.bindings.len() == enforcer::MAX_BINDINGS),
        )
        .await
    else {
        unreachable!()
    };
    assert_eq!(bind.bindings.iter().filter(|b| b.delegated).count(), 7);
    assert_eq!(
        enforcer.delegate(payer, Subject::new([99; 4]).unwrap()),
        Err(DelegateError::TooMany)
    );
}

#[tokio::test]
async fn a_conflict_stops_sales_to_that_payer_only() {
    let stub = Stub::new();
    let (enforcer, mut conn) = connected(&stub, lan_hello()).await;
    let (first, second) = (key(9), key(10));
    enforcer.register(first, lan(23));
    enforcer.register(second, lan(23));
    conn.until("the second payer's set", |m| is_set(m, second, Some(0)))
        .await;

    conn.send(EnforcerMessage::Conflict(Conflict {
        peer: second,
        subject: Subject::address(lan(23)),
    }))
    .await;
    let probe = enforcer.clone();
    wait_for("the conflict", move || !probe.selling(second)).await;
    assert!(enforcer.selling(first), "the first payer keeps the subject");
}

/// Whether `tollgated` closes the connection after the enforcer sends `bytes`,
/// framed, once `hello` is done — and stops selling until the next one.
async fn closes_on(frame: Vec<u8>) -> bool {
    let stub = Stub::new();
    let (enforcer, mut conn) = connected(&stub, lan_hello()).await;
    let payer = key(11);
    enforcer.register(payer, lan(11));
    conn.stream.write_all(&frame).await.expect("write");
    let closed = conn.closed_by_tollgated().await;
    if closed {
        let probe = enforcer.clone();
        wait_for("sales to stop", move || !probe.selling(payer)).await;
    }
    closed
}

fn framed(body: &[u8]) -> Vec<u8> {
    let mut out = (body.len() as u16).to_le_bytes().to_vec();
    out.extend_from_slice(body);
    out
}

fn framed_msg(msg: EnforcerMessage) -> Vec<u8> {
    let mut out = Vec::new();
    enforcer::encode_frame(&msg, &mut out).expect("encode");
    out
}

#[tokio::test]
async fn protocol_errors_close_the_connection() {
    // Not CBOR at all.
    assert!(closes_on(framed(&[0xff, 0x00, 0x13])).await);
    // A wire-protocol message on the enforcer socket.
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
        closes_on(framed_msg(EnforcerMessage::Set(enforcer::Set {
            peer: key(1),
            rate: Some(1),
        })))
        .await
    );
    // A second hello.
    assert!(closes_on(framed_msg(EnforcerMessage::Hello(lan_hello()))).await);
}

#[tokio::test]
async fn anything_before_hello_is_a_protocol_error() {
    let stub = Stub::new();
    let connecting = tokio::spawn(External::connect(
        stub.path.clone(),
        expect(PeerIdentity::Address, "byte"),
    ));
    let mut conn = stub.accept().await;
    conn.send(EnforcerMessage::Counters(Counters {
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
// A node behind the enforcer
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
            // A refusal names a ceiling of zero while the enforcer is down; test
            // again soon rather than after the default ten seconds.
            cap_hold_ms: 1_000,
            ..BuyerPolicy::default()
        },
        listen,
        peer_identity: PeerIdentity::Address,
        mint_url: "http://127.0.0.1/unused".into(),
        mint_local: "http://127.0.0.1/unused".into(),
        connector: None,
        channel_ttl_seconds: 3_600,
        peers,
    }
}

/// The provider behind the enforcer, and a payer buying from it.
struct Pair {
    provider: PubKey,
    published: tollgate_net::control::Published,
    payer: PubKey,
    payer_enforcer: Arc<Loopback>,
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
    enforcer: Arc<dyn Enforcer>,
) -> (std::net::SocketAddr, tollgate_net::control::Published) {
    let control = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .await
        .expect("bind");
    let listen = control.local_addr().expect("addr");
    let config = config(identity.clone(), listen, peers);
    let node = Node::new(&config, Arc::new(LocalChannels::new(identity)), enforcer);
    let published = node.published();
    tokio::spawn(async move {
        let _ = node.run_on(control, config, std::future::pending()).await;
    });
    (listen, published)
}

/// Start a provider behind `enforcer`, and a payer dialling it.
async fn pair(enforcer: External) -> Pair {
    let identity = Identity::generate();
    let provider = identity.pubkey();
    let (listen, published) = run_node(identity, vec![], Arc::new(enforcer)).await;

    // The payer: an ordinary node on the loopback enforcer. Its data plane is
    // not started; demand is told, not measured.
    let identity = Identity::generate();
    let payer = identity.pubkey();
    let payer_enforcer = Arc::new(Loopback::new());
    run_node(
        identity,
        vec![PeerConfig {
            pubkey: provider,
            endpoint: Some(listen.to_string()),
            policy: PeerPolicy::default(),
        }],
        payer_enforcer.clone(),
    )
    .await;

    Pair {
        provider,
        published,
        payer,
        payer_enforcer,
    }
}

/// The first rate the enforcer hears for `payer`, and everything up to it.
async fn first_set(conn: &mut Conn, payer: PubKey) -> (Vec<EnforcerMessage>, Option<u64>) {
    let mut before = Vec::new();
    loop {
        let msg = conn.recv().await.expect("the connection to stay up");
        if let EnforcerMessage::Set(s) = &msg
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
    let (enforcer, mut conn) = connected(&stub, lan_hello()).await;
    let p = pair(enforcer).await;

    // Bound before anything is sold, to the address it came from, and closed.
    let (before, rate) = first_set(&mut conn, p.payer).await;
    assert!(
        before.contains(&EnforcerMessage::Bind(enforcer::Bind {
            peer: p.payer,
            bindings: vec![Binding {
                subject: Subject::address(IpAddr::V4(Ipv4Addr::LOCALHOST)),
                delegated: false,
            }],
        })),
        "bound before its first set: {before:?}"
    );
    assert_eq!(rate, Some(0), "the enforcer starts closed");

    // It pays, and the enforcer opens it at the rate it bought: 125% of 2 MB/s.
    p.payer_enforcer.set_demand(p.provider, 2_000_000);
    conn.until("the payer to be opened at what it bought", |m| {
        is_set(m, p.payer, Some(2_500_000))
    })
    .await;

    // What the enforcer carried is what the grant is drawn down by.
    conn.send(EnforcerMessage::Counters(Counters {
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

    // It stops paying, and the enforcer closes it again.
    p.payer_enforcer.set_demand(p.provider, 0);
    conn.until("the payer to be closed when its grant lapses", |m| {
        is_set(m, p.payer, Some(0))
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn nothing_is_sold_while_the_enforcer_is_down() {
    let stub = Stub::new();
    let (enforcer, conn) = connected(&stub, lan_hello()).await;
    // The enforcer goes before the payer arrives.
    drop(conn);
    let probe = enforcer.clone();
    wait_for("the enforcer to notice", move || !probe.connected()).await;

    let p = pair(enforcer.clone()).await;
    p.payer_enforcer.set_demand(p.provider, 2_000_000);
    wait_for("the payer's session", || p.session().is_some()).await;
    tokio::time::sleep(Duration::from_secs(3)).await;
    let s = p.session().expect("the session is kept");
    assert_eq!(s.authorized, 0, "nothing bought while the enforcer is down");
    assert!(
        s.incoming_channels.is_empty(),
        "no new channel taken while the enforcer is down"
    );

    // The enforcer comes back: it is told everything, the channel is taken, and
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
    let probe = enforcer.clone();
    wait_for("the enforcer to notice", move || !probe.connected()).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let bought = p.session().expect("kept").authorized;
    tokio::time::sleep(Duration::from_secs(3)).await;
    let s = p.session().expect("the session is kept through an outage");
    assert_eq!(
        s.authorized, bought,
        "nothing bought while the enforcer is down"
    );

    // And picks up where it was once the enforcer is back.
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
