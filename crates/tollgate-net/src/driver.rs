//! Bridges transport/adapter/wallet ↔ the tollgate-core Session.
//!
//! Holds the core `Session` and a per-peer outbox. The server feeds inbound
//! frames in; the driver runs them through `Session::handle`, executes the
//! resulting `Action`s (set firewall access, verify tokens with the mint), and
//! queues any outbound wire messages in the outbox for the server to return on
//! the same exchange.

use std::collections::BTreeMap;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::sync::Mutex;

use tollgate_core::action::Action;
use tollgate_core::event::Event;
use tollgate_core::metering::Counters;
use tollgate_core::time::Millis;
use tollgate_core::{PeerId, Price, Session};
use tollgate_protocol::PublicKey;

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
    wallet: BootstrapWallet,
    adapter: IpAdapter,
    #[allow(dead_code)]
    identity: Arc<Identity>,
}

impl Driver {
    pub fn new(
        wallet: BootstrapWallet,
        adapter: IpAdapter,
        identity: Arc<Identity>,
        price: Price,
    ) -> Self {
        let mut session = Session::new();
        session.set_price(price);
        Self(Arc::new(Inner {
            session: Mutex::new(session),
            outbox: Mutex::new(BTreeMap::new()),
            peer_ip: Mutex::new(BTreeMap::new()),
            wallet,
            adapter,
            identity,
        }))
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
                let peers: Vec<(String, IpAddr)> = driver
                    .0
                    .peer_ip
                    .lock()
                    .await
                    .iter()
                    .map(|(hex, ip)| (hex.clone(), *ip))
                    .collect();
                for (hex, ip) in peers {
                    let counters = driver.0.adapter.read_counters(ip);
                    driver.meter(&hex, counters).await;
                }
            }
        });
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
        let peer = parse_peer(peer_hex);
        let actions = self.handle(Event::PeerConnected { peer }).await;
        self.dispatch(actions, peer_hex).await;
    }

    /// A peer disconnected.
    /// Not yet wired: HTTP polling detects this via poll timeout and the
    /// WebSocket path via close frame — both arrive in a later step.
    #[allow(dead_code)]
    pub async fn peer_disconnected(&self, peer_hex: &str) {
        let peer = parse_peer(peer_hex);
        let actions = self.handle(Event::PeerDisconnected { peer }).await;
        // Dispatch first (so the SetAccess(None) revoke can still resolve the IP),
        // then forget the mapping.
        self.dispatch(actions, peer_hex).await;
        self.0.peer_ip.lock().await.remove(peer_hex);
    }

    /// A decoded protocol frame arrived from a peer.
    pub async fn message_received(&self, peer_hex: &str, bytes: Vec<u8>) {
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
                match self.0.peer_ip.lock().await.get(&hex).copied() {
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
                tracing::debug!(peer = %peer_to_hex(peer), "start metering (stub)");
            }
            Action::StopMetering { peer } => {
                tracing::debug!(peer = %peer_to_hex(peer), "stop metering");
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
        self.0
            .outbox
            .lock()
            .await
            .entry(peer_hex.to_string())
            .or_default()
            .push(message);
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
    async fn bare_connect_queues_no_wire_message() {
        let driver = test_driver();
        let peer = PeerId(PublicKey::from_bytes([4u8; 33]));
        let hex = peer_to_hex(peer);

        driver.peer_connected(&hex, None).await;

        // PeerConnected only sets access (None); nothing to send on the wire.
        assert!(driver.drain_outbox(&hex).await.is_empty());
    }
}
