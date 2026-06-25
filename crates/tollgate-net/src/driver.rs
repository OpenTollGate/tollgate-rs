//! Bridges transport/adapter/wallet ↔ the tollgate-core Session.
//!
//! Holds the core `Session` and a per-peer outbox. The server feeds inbound
//! frames in; the driver runs them through `Session::handle`, executes the
//! resulting `Action`s (set firewall access, verify tokens with the mint), and
//! queues any outbound wire messages in the outbox for the server to return on
//! the same exchange.

use std::collections::{BTreeMap, BTreeSet};
use std::net::IpAddr;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::sync::Mutex;

use tollgate_core::action::Action;
use tollgate_core::event::Event;
use tollgate_core::metering::Counters;
use tollgate_core::session::PeerPhase;
use tollgate_core::time::Millis;
use tollgate_core::{PeerId, Price, Session};
use tollgate_protocol::{
    Announce, MessageType, PROTOCOL_VERSION, PriceSheet, PublicKey, peek_type,
};

use tollgate_net::status::{
    MintStatus, NodeStatus, PeerStatus, Pricing, PricingStatus, ProductStatus,
};

use crate::adapter::IpAdapter;
use crate::client;
use crate::config::{Identity, UpstreamConfig};
use crate::wallet::BootstrapWallet;

/// What we know about an upstream peer we *buy* from (the mesh's inbound
/// direction). Updated by the per-upstream consume loop; read by `status()`.
#[derive(Clone)]
struct UpstreamState {
    /// The upstream's pubkey (from its Announce).
    pubkey: String,
    /// The upstream's URL (the configured `peer`).
    url: String,
    /// What they charge us.
    price: Price,
    /// Our remaining scaled balance with them, signed (`−` they owe us, e.g. when
    /// they deliver at a negative price and pay us).
    remaining_scaled: i64,
    /// Usage from *our* perspective: `sent` to them, `recv` from them.
    sent: u64,
    recv: u64,
    /// Seconds the session has run (from their report's `elapsed_ms`).
    metered_secs: u64,
    /// Buy-side metering drift: their claimed delivery vs what we measured receiving.
    drift: Option<f64>,
    /// Whether we're currently paid up / online with them.
    online: bool,
    /// When we last heard back from them.
    last_seen: Millis,
}

/// Shared handle — cheaply cloned (Arc) and given to each connection.
#[derive(Clone)]
pub struct Driver(Arc<Inner>);

struct Inner {
    session: Mutex<Session>,
    /// Per-peer queue of outbound message bodies (unframed), keyed by pubkey hex.
    outbox: Mutex<BTreeMap<String, Vec<Vec<u8>>>>,
    /// Maps a peer's pubkey hex to its source IP — the firewall enforces by IP.
    peer_ip: Mutex<BTreeMap<String, IpAddr>>,
    /// Last time (host wall-clock ms) each peer was heard from. The reaper drops
    /// peers idle past a timeout — the only "disconnect" signal the HTTP-polling
    /// transport has (there is no socket close to observe).
    last_seen: Mutex<BTreeMap<String, Millis>>,
    /// Peers currently being metered (Active). `StartMetering`/`StopMetering`
    /// maintain this; the metering loop only reads counters for these, so blocked
    /// and suspended peers aren't polled.
    metered: Mutex<BTreeSet<String>>,
    wallet: BootstrapWallet,
    adapter: IpAdapter,
    identity: Arc<Identity>,
    /// Resource unit this node meters ("bytes", "wh", …), sent in our Announce.
    unit: String,
    /// What we charge peers for our delivery (v1: one price for all).
    our_price: Price,
    /// Our pre-encoded PriceSheet, advertised after Announce on each exchange so
    /// peers can discover the price and accepted mints. Empty if we sell nothing.
    price_sheet: Vec<u8>,
    /// Upstream peers we buy from, keyed by URL — the inbound/mesh direction.
    /// A std mutex so the (sync) consume callback can update it; the critical
    /// section never spans an await.
    upstreams: StdMutex<BTreeMap<String, UpstreamState>>,
}

impl Driver {
    pub fn new(
        wallet: BootstrapWallet,
        adapter: IpAdapter,
        identity: Arc<Identity>,
        price: Price,
        unit: impl Into<String>,
        price_sheet: Vec<u8>,
    ) -> Self {
        let mut session = Session::new();
        session.set_price(price);
        Self(Arc::new(Inner {
            session: Mutex::new(session),
            outbox: Mutex::new(BTreeMap::new()),
            peer_ip: Mutex::new(BTreeMap::new()),
            last_seen: Mutex::new(BTreeMap::new()),
            metered: Mutex::new(BTreeSet::new()),
            wallet,
            adapter,
            identity,
            unit: unit.into(),
            our_price: price,
            price_sheet,
            upstreams: StdMutex::new(BTreeMap::new()),
        }))
    }

