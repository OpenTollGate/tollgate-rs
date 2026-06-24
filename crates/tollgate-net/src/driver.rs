//! Bridges transport/adapter/wallet ↔ the tollgate-core Session.
//!
//! Holds the core `Session` and a per-peer outbox. The server feeds inbound
//! frames in; the driver runs them through `Session::handle`, executes the
//! resulting `Action`s (set firewall access, verify tokens with the mint), and
//! queues any outbound wire messages in the outbox for the server to return on
//! the same exchange.

use std::collections::{BTreeMap, BTreeSet};
use std::net::IpAddr;
use std::sync::Arc;
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

use tollgate_net::status::{MintStatus, NodeStatus, PeerStatus, PricingStatus, ProductStatus};

use crate::adapter::IpAdapter;
use crate::config::Identity;
use crate::wallet::BootstrapWallet;

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
    /// Our pre-encoded PriceSheet, advertised after Announce on each exchange so
    /// peers can discover the price and accepted mints. Empty if we sell nothing.
    price_sheet: Vec<u8>,
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
            price_sheet,
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
    pub async fn peer_connected(&self, peer_hex: &str, ip: Option<IpAddr>) {
        if let Some(ip) = ip {
            self.0.peer_ip.lock().await.insert(peer_hex.to_string(), ip);
        }
        self.touch(peer_hex).await;
        let peer = parse_peer(peer_hex);
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
        let metered = self.0.metered.lock().await.clone();
        let now = now_millis().0;

        let peers = snaps
            .iter()
            .map(|s| {
                let hex = peer_to_hex(s.peer);
                PeerStatus {
                    ip: peer_ip.get(&hex).map(|ip| ip.to_string()),
                    idle_ms: last_seen.get(&hex).map_or(0, |t| now.saturating_sub(t.0)),
                    metered_secs: s
                        .metering_start
                        .map_or(0, |start| now.saturating_sub(start.0) / 1000),
                    metered: metered.contains(&hex),
                    allowed: matches!(s.phase, PeerPhase::Active),
                    phase: format!("{:?}", s.phase),
                    balance: s.balance,
                    delivered: s.delivered,
                    received: s.received,
                    pubkey: hex,
                }
            })
            .collect();

        NodeStatus {
            pubkey: self.0.identity.pubkey_hex(),
            unit: self.0.unit.clone(),
            peers,
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
                self.enqueue(&peer_to_hex(peer), bytes).await;
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
        assert_eq!(p.phase, "Active");
        assert_eq!(p.balance, 4242);
        assert_eq!(p.ip.as_deref(), Some("10.0.0.9"));
        assert!(p.allowed);
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
}
