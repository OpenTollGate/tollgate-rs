//! Two real nodes, two real sockets, real bytes.
//!
//! The in-memory harness in `tollgate-core` proves the state machine is right.
//! This proves the whole thing works when the clock, the sockets and the signer
//! are real: traffic appears, the buyer notices, it signs a TopUp, the provider
//! admits it, the shaper opens up, and the throughput the operator sees follows.
//!
//! Every node binds ports the OS picked, so these tests run in parallel with
//! each other and with another `cargo test` on the same machine.

use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::net::TcpListener;
use tollgate_core::buyer::BuyerPolicy;
use tollgate_core::config::{GrantPolicy, NodePolicy, PeerPolicy};
use tollgate_net::adapter::{Loopback, ResourceAdapter};
use tollgate_net::channel::{ChannelBackend, FundedChannel, LocalChannels, VerifiedChannel};
use tollgate_net::identity::Identity;
use tollgate_net::node::{Node, NodeConfig, PeerConfig};
use tollgate_net::settle::Backoff;
use tollgate_net::wire::Identify;
use tollgate_protocol::{ChannelId, PubKey, Signature};

/// How long to wait for something the protocol does on its own: a peering, a
/// purchase, a grant lapsing.
///
/// Each normally takes well under a second. The deadline only decides how long
/// a genuine failure takes to report, so it is sized for a machine running
/// several test suites at once rather than for an idle one.
const PATIENCE: Duration = Duration::from_secs(60);

fn node_policy(mint: &str) -> NodePolicy {
    NodePolicy {
        unit: "byte".into(),
        accepted_mints: vec![mint.into()],
        received_multiplier: 0,
        minimum_flow: 4_096,
        grants: GrantPolicy {
            min_window_ms: 200,
            max_window_ms: 30_000,
            max_rate: None,
        },
        initial_channel_capacity: 10_000_000_000,
        stale_timeout_ms: 0,
        rollover_threshold_pct: 80,
    }
}

fn buyer_policy() -> BuyerPolicy {
    BuyerPolicy {
        // A short window keeps the demo responsive: the forfeit on raising the
        // rate can never exceed one window's worth.
        window_ms: 1_000,
        renew_lead_ms: 300,
        ..BuyerPolicy::default()
    }
}

/// Bind a node's two planes on free ports, and hold them.
///
/// A peer finds the data plane one port above the control plane, so the two
/// have to be adjacent. Ask the OS for any free control port, then try the one
/// above it; if that is taken, let both go and ask again. Holding the listeners
/// rather than handing out numbers means nothing else — another test here, or
/// another `cargo test` on the same machine — can take a port between choosing
/// it and using it.
async fn bind_planes() -> (TcpListener, TcpListener) {
    for _ in 0..100 {
        let control = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("bind a control plane on any free port");
        let addr = control.local_addr().expect("a bound address");
        let Some(data_port) = addr.port().checked_add(1) else {
            continue;
        };
        if let Ok(data) = TcpListener::bind((addr.ip(), data_port)).await {
            return (control, data);
        }
    }
    panic!("no free pair of adjacent ports on loopback after 100 tries");
}

/// A node a test started.
struct Spawned {
    pubkey: PubKey,
    adapter: Arc<Loopback>,
    /// What the node publishes, which is where a session either appears or
    /// does not.
    published: tollgate_net::control::Published,
    /// Where a peer dials it.
    endpoint: String,
}

/// Start a node on free ports.
async fn spawn_node(mint: &str, peers: Vec<PeerConfig>) -> Spawned {
    spawn_node_as(Identity::generate(), node_policy(mint), peers).await
}

/// The same, for a test that has to know a node's key before it starts — a
/// policy written about a peer names it, so somebody has to be first — or that
/// sells on terms of its own.
async fn spawn_node_as(identity: Identity, policy: NodePolicy, peers: Vec<PeerConfig>) -> Spawned {
    let pubkey = identity.pubkey();
    let (control, data) = bind_planes().await;
    let listen = control.local_addr().expect("a bound address");

    let config = NodeConfig {
        identity,
        policy,
        buyer: buyer_policy(),
        listen,
        // These tests talk plain IP on loopback, where an address commits to
        // nothing there is to check.
        identify: Identify::Claimed,
        // Unused here: these tests drive `LocalChannels`, so no mint is served
        // and nothing binds this.
        mint_listen: SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
        mint_url: "http://127.0.0.1/unused".into(),
        peers,
    };

    let adapter = Arc::new(Loopback::new());
    let node = Node::new(
        &config,
        Arc::new(LocalChannels::new(config.identity.clone())),
        adapter.clone(),
    );
    let published = node.published();
    spawn_loopback_plane(&config, data, adapter.clone());
    tokio::spawn(async move {
        if let Err(e) = node.run_on(control, config, std::future::pending()).await {
            eprintln!("node stopped: {e}");
        }
    });

    Spawned {
        pubkey,
        adapter,
        published,
        endpoint: listen.to_string(),
    }
}