    /// This node's own Announce, encoded — sent to a peer so both sides learn
    /// each other's identity (mutual detection).
    fn our_announce(&self) -> Vec<u8> {
        let pubkey = PublicKey::from_bytes(self.0.identity.public_key.serialize());
        Announce::new(PROTOCOL_VERSION, pubkey, self.0.unit.clone(), 0).encode()
    }

    /// Spawn the metering loop: every `period`, read each known peer's byte
    /// counters and feed a MeterSample into the session (which charges the
    /// balance and suspends exhausted peers).
    pub fn spawn_metering(&self, period: Duration) {
        let driver = self.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(period);
            ticker.tick().await; // the first tick fires immediately; skip it
            loop {
                ticker.tick().await;
                // Only meter Active peers that have a known IP. Locks are released
                // before any counter read or `meter` await.
                let peers: Vec<(String, IpAddr)> = {
                    let metered = driver.0.metered.lock().await;
                    let peer_ip = driver.0.peer_ip.lock().await;
                    metered
                        .iter()
                        .filter_map(|hex| peer_ip.get(hex).map(|ip| (hex.clone(), *ip)))
                        .collect()
                };
                for (hex, ip) in peers {
                    let counters = driver.0.adapter.read_counters(ip);
                    driver.meter(&hex, counters).await;
                }
            }
        });
    }

    /// Spawn one consume loop per configured upstream: connect, pay, and
    /// auto-top-up, recording each as an upstream peer (the inbound/mesh
    /// direction) so it shows in status/tolltop. A loop that errors is restarted
    /// after a short delay.
    pub fn spawn_upstreams(&self, upstreams: Vec<UpstreamConfig>, identity: Arc<Identity>) {
        let unit = self.0.unit.clone();
        for cfg in upstreams {
            if cfg.peer.is_empty() {
                continue;
            }
            let driver = self.clone();
            let identity = identity.clone();
            let unit = unit.clone();
            tokio::spawn(async move {
                loop {
                    let opts = client::ConsumeOpts {
                        amount_sat: cfg.amount,
                        topup_sat: cfg.topup,
                        interval: Duration::from_secs(cfg.interval_secs.max(1)),
                        max_polls: None,
                        understate_received_pct: 0,
                        meter_iface: cfg.meter_iface.clone(),
                        meter_upstream: cfg.meter_upstream,
                    };
                    let d = driver.clone();
                    let url = cfg.peer.clone();
                    let result = client::run_consume(
                        &cfg.peer,
                        &identity,
                        &unit,
                        &cfg.mint,
                        opts,
                        move |ev| {
                            d.record_upstream(&url, ev);
                        },
                    )
                    .await;
                    if let Err(e) = result {
                        tracing::warn!(peer = %cfg.peer, err = %e, "upstream loop ended; retrying");
                    }
                    driver.mark_upstream_offline(&cfg.peer);
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
            });
        }
    }

    /// Record an observation from an upstream's consume loop (sync — std mutex).
    fn record_upstream(&self, url: &str, ev: &client::ConsumeEvent) {
        let mut map = self.0.upstreams.lock().unwrap_or_else(|e| e.into_inner());
        let state = map.entry(url.to_string()).or_insert_with(|| UpstreamState {
            pubkey: String::new(),
            url: url.to_string(),
            price: Price::default(),
            remaining_scaled: 0,
            sent: 0,
            recv: 0,
            metered_secs: 0,
            drift: None,
            online: true,
            last_seen: now_millis(),
        });
        state.pubkey = ev.peer_pubkey.clone();
        state.price = ev.price;
        state.remaining_scaled = ev.remaining_scaled;
        state.drift = ev.drift;
        state.online = !ev.cut_off;
        state.last_seen = now_millis();
        if let Some(r) = &ev.report {
            // Flip to our perspective: we received what they delivered to us, and
            // sent them what they received from us.
            state.recv = r.delivered;
            state.sent = r.received;
            state.metered_secs = r.elapsed_ms / 1000;
        }
    }

    /// Mark an upstream offline (its loop ended and is retrying).
    fn mark_upstream_offline(&self, url: &str) {
        if let Some(s) = self
            .0
            .upstreams
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_mut(url)
        {
            s.online = false;
        }
    }

    /// Spawn the reaper: every `sweep_every`, drop peers idle longer than
    /// `idle_after`. In the HTTP-polling transport a peer "disconnects" simply by
    /// no longer polling, so silence past a timeout is the disconnect signal.
    pub fn spawn_reaper(&self, idle_after: Duration, sweep_every: Duration) {
        let driver = self.clone();
        let idle_ms = idle_after.as_millis() as u64;
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(sweep_every);
            ticker.tick().await; // skip the immediate first tick
            loop {
                ticker.tick().await;
                driver.reap_idle(now_millis().0, idle_ms).await;
            }
        });
    }

    /// One reaping sweep at wall-clock `now_ms`: tear down every peer idle for at
    /// least `idle_ms` that is **not** `Active`. Active peers hold paid balance and
    /// are kept even while silent — a bootstrap client pays once, then consumes
    /// without polling again. Factored out of [`Self::spawn_reaper`] so tests can
    /// drive a sweep at a controlled time.
    async fn reap_idle(&self, now_ms: u64, idle_ms: u64) {
        let idle: Vec<String> = self
            .0
            .last_seen
            .lock()
            .await
            .iter()
            .filter(|(_, seen)| now_ms.saturating_sub(seen.0) >= idle_ms)
            .map(|(hex, _)| hex.clone())
            .collect();
        for hex in idle {
            let peer = parse_peer(&hex);
            let phase = self.0.session.lock().await.peer_phase(&peer);
            if matches!(phase, Some(PeerPhase::Active)) {
                continue;
            }
            tracing::info!(peer = %hex, ?phase, "reaping idle peer");
            self.peer_disconnected(&hex).await;
        }
    }

    /// Record that we just heard from a peer (resets its idle timer).
    async fn touch(&self, peer_hex: &str) {
        self.0
            .last_seen
            .lock()
            .await
            .insert(peer_hex.to_string(), now_millis());
    }

    /// Feed a meter reading for a peer into the session.
    async fn meter(&self, peer_hex: &str, counters: Counters) {
        let peer = parse_peer(peer_hex);
        let actions = self.handle(Event::MeterSample { peer, counters }).await;
        self.dispatch(actions, peer_hex).await;
    }

    /// A peer connected — its identity was established by Announce. `ip` is the
    /// source address the firewall will gate (absent in tests without a socket).
    /// Returns whether this was a *new* peer, as opposed to a keep-alive
    /// re-announce (the HTTP transport re-sends Announce every poll) — the caller
    /// logs the first at INFO and stays quiet after, so polls don't spam the log.
    pub async fn peer_connected(&self, peer_hex: &str, ip: Option<IpAddr>) -> bool {
        if let Some(ip) = ip {
            self.0.peer_ip.lock().await.insert(peer_hex.to_string(), ip);
        }
        self.touch(peer_hex).await;
        let peer = parse_peer(peer_hex);
        let is_new = self.0.session.lock().await.peer_phase(&peer).is_none();
        let actions = self.handle(Event::PeerConnected { peer }).await;
        self.dispatch(actions, peer_hex).await;
        // Announce ourselves back so the peer learns our identity (mutual
        // detection). Queued on the outbox; the transport returns it.
        self.enqueue(peer_hex, self.our_announce()).await;
        // Advertise our pricing so the peer can discover the price and accepted
        // mints (a bootstrap client sizes its token from this). Empty for a node
        // that sells nothing, e.g. a pure client.
        if !self.0.price_sheet.is_empty() {
            self.enqueue(peer_hex, self.0.price_sheet.clone()).await;
        }
        is_new
    }

    /// A peer disconnected — driven by the reaper on idle timeout (and, once the
    /// WebSocket path lands, by a close frame).
    pub async fn peer_disconnected(&self, peer_hex: &str) {
        let peer = parse_peer(peer_hex);
        let actions = self.handle(Event::PeerDisconnected { peer }).await;
        // Dispatch first (so the SetAccess(None) revoke can still resolve the IP),
        // then forget all per-peer state.
        self.dispatch(actions, peer_hex).await;
        self.0.peer_ip.lock().await.remove(peer_hex);
        self.0.last_seen.lock().await.remove(peer_hex);
        self.0.metered.lock().await.remove(peer_hex);
        // Drop any undelivered queue (e.g. a Reject from exhaustion the gone peer
        // never polled for) so nothing leaks into a future reconnect's session.
        self.0.outbox.lock().await.remove(peer_hex);
    }

    /// A decoded protocol frame arrived from a peer.
    pub async fn message_received(&self, peer_hex: &str, bytes: Vec<u8>) {
        self.touch(peer_hex).await;
        let peer = parse_peer(peer_hex);
        let actions = self.handle(Event::MessageReceived { peer, bytes }).await;
        self.dispatch(actions, peer_hex).await;
    }

    /// Remove and return all queued outbound message bodies for a peer.
    pub async fn drain_outbox(&self, peer_hex: &str) -> Vec<Vec<u8>> {
        self.0
            .outbox
            .lock()
            .await
            .remove(peer_hex)
            .unwrap_or_default()
    }

    /// Build a status snapshot of the node and its peers for the control socket.
    /// Each lock is cloned and released before the next is taken, so this never
    /// holds two at once — the metering loop locks `metered` then `peer_ip`, so
    /// holding both here in the other order could otherwise deadlock.
    pub async fn status(&self) -> NodeStatus {
        let snaps = self.0.session.lock().await.snapshot();
        let peer_ip = self.0.peer_ip.lock().await.clone();
        let last_seen = self.0.last_seen.lock().await.clone();
        let now = now_millis().0;
        let our_price = Pricing {
            price_per_second: self.0.our_price.per_second,
            price_per_unit: self.0.our_price.per_unit,
        };

        // A peering is bidirectional, so each peer is ONE entry keyed by pubkey.
        // The core session fills the outbound side (we deliver to them, they prepay
        // us); the upstream loops fill the inbound side (they deliver to us, we
        // prepay them). A peer that's both is merged.
        let mut by_pubkey: BTreeMap<String, PeerStatus> = BTreeMap::new();

        for s in &snaps {
            let hex = peer_to_hex(s.peer);
            by_pubkey.insert(
                hex.clone(),
                PeerStatus {
                    ip: peer_ip.get(&hex).map(|ip| ip.to_string()),
                    state: format!("{:?}", s.phase),
                    delivered: s.delivered,
                    received: s.received,
                    our_price: our_price.clone(),
                    their_price: Pricing::default(),
                    their_balance: s.balance,
                    our_balance: 0,
                    metered_secs: s
                        .metering_start
                        .map_or(0, |start| now.saturating_sub(start.0) / 1000),
                    idle_ms: last_seen.get(&hex).map_or(0, |t| now.saturating_sub(t.0)),
                    drift: s.drift,
                    pubkey: hex,
                },
            );
        }

        let upstreams = self
            .0
            .upstreams
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        for u in upstreams.values() {
            let up_idle = now.saturating_sub(u.last_seen.0);
            let entry = by_pubkey
                .entry(u.pubkey.clone())
                .or_insert_with(|| PeerStatus {
                    pubkey: u.pubkey.clone(),
                    ip: Some(u.url.clone()),
                    state: if u.online { "Active" } else { "Cut-off" }.to_string(),
                    delivered: 0,
                    received: 0,
                    our_price: our_price.clone(),
                    their_price: Pricing::default(),
                    their_balance: 0,
                    our_balance: 0,
                    metered_secs: 0,
                    idle_ms: up_idle,
                    drift: None,
                });
            // Fill the inbound (we-buy) side; usage is the same flow both sides see.
            entry.delivered = entry.delivered.max(u.sent);
            entry.received = entry.received.max(u.recv);
            entry.their_price = Pricing {
                price_per_second: u.price.per_second,
                price_per_unit: u.price.per_unit,
            };
            entry.our_balance = u.remaining_scaled;
            entry.metered_secs = entry.metered_secs.max(u.metered_secs);
            entry.idle_ms = entry.idle_ms.min(up_idle);
            // Surface buy-side drift when the sell side hasn't already set drift
            // (a buy-only peer); a bidirectional peer keeps its sell-side figure.
            entry.drift = entry.drift.or(u.drift);
        }

        NodeStatus {
            pubkey: self.0.identity.pubkey_hex(),
            unit: self.0.unit.clone(),
            peers: by_pubkey.into_values().collect(),
            pricing: decode_pricing(&self.0.price_sheet),
        }
    }

    /// Run one event through the core session, releasing the lock before any I/O.
    async fn handle(&self, event: Event) -> Vec<Action> {
        let now = now_millis();
        self.0.session.lock().await.handle(event, now)
    }

    async fn dispatch(&self, actions: Vec<Action>, peer_hex: &str) {
        for action in actions {
            match action {
                Action::VerifyBootstrapToken { peer, token } => {
                    // Verify with the mint inline (the session lock is not held
                    // here), then feed the result back into the session. The
                    // follow-up actions never contain another VerifyBootstrapToken,
                    // so there is no async recursion.
                    let (ok, amount) = self.verify_token(peer_hex, &token).await;
                    let follow = self
                        .handle(Event::BootstrapVerified { peer, amount, ok })
                        .await;
                    for action in follow {
                        self.execute(action).await;
                    }
                }
                other => self.execute(other).await,
            }
        }
    }

    /// Execute a non-I/O action. (Token verification is handled in `dispatch`.)
    async fn execute(&self, action: Action) {
        match action {
            Action::SetAccess { peer, level } => {
                let hex = peer_to_hex(peer);
                let ip = self.0.peer_ip.lock().await.get(&hex).copied();
                // Backend-independent access-decision log (the nftables backend
                // itself logs only at debug). This is the authoritative line a
                // test or operator greps for.
                tracing::info!(
                    peer = %hex,
                    ?level,
                    ip = ?ip,
                    allowed = level.allows_delivery(),
                    "access decision"
                );
                match ip {
                    Some(ip) if level.allows_delivery() => self.0.adapter.allow(ip),
                    Some(ip) => self.0.adapter.deny(ip),
                    None => {
                        tracing::debug!(peer = %hex, ?level, "no IP mapping; access not enforced")
                    }
                }
            }
            Action::Send { peer, bytes } => {
                let hex = peer_to_hex(peer);
                // A transit-loss Reject is the metering-drift warning. Log it so an
                // operator (or integration test) can watch drift escalate toward the
                // cut, distinct from a balance-exhaustion suspension.
                if matches!(peek_type(&bytes), Some(MessageType::Reject))
                    && tollgate_protocol::Reject::decode(&bytes)
                        .map(|r| r.is_transit_loss())
                        .unwrap_or(false)
                {
                    tracing::warn!(peer = %hex, "metering drift over tolerance (transit-loss warning)");
                }
                self.enqueue(&hex, bytes).await;
            }
            Action::StartMetering { peer } => {
                let hex = peer_to_hex(peer);
                tracing::debug!(peer = %hex, "start metering");
                self.0.metered.lock().await.insert(hex);
            }
            Action::StopMetering { peer } => {
                let hex = peer_to_hex(peer);
                tracing::debug!(peer = %hex, "stop metering");
                self.0.metered.lock().await.remove(&hex);
            }
            Action::Disconnect { peer } => {
                tracing::debug!(peer = %peer_to_hex(peer), "disconnect (stub)");
            }
            Action::VerifyBootstrapToken { .. } => {
                tracing::error!("VerifyBootstrapToken must be handled by dispatch");
            }
        }
    }

    /// Verify a bootstrap token with its mint. Returns `(accepted, milli_sat)`.
    async fn verify_token(&self, peer_hex: &str, token: &[u8]) -> (bool, u64) {
        let token_str = match std::str::from_utf8(token) {
            Ok(s) => s,
            Err(_) => {
                tracing::warn!(peer = peer_hex, "bootstrap token is not valid UTF-8");
                return (false, 0);
            }
        };
        match self.0.wallet.verify(token_str).await {
            Ok(amount) => {
                tracing::info!(
                    peer = peer_hex,
                    amount_milli_sat = amount,
                    "bootstrap token verified"
                );
                (true, amount)
            }
            Err(e) => {
                tracing::warn!(peer = peer_hex, err = %e, "bootstrap token rejected");
                (false, 0)
            }
        }
    }

    async fn enqueue(&self, peer_hex: &str, message: Vec<u8>) {
        let mut outbox = self.0.outbox.lock().await;
        let queue = outbox.entry(peer_hex.to_string()).or_default();
        // MeteringReports are cumulative and self-healing, so a peer only needs
        // the latest. Drop any pending report before queuing a fresh one, else a
        // peer that polls infrequently would accumulate a backlog (the metering
        // loop emits one every interval whether or not the peer is reading).
        if matches!(peek_type(&message), Some(MessageType::MeteringReport)) {
            queue.retain(|f| !matches!(peek_type(f), Some(MessageType::MeteringReport)));
        }
        queue.push(message);
    }
}

