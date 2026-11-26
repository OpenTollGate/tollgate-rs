//! The bridge's admission against a real enforcer connection.
//!
//! `Loopback` proves the bridge calls the `Enforcer` trait correctly. It cannot
//! prove the calls survive encoding, framing and a second process — which is the
//! part that fails in the field. This test runs a real enforcer daemon on a real
//! unix socket, connects to it with the real `External` client, and asserts on what
//! arrived.
//!
//! It needs a `tokio` runtime because `External::connect` is async, and the daemon
//! runs on its own thread, so both halves are genuinely concurrent.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use tollgate_core::AccessLevel;
use tollgate_net::bridge::admit_fips::admit_peer;
use tollgate_net::bridge::{Credit, enforcer_daemon};
use tollgate_net::enforcer::Enforcer;
use tollgate_net::enforcer::{Expected, External};
use tollgate_protocol::PubKey;

/// A unique socket path per test, so tests can run concurrently.
fn socket_path(tag: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    std::env::temp_dir().join(format!("enforcer-{tag}-{}-{n}.sock", std::process::id()))
}

fn a_key(b: u8) -> PubKey {
    let mut bytes = [0u8; 33];
    bytes[0] = 0x02;
    bytes[1] = 0x11;
    bytes[2] = b;
    PubKey(bytes)
}

/// A runtime that keeps making progress on a second thread.
///
/// `External::connect` spawns the task that owns the socket. A current-thread
/// runtime only polls while `block_on` is running, so after `connect` returned the
/// sender would never run and every `set_shaping_rate` would sit in a channel
/// forever. Multi-thread keeps the writer live while the test uses the sync trait
/// the rest of the node uses.
fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("a runtime")
}

fn expected() -> Expected {
    Expected {
        identity: tollgate_protocol::enforcer::Identity::Address,
        unit: "byte".into(),
    }
}

/// A peer admitted through a real enforcer connection is bound, opened and shaped,
/// and the daemon on the other end saw exactly that.
#[test]
fn an_admission_crosses_a_real_enforcer_connection() {
    let path = socket_path("admit");
    let unit = "byte";
    let daemon = enforcer_daemon::serve(&path, unit).expect("start the enforcer daemon");
    let rt = runtime();

    let enforcer = rt
        .block_on(External::connect(&path, expected()))
        .expect("connect to the enforcer");
    // The daemon answers with a hello before anything else; without it the
    // connect would have failed, so reaching here already proves the handshake.

    let credit = Credit::new();
    credit
        .deposit("https://m", unit, 10_000)
        .expect("a buyer paid");
    let peer = a_key(0x01);
    let spent = admit_peer(&credit, &enforcer, "https://m", unit, peer, 250, 4).expect("admitted");
    assert_eq!(spent, 1_000);

    // The daemon records what arrived. Give it a moment: the socket write and the
    // read on the other thread are ordered but not synchronous.
    let recorded = Arc::clone(&daemon.recorded);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let mut seen_rate = None;
    while std::time::Instant::now() < deadline {
        if let Some(rate) = recorded
            .lock()
            .expect("not poisoned")
            .rates
            .get(peer.0.as_slice())
        {
            seen_rate = Some(*rate);
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert_eq!(
        seen_rate,
        Some(250),
        "the daemon never saw the shaping rate for the admitted peer"
    );

    let bindings = recorded.lock().expect("not poisoned").bindings.clone();
    assert!(
        !bindings.is_empty(),
        "the daemon was never told which subject to recognise"
    );
    // The subject is the address derived from the peer's key, sixteen bytes.
    assert_eq!(
        bindings[0].len(),
        16,
        "an address subject must arrive as the sixteen-byte form"
    );

    // Closing is visible on the far side: the peer is forgotten.
    enforcer.set_shaping_rate(peer, 0);
    enforcer.remove(peer);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let mut still_tracked = true;
    while std::time::Instant::now() < deadline {
        if !recorded
            .lock()
            .expect("not poisoned")
            .tracked
            .iter()
            .any(|k| k.as_slice() == peer.0.as_slice())
        {
            still_tracked = false;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(!still_tracked, "closing did not reach the daemon");
}

/// An unpaid peer is refused before anything reaches the enforcer, so the far side
/// never hears of it.
#[test]
fn an_unpaid_admission_sends_nothing() {
    let path = socket_path("unpaid");
    let daemon = enforcer_daemon::serve(&path, "byte").expect("start the enforcer daemon");
    let rt = runtime();
    let enforcer = rt
        .block_on(External::connect(&path, expected()))
        .expect("connect");

    let credit = Credit::new();
    credit.deposit("https://m", "byte", 10).expect("a little");
    let peer = a_key(0x02);
    let refused = admit_peer(&credit, &enforcer, "https://m", "byte", peer, 1_000, 4)
        .expect_err("1000 x 4 is more than 10");
    assert!(!refused.message().is_empty());

    // Nothing should have been bound for this peer.
    std::thread::sleep(std::time::Duration::from_millis(100));
    let recorded = daemon.recorded.lock().expect("not poisoned");
    assert!(
        !recorded.bindings.iter().any(|b| b.len() == 16),
        "an unpaid peer reached the enforcer"
    );
    assert!(
        !recorded.rates.contains_key(peer.0.as_slice()),
        "an unpaid peer was shaped"
    );
}

/// A rate only reaches the enforcer for a peer it already tracks.
///
/// That is `External`'s rule, not an accident of the transport: `set_shaping_rate`
/// returns early for a peer it has no entry for, because shaping something the node
/// never registered would be attributing traffic to a peer it cannot name. So this
/// test registers first — which is also what `apply` and `admit_peer` do, in that
/// order, for the same reason.
#[test]
fn access_and_rate_arrive_together() {
    let path = socket_path("access");
    let daemon = enforcer_daemon::serve(&path, "byte").expect("start the enforcer daemon");
    let rt = runtime();
    let enforcer = rt
        .block_on(External::connect(&path, expected()))
        .expect("connect");

    let peer = a_key(0x03);
    enforcer.register(
        peer,
        std::net::IpAddr::V4(std::net::Ipv4Addr::new(192, 0, 2, 3)),
    );
    enforcer.set_access(peer, AccessLevel::Active);
    enforcer.set_shaping_rate(peer, 77);

    let recorded = Arc::clone(&daemon.recorded);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let mut got = None;
    while std::time::Instant::now() < deadline {
        if let Some(rate) = recorded
            .lock()
            .expect("not poisoned")
            .rates
            .get(peer.0.as_slice())
        {
            got = Some(*rate);
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert_eq!(got, Some(77));
}