/// Start the loopback data plane for a node: a listener, and a dialer per peer.
///
/// The node does not do this itself, because a kernel adapter forwards real
/// traffic and has no socket of its own.
fn spawn_loopback_plane(config: &NodeConfig, data: TcpListener, adapter: Arc<Loopback>) {
    let local = config.identity.pubkey();
    tokio::spawn(tollgate_net::dataplane::listen(data, adapter.clone()));
    for peer in &config.peers {
        let Some(endpoint) = peer.endpoint.clone() else {
            continue;
        };
        tollgate_net::dataplane::keep_dialing(endpoint, local, peer.pubkey, adapter.clone());
    }
}

/// A peer entry for a node to dial.
fn dial(peer: &Spawned) -> PeerConfig {
    PeerConfig {
        pubkey: peer.pubkey,
        endpoint: Some(peer.endpoint.clone()),
        policy: PeerPolicy::default(),
    }
}

/// Poll until `check` passes, or give up after [`PATIENCE`]. Real sockets and
/// a real clock mean nothing is instant; a deadline beats a fixed sleep.
async fn wait_for(label: &str, mut check: impl FnMut() -> bool) {
    let deadline = Instant::now() + PATIENCE;
    while Instant::now() < deadline {
        if check() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("timed out waiting for {label}");
}

/// Bytes per second actually arriving from a peer, measured over `window`.
///
/// The divisor is the time that really passed, not the time asked for: a
/// sleep on a loaded machine overruns, and dividing by the nominal window
/// would credit the overrun's bytes to a shorter interval.
async fn measure_download(adapter: &Loopback, peer: PubKey, window: Duration) -> u64 {
    let before = adapter.counters(peer).received;
    let started = Instant::now();
    tokio::time::sleep(window).await;
    let after = adapter.counters(peer).received;
    let elapsed_ms = started.elapsed().as_millis().max(1) as u64;
    (after - before) * 1_000 / elapsed_ms
}

/// Bring up a provider and a client, connected and paying.
async fn connected_pair() -> (PubKey, Arc<Loopback>, PubKey, Arc<Loopback>) {
    let provider = spawn_node("https://provider.example/mint", vec![]).await;

    // The client dials, so it is the side that has to know the provider's key
    // up front. A listener learns who is calling from the Announce.
    let client = spawn_node("https://client.example/mint", vec![dial(&provider)]).await;

    let probe = Arc::clone(&provider.adapter);
    let client_key = client.pubkey;
    wait_for("the provider to see the client", move || {
        probe.peers().contains(&client_key)
    })
    .await;

    (
        provider.pubkey,
        provider.adapter,
        client.pubkey,
        client.adapter,
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_client_that_wants_bandwidth_buys_it_and_gets_it() {
    let (provider, provider_adapter, client, client_adapter) = connected_pair().await;

    // Nothing bought yet: the client is held at the minimum flow allowance,
    // which is what leaves it able to send the TopUp that changes that.
    assert!(
        provider_adapter.shaping_rate(client) <= 4_096,
        "an unpaid peer should be at the allowance, not above it"
    );

    // Traffic appears.
    const WANTED: u64 = 2_000_000;
    client_adapter.set_demand(provider, WANTED);

    // 125% headroom on 2 MB/s.
    let probe = Arc::clone(&provider_adapter);
    wait_for(
        "the provider to shape the client to what it bought",
        move || probe.shaping_rate(client) == 2_500_000,
    )
    .await;

    let throughput = measure_download(&client_adapter, provider, Duration::from_secs(2)).await;
    assert!(
        throughput > WANTED,
        "the client bought 2.5 MB/s and measured only {throughput} B/s"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_traffic_spike_raises_the_purchased_rate() {
    // The point of the whole exercise: the algorithm notices demand climbing
    // and buys more, without anything being negotiated or acknowledged.
    let (provider, provider_adapter, client, client_adapter) = connected_pair().await;

    client_adapter.set_demand(provider, 1_000_000);
    let probe = Arc::clone(&provider_adapter);
    wait_for("the first purchase", move || {
        probe.shaping_rate(client) == 1_250_000
    })
    .await;

    let before = measure_download(&client_adapter, provider, Duration::from_secs(2)).await;

    // Demand jumps sixteenfold.
    client_adapter.set_demand(provider, 16_000_000);
    let probe = Arc::clone(&provider_adapter);
    wait_for("the rate to be raised", move || {
        probe.shaping_rate(client) == 20_000_000
    })
    .await;

    let after = measure_download(&client_adapter, provider, Duration::from_secs(2)).await;
    assert!(
        after > before * 4,
        "throughput should have followed the purchase: {before} B/s -> {after} B/s"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_client_that_stops_buying_falls_back_to_the_allowance() {
    // Non-payment enforces itself. Nothing detects it, no message announces it,
    // and there is no delivered-but-unpaid balance to chase.
    let (provider, provider_adapter, client, client_adapter) = connected_pair().await;

    client_adapter.set_demand(provider, 4_000_000);
    let probe = Arc::clone(&provider_adapter);
    wait_for("the purchase", move || {
        probe.shaping_rate(client) == 5_000_000
    })
    .await;

    client_adapter.set_demand(provider, 0);

    let probe = Arc::clone(&provider_adapter);
    wait_for("the grant to lapse back to the allowance", move || {
        probe.shaping_rate(client) == 4_096
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn both_directions_are_bought_and_shaped_independently() {
    // Two channels is the default. Each side pays for what it received, so both
    // owe, both fund, and each buys on its own schedule.
    let (provider, provider_adapter, client, client_adapter) = connected_pair().await;

    client_adapter.set_demand(provider, 1_000_000);
    provider_adapter.set_demand(client, 4_000_000);

    let probe = Arc::clone(&provider_adapter);
    wait_for("the client's purchase", move || {
        probe.shaping_rate(client) == 1_250_000
    })
    .await;

    let probe = Arc::clone(&client_adapter);
    wait_for("the provider's purchase", move || {
        probe.shaping_rate(provider) == 5_000_000
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_channel_that_fills_up_rolls_over_and_buying_continues() {
    // A channel sized to be exhausted in a few seconds, so the boundary is
    // crossed several times inside the test. Before per-channel cumulative,
    // this looped funding a channel per tick and then stopped buying forever.
    //
    // The funder sizes a channel, so it is the client — the side paying —
    // whose channels have to be small. The provider gets the same policy only
    // so that neither side is left at the 10 GB default.
    const CAPACITY: u64 = 5_000_000;
    let small_channels = |mint: &str| NodePolicy {
        // ~2 seconds of traffic at the rate the client will buy.
        initial_channel_capacity: CAPACITY,
        ..node_policy(mint)
    };

    let provider = spawn_node_as(
        Identity::generate(),
        small_channels("https://provider.example/mint"),
        vec![],
    )
    .await;
    let client = spawn_node_as(
        Identity::generate(),
        small_channels("https://client.example/mint"),
        vec![dial(&provider)],
    )
    .await;
    let provider_hex = hex::encode(provider.pubkey.0);
    let client_published = client.published;
    let (provider, provider_adapter) = (provider.pubkey, provider.adapter);
    let (client, client_adapter) = (client.pubkey, client.adapter);

    // Every channel the client pays the provider on, in the order it used
    // them, as its own published snapshot shows them.
    let paid_on = Arc::new(std::sync::Mutex::new(Vec::<(String, u64)>::new()));
    let note_channel = {
        let paid_on = Arc::clone(&paid_on);
        move || {
            let snapshot = client_published.load();
            let Some(channel) = snapshot
                .peers
                .iter()
                .find(|p| p.pubkey == provider_hex)
                .and_then(|p| p.outgoing_channel.as_ref())
            else {
                return;
            };
            let mut paid_on = paid_on.lock().expect("not poisoned");
            if paid_on.last().map(|(id, _)| id) != Some(&channel.id) {
                paid_on.push((channel.id.clone(), channel.capacity));
            }
        }
    };

    let probe = Arc::clone(&provider_adapter);
    wait_for("the peering", move || probe.peers().contains(&client)).await;

    client_adapter.set_demand(provider, 2_000_000);
    let probe = Arc::clone(&provider_adapter);
    wait_for("the first purchase", move || {
        probe.shaping_rate(client) == 2_500_000
    })
    .await;

    // Run through several channels' worth of capacity. Waiting for the bytes
    // rather than for a fixed time means a slow machine still crosses every
    // boundary before the checks below, instead of measuring a window too
    // short to reach one. A client that stopped buying after a rollover is
    // held at the 4 KB/s allowance and would need hours to get here.
    let start = client_adapter.counters(provider).received;
    let started = Instant::now();
    let probe = Arc::clone(&client_adapter);
    let note = note_channel;
    wait_for("three channels' worth of traffic", move || {
        note();
        probe.counters(provider).received - start >= 3 * CAPACITY
    })
    .await;
    let sustained = 3 * CAPACITY * 1_000 / started.elapsed().as_millis().max(1) as u64;
    assert!(
        sustained > 1_500_000,
        "throughput collapsed across the rollovers: {sustained} B/s"
    );

    // The boundaries were really crossed. Every byte is prepaid, so 15 MB
    // received needs 15 MB signed, which no fewer than three 5 MB channels can
    // hold; each stays active for about two seconds, far longer than a poll.
    let paid_on = paid_on.lock().expect("not poisoned").clone();
    assert!(
        paid_on.iter().all(|&(_, capacity)| capacity == CAPACITY),
        "the client funded channels of another size: {paid_on:?}"
    );
    assert!(
        paid_on.len() >= 3,
        "the client should have rolled over onto a third channel: {paid_on:?}"
    );

    // And it is still shaping at what was bought, not at the allowance. A
    // renewal can land a tick late on a loaded machine and let one grant lapse
    // for a moment, so this waits for the bought rate rather than sampling a
    // single instant; a client that could no longer buy never gets back to it.
    let probe = Arc::clone(&provider_adapter);
    wait_for("the client to still be shaped at what it buys", move || {
        probe.shaping_rate(client) == 2_500_000
    })
    .await;
}

/// A peer the operator refused is refused when it calls in.
///
/// The refusal has to survive the fact that there is nothing to dial: a node
/// does not reach out to a peer it will not talk to, so the entry carrying
/// `blocked` is exactly the entry with no endpoint. Losing such entries for
/// want of an address would admit precisely the peer being turned away.
#[tokio::test]
async fn a_blocked_peer_that_dials_in_is_refused() {
    // Somebody has to be named first, and it is the side being refused.
    let client_identity = Identity::generate();
    let client = client_identity.pubkey();

    let provider = spawn_node_as(
        Identity::generate(),
        node_policy("https://provider.example/mint"),
        vec![PeerConfig {
            pubkey: client,
            endpoint: None,
            policy: PeerPolicy {
                blocked: true,
                ..PeerPolicy::default()
            },
        }],
    )
    .await;

    let _client = spawn_node_as(
        client_identity,
        node_policy("https://client.example/mint"),
        vec![dial(&provider)],
    )
    .await;

    // The client redials every couple of seconds, so this covers several
    // attempts rather than catching the gap between two of them.
    tokio::time::sleep(Duration::from_secs(6)).await;

    let peers = &provider.published.load().peers;
    assert!(
        peers.is_empty(),
        "the blocked peer opened a session anyway: {peers:?}"
    );
}

/// A backend whose first few attempts to settle each channel fail, as they
/// would against a mint that is briefly unreachable. Everything else is
/// `LocalChannels`.
///
/// Failing per channel rather than overall means a node that never retries
/// never settles anything: a later channel's first attempt cannot stand in for
/// an earlier one's retry.
#[derive(Debug)]
struct FlakySettle {
    inner: LocalChannels,
    failures: u32,
    attempts: Mutex<HashMap<ChannelId, u32>>,
    settled: AtomicU32,
}

impl ChannelBackend for FlakySettle {
    fn fund(&self, peer: PubKey, mint_url: &str, capacity: u64) -> anyhow::Result<FundedChannel> {
        self.inner.fund(peer, mint_url, capacity)
    }

    fn verify(&self, peer: PubKey, funding: &[u8]) -> anyhow::Result<VerifiedChannel> {
        self.inner.verify(peer, funding)
    }

    fn sign_update(&self, channel_id: ChannelId, cumulative: u64) -> anyhow::Result<Signature> {
        self.inner.sign_update(channel_id, cumulative)
    }

    fn verify_update(
        &self,
        peer: PubKey,
        channel_id: ChannelId,
        cumulative: u64,
        signature: Signature,
    ) -> bool {
        self.inner
            .verify_update(peer, channel_id, cumulative, signature)
    }

    fn record_update(
        &self,
        peer: PubKey,
        channel_id: ChannelId,
        cumulative: u64,
        signature: Signature,
    ) -> anyhow::Result<()> {
        self.inner
            .record_update(peer, channel_id, cumulative, signature)
    }

    fn settle(&self, channel_id: ChannelId) -> anyhow::Result<()> {
        let attempt = {
            let mut attempts = self.attempts.lock().expect("not poisoned");
            let n = attempts.entry(channel_id).or_default();
            *n += 1;
            *n
        };
        if attempt <= self.failures {
            anyhow::bail!("mint unreachable (attempt {attempt})");
        }
        self.inner.settle(channel_id)?;
        self.settled.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

/// Start a node whose first `failures` settlements fail, retrying on a
/// schedule short enough for a test.
async fn spawn_flaky_node(
    policy: NodePolicy,
    peers: Vec<PeerConfig>,
    failures: u32,
) -> (Spawned, Arc<FlakySettle>) {
    let identity = Identity::generate();
    let pubkey = identity.pubkey();
    let (control, data) = bind_planes().await;
    let listen = control.local_addr().expect("a bound address");
    let config = NodeConfig {
        identity,
        policy,
        buyer: buyer_policy(),
        listen,
        identify: Identify::Claimed,
        // Unused here: these tests drive `LocalChannels`, so no mint is served
        // and nothing binds this.
        mint_listen: SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
        mint_url: "http://127.0.0.1/unused".into(),
        peers,
    };
    let backend = Arc::new(FlakySettle {
        inner: LocalChannels::new(config.identity.clone()),
        failures,
        attempts: Mutex::default(),
        settled: AtomicU32::new(0),
    });
    let adapter = Arc::new(Loopback::new());
    let node = Node::new(&config, backend.clone(), adapter.clone()).with_settle_backoff(Backoff {
        initial: Duration::from_millis(10),
        max: Duration::from_millis(100),
    });
    let published = node.published();
    spawn_loopback_plane(&config, data, adapter.clone());
    tokio::spawn(async move {
        if let Err(e) = node.run_on(control, config, std::future::pending()).await {
            eprintln!("node stopped: {e}");
        }
    });
    let spawned = Spawned {
        pubkey,
        adapter,
        published,
        endpoint: listen.to_string(),
    };
    (spawned, backend)
}

/// A settlement that fails is tried again until it goes through.
///
/// Core lets go of a channel the moment it asks for it to be settled, so
/// before retries a mint that blinked at the wrong moment lost the channel for
/// good: after its refund timelock the funder could take all of it back.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_settlement_that_fails_is_retried_until_it_succeeds() {
    // Channels small enough to drain, and be settled, within seconds.
    let small_channels = |mint: &str| NodePolicy {
        initial_channel_capacity: 5_000_000,
        ..node_policy(mint)
    };
    let (provider_node, provider_backend) =
        spawn_flaky_node(small_channels("https://provider.example/mint"), vec![], 3).await;
    let (client_node, client_backend) = spawn_flaky_node(
        small_channels("https://client.example/mint"),
        vec![dial(&provider_node)],
        3,
    )
    .await;
    let (provider, client) = (provider_node.pubkey, client_node.pubkey);
    let client_adapter = Arc::clone(&client_node.adapter);

    let probe = Arc::clone(&provider_node.adapter);
    wait_for("the peering", move || probe.peers().contains(&client)).await;

    client_adapter.set_demand(provider, 2_000_000);

    // Both ends settle a drained channel: the provider to claim what it
    // earned, the client to retire it. Each has three failures to get past.
    for (side, backend) in [("provider", provider_backend), ("client", client_backend)] {
        let probe = Arc::clone(&backend);
        wait_for(
            &format!("the {side} to settle a drained channel despite the failures"),
            move || probe.settled.load(Ordering::SeqCst) >= 1,
        )
        .await;
        let attempts = backend.attempts.lock().expect("not poisoned");
        assert!(
            attempts.values().any(|&n| n == backend.failures + 1),
            "the {side} should have settled on the attempt after its failures: {attempts:?}"
        );
    }
}