/// Build a `PeerId` from a hex-encoded compressed public key.
/// Falls back to a zeroed key on bad input — the server validates before calling.
fn parse_peer(peer_hex: &str) -> PeerId {
    let bytes = hex::decode(peer_hex).unwrap_or_default();
    let arr: [u8; 33] = bytes.try_into().unwrap_or([0u8; 33]);
    PeerId(PublicKey::from_bytes(arr))
}

fn peer_to_hex(peer: PeerId) -> String {
    hex::encode(peer.0.as_bytes())
}

fn now_millis() -> Millis {
    Millis(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64,
    )
}

/// Decode our advertised PriceSheet into the serde-friendly [`PricingStatus`].
/// Empty/undecodable bytes (e.g. a node that sells nothing) yield empty pricing.
fn decode_pricing(encoded: &[u8]) -> PricingStatus {
    let Ok(sheet) = PriceSheet::decode(encoded) else {
        return PricingStatus::default();
    };
    PricingStatus {
        products: sheet
            .products
            .iter()
            .map(|p| ProductStatus {
                product_id: hex::encode(p.product_id.as_slice()),
                pricing_scale: p.pricing_scale,
                mints: p
                    .mints
                    .iter()
                    .map(|m| MintStatus {
                        mint_url: m.mint_url.clone(),
                        mint_unit: m.mint_unit.clone(),
                        price_per_second: m.price_per_second,
                        price_per_unit: m.price_per_unit,
                    })
                    .collect(),
            })
            .collect(),
        min_interval_ms: sheet.interval_ms.0,
        max_interval_ms: sheet.interval_ms.1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::config::Config;

    fn test_driver() -> Driver {
        let identity = Arc::new(Identity::load_or_generate(&Config::default()).unwrap());
        Driver::new(
            BootstrapWallet::new(vec![]),
            IpAdapter::new(),
            identity,
            Price::default(),
            "bytes",
            Vec::new(), // no PriceSheet by default
        )
    }

    #[tokio::test]
    async fn send_action_is_queued_then_drained_once() {
        let driver = test_driver();
        let peer = PeerId(PublicKey::from_bytes([3u8; 33]));
        let hex = peer_to_hex(peer);

        driver
            .execute(Action::Send {
                peer,
                bytes: vec![1, 2, 3],
            })
            .await;

        assert_eq!(driver.drain_outbox(&hex).await, vec![vec![1u8, 2, 3]]);
        // A second drain yields nothing.
        assert!(driver.drain_outbox(&hex).await.is_empty());
    }

    #[tokio::test]
    async fn connect_queues_our_announce_for_mutual_detection() {
        let driver = test_driver();
        let peer = PeerId(PublicKey::from_bytes([4u8; 33]));
        let hex = peer_to_hex(peer);

        driver.peer_connected(&hex, None).await;

        // On connect we Announce ourselves back so the peer learns our identity.
        let queued = driver.drain_outbox(&hex).await;
        assert_eq!(queued.len(), 1);
        let announce = tollgate_protocol::Announce::decode(&queued[0]).expect("our Announce");
        assert_eq!(
            announce.public_key().as_bytes(),
            &driver.0.identity.public_key.serialize()
        );
    }

    #[tokio::test]
    async fn connect_advertises_our_price_sheet_after_announce() {
        use tollgate_protocol::{MessageType, MintPrice, PriceSheet, ProductOffer, peek_type};

        let prices = vec![MintPrice {
            mint_url: "http://mint".to_string(),
            price_per_second: 0,
            price_per_unit: 5,
            mint_unit: "sat".to_string(),
        }];
        let sheet =
            PriceSheet::new(vec![ProductOffer::new(1000, &prices, vec![])], 5000, 60000).encode();

        let identity = Arc::new(Identity::load_or_generate(&Config::default()).unwrap());
        let driver = Driver::new(
            BootstrapWallet::new(vec![]),
            IpAdapter::new(),
            identity,
            Price::default(),
            "bytes",
            sheet,
        );

        let peer = PeerId(PublicKey::from_bytes([8u8; 33]));
        let hex = peer_to_hex(peer);
        driver.peer_connected(&hex, None).await;

        // Announce first, then our PriceSheet.
        let queued = driver.drain_outbox(&hex).await;
        assert_eq!(
            queued.len(),
            2,
            "expected Announce + PriceSheet, got {queued:?}"
        );
        assert!(matches!(peek_type(&queued[0]), Some(MessageType::Announce)));
        let advertised = PriceSheet::decode(&queued[1]).expect("our PriceSheet");
        assert_eq!(advertised.products[0].mints[0].price_per_unit, 5);
    }

    #[tokio::test]
    async fn start_and_stop_metering_track_the_metered_set() {
        let driver = test_driver();
        let peer = PeerId(PublicKey::from_bytes([7u8; 33]));
        let hex = peer_to_hex(peer);

        driver.execute(Action::StartMetering { peer }).await;
        assert!(driver.0.metered.lock().await.contains(&hex));

        driver.execute(Action::StopMetering { peer }).await;
        assert!(!driver.0.metered.lock().await.contains(&hex));
    }

    #[tokio::test]
    async fn metering_reports_coalesce_keeping_only_the_latest() {
        use tollgate_protocol::{MessageType, MeteringReport, peek_type};

        let driver = test_driver();
        let peer = PeerId(PublicKey::from_bytes([12u8; 33]));
        let hex = peer_to_hex(peer);

        driver
            .execute(Action::Send {
                peer,
                bytes: MeteringReport::new(1000, 1, 0).encode(),
            })
            .await;
        driver.enqueue(&hex, vec![0xDE, 0xAD]).await; // a non-report frame is preserved
        driver
            .execute(Action::Send {
                peer,
                bytes: MeteringReport::new(2000, 2, 0).encode(),
            })
            .await;

        let drained = driver.drain_outbox(&hex).await;
        let reports: Vec<_> = drained
            .iter()
            .filter(|f| matches!(peek_type(f), Some(MessageType::MeteringReport)))
            .collect();
        assert_eq!(reports.len(), 1, "only the latest report is kept");
        assert_eq!(MeteringReport::decode(reports[0]).unwrap().elapsed_ms, 2000);
        assert!(
            drained.iter().any(|f| f == &vec![0xDE, 0xAD]),
            "non-report frames are preserved"
        );
    }

    #[tokio::test]
    async fn status_reports_active_peer_with_ip() {
        let driver = test_driver();
        let peer = PeerId(PublicKey::from_bytes([7u8; 33]));
        let hex = peer_to_hex(peer);
        driver
            .peer_connected(&hex, Some("10.0.0.9".parse().unwrap()))
            .await;
        let _ = driver
            .handle(Event::BootstrapVerified {
                peer,
                amount: 4242,
                ok: true,
            })
            .await;

        let st = driver.status().await;
        assert_eq!(st.peers.len(), 1);
        let p = &st.peers[0];
        assert_eq!(p.pubkey, hex);
        assert_eq!(p.state, "Active");
        assert_eq!(p.their_balance, 4242); // they prepaid us (outbound)
        assert_eq!(p.our_balance, 0);
        assert_eq!(p.ip.as_deref(), Some("10.0.0.9"));
        assert_eq!(p.net_balance(), 4242); // net earner
    }

    #[tokio::test]
    async fn status_includes_advertised_pricing() {
        use tollgate_protocol::{MintPrice, PriceSheet, ProductOffer};

        let prices = vec![MintPrice {
            mint_url: "http://mint".to_string(),
            price_per_second: 0,
            price_per_unit: 9,
            mint_unit: "sat".to_string(),
        }];
        let sheet =
            PriceSheet::new(vec![ProductOffer::new(1000, &prices, vec![])], 5000, 60000).encode();
        let identity = Arc::new(Identity::load_or_generate(&Config::default()).unwrap());
        let driver = Driver::new(
            BootstrapWallet::new(vec![]),
            IpAdapter::new(),
            identity,
            Price::default(),
            "bytes",
            sheet,
        );

        let pricing = driver.status().await.pricing;
        assert_eq!(pricing.products.len(), 1);
        assert_eq!(pricing.products[0].mints[0].price_per_unit, 9);
        assert_eq!(pricing.min_interval_ms, 5000);
        assert_eq!(pricing.max_interval_ms, 60000);
    }

    #[tokio::test]
    async fn status_surfaces_buy_side_drift_from_an_upstream() {
        let driver = test_driver();
        let peer = PeerId(PublicKey::from_bytes([15u8; 33]));
        let hex = peer_to_hex(peer);
        // An observation from a consume loop carrying buy-side drift — the provider's
        // claimed delivery diverged from what we independently measured receiving.
        let ev = client::ConsumeEvent {
            poll: 1,
            peer_pubkey: hex.clone(),
            price: Price::default(),
            paid_scaled: 10_000,
            remaining_scaled: 7_000,
            report: None,
            cut_off: false,
            topped_up: false,
            drift: Some(0.12),
        };
        driver.record_upstream("http://up:4747", &ev);

        let st = driver.status().await;
        let p = st.peers.iter().find(|p| p.pubkey == hex).expect("upstream peer");
        assert_eq!(p.drift, Some(0.12)); // buy-side drift reaches the status snapshot
        assert_eq!(p.our_balance, 7_000); // they hold our remaining prepayment
    }

    #[tokio::test]
    async fn reaper_drops_idle_unpaid_peer_but_keeps_active() {
        let driver = test_driver();

        // A peer that announced but never paid (stays New / blocked).
        let unpaid = PeerId(PublicKey::from_bytes([5u8; 33]));
        let unpaid_hex = peer_to_hex(unpaid);
        driver.peer_connected(&unpaid_hex, None).await;

        // A peer that paid and is Active.
        let active = PeerId(PublicKey::from_bytes([6u8; 33]));
        let active_hex = peer_to_hex(active);
        driver.peer_connected(&active_hex, None).await;
        let _ = driver
            .handle(Event::BootstrapVerified {
                peer: active,
                amount: 1_000,
                ok: true,
            })
            .await;
        assert_eq!(
            driver.0.session.lock().await.peer_phase(&active),
            Some(PeerPhase::Active)
        );

        // Sweep far in the future so both peers count as idle.
        driver.reap_idle(now_millis().0 + 1_000_000, 1_000).await;

        // The unpaid peer is gone; the Active peer survives despite being idle.
        assert_eq!(driver.0.session.lock().await.peer_phase(&unpaid), None);
        assert!(!driver.0.last_seen.lock().await.contains_key(&unpaid_hex));
        assert_eq!(
            driver.0.session.lock().await.peer_phase(&active),
            Some(PeerPhase::Active)
        );
        assert!(driver.0.last_seen.lock().await.contains_key(&active_hex));
    }

    /// Whether any queued frame is a transit-loss Reject (the drift warning).
    fn frames_have_transit_loss(frames: &[Vec<u8>]) -> bool {
        use tollgate_protocol::{MessageType, Reject, peek_type};
        frames.iter().any(|f| {
            matches!(peek_type(f), Some(MessageType::Reject))
                && Reject::decode(f)
                    .map(|r| r.is_transit_loss())
                    .unwrap_or(false)
        })
    }

    /// End-to-end through the driver: a peer that consistently under-reports what
    /// it received (a meter that lies or malfunctions) is warned each interval and
    /// cut off after the third, then reaped once idle — fully disconnected.
    #[tokio::test]
    async fn lying_peer_is_warned_then_suspended_after_three_drift_intervals() {
        use tollgate_protocol::MeteringReport;

        // Charge per unit so metering runs; default policy = 5% tolerance, cut at 3.
        let identity = Arc::new(Identity::load_or_generate(&Config::default()).unwrap());
        let driver = Driver::new(
            BootstrapWallet::new(vec![]),
            IpAdapter::new(),
            identity,
            Price {
                per_second: 0,
                per_unit: 1,
            },
            "bytes",
            Vec::new(),
        );

        let peer = PeerId(PublicKey::from_bytes([9u8; 33]));
        let hex = peer_to_hex(peer);
        driver
            .peer_connected(&hex, Some("10.0.0.9".parse().unwrap()))
            .await;
        // A large balance, so any suspension is from drift — never exhaustion.
        let _ = driver
            .handle(Event::BootstrapVerified {
                peer,
                amount: 1_000_000,
                ok: true,
            })
            .await;
        driver.drain_outbox(&hex).await; // clear announce / bootstrap-ack frames

        // Baseline reading: establishes the metering start, charges nothing.
        driver.meter(&hex, Counters::default()).await;

        // One interval: the peer claims it received `r`, then our meter shows we
        // delivered `d`; return whatever we queued back to the peer.
        async fn interval(driver: &Driver, hex: &str, r: u64, d: u64) -> Vec<Vec<u8>> {
            driver
                .message_received(hex, MeteringReport::new(0, 0, r).encode())
                .await;
            driver
                .meter(
                    hex,
                    Counters {
                        delivered: d,
                        received: 0,
                    },
                )
                .await;
            driver.drain_outbox(hex).await
        }
        async fn phase(driver: &Driver, peer: PeerId) -> Option<PeerPhase> {
            driver.0.session.lock().await.peer_phase(&peer)
        }

        // The peer under-reports by 20% every interval (well over the 5% tolerance).
        let a1 = interval(&driver, &hex, 80, 100).await;
        assert!(frames_have_transit_loss(&a1), "strike 1 sends a warning");
        assert_eq!(phase(&driver, peer).await, Some(PeerPhase::Active));

        let a2 = interval(&driver, &hex, 160, 200).await;
        assert!(frames_have_transit_loss(&a2), "strike 2 sends a warning");
        assert_eq!(phase(&driver, peer).await, Some(PeerPhase::Active));

        let a3 = interval(&driver, &hex, 240, 300).await;
        assert!(frames_have_transit_loss(&a3), "strike 3 sends a warning");
        // …and cuts the peer off — no longer Active.
        assert_eq!(phase(&driver, peer).await, Some(PeerPhase::Suspended));

        // The drift is visible in the status snapshot the control socket serves.
        let st = driver.status().await;
        let p = st.peers.iter().find(|p| p.pubkey == hex).expect("peer");
        assert_eq!(p.state, "Suspended");
        assert!(
            p.drift.expect("drift known") > 0.05,
            "drift {:?} exceeds tolerance",
            p.drift
        );

        // A suspended (non-Active) peer is reaped once idle — fully disconnected.
        driver.reap_idle(now_millis().0 + 1_000_000, 1_000).await;
        assert_eq!(phase(&driver, peer).await, None);
    }
}
