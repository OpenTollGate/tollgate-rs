//! Bridges transport/adapter/wallet ↔ the tollgate-core Session.
//!
//! The driver holds the core `Session` behind an async Mutex and exposes a
//! `handle` method the server calls per incoming frame. It translates
//! network bytes into `Event`s, calls `Session::handle`, and executes the
//! returned `Action`s — calling the wallet or adapter as needed.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use tokio::sync::Mutex;

use tollgate_core::action::Action;
use tollgate_core::event::Event;
use tollgate_core::time::Millis;
use tollgate_core::{PeerId, Session};
use tollgate_protocol::PublicKey;

use crate::adapter::IpAdapter;
use crate::config::Identity;
use crate::wallet::BootstrapWallet;

/// Shared handle — cheaply cloned and given to each connection task.
#[derive(Clone)]
pub struct Driver(Arc<Inner>);

struct Inner {
    session: Mutex<Session>,
    wallet: BootstrapWallet,
    adapter: IpAdapter,
    #[allow(dead_code)]
    identity: Arc<Identity>,
}

impl Driver {
    pub fn new(wallet: BootstrapWallet, adapter: IpAdapter, identity: Arc<Identity>) -> Self {
        Self(Arc::new(Inner {
            session: Mutex::new(Session::new()),
            wallet,
            adapter,
            identity,
        }))
    }

    /// Called by the server when a peer connects (transport-layer auth done).
    pub async fn peer_connected(&self, peer_hex: &str) {
        let peer = parse_peer(peer_hex);
        let now = now_millis();
        let actions = self
            .0
            .session
            .lock()
            .await
            .handle(Event::PeerConnected { peer }, now);
        self.dispatch(actions, peer_hex).await;
    }

    /// Called by the server when a peer disconnects.
    /// Not yet wired: HTTP polling detects disconnect via poll timeout and the
    /// WebSocket path via close frame — both arrive in a later step.
    #[allow(dead_code)]
    pub async fn peer_disconnected(&self, peer_hex: &str) {
        let peer = parse_peer(peer_hex);
        let now = now_millis();
        let actions = self
            .0
            .session
            .lock()
            .await
            .handle(Event::PeerDisconnected { peer }, now);
        self.dispatch(actions, peer_hex).await;
    }

    /// Called by the server with each decoded frame from a peer.
    /// Returns encoded response frames to send back (may be empty).
    pub async fn message_received(&self, peer_hex: &str, bytes: Vec<u8>) -> Vec<u8> {
        let peer = parse_peer(peer_hex);
        let now = now_millis();
        let actions = self
            .0
            .session
            .lock()
            .await
            .handle(Event::MessageReceived { peer, bytes }, now);
        self.dispatch(actions, peer_hex).await;
        // TODO: collect Send actions and return their encoded bytes.
        Vec::new()
    }

    async fn dispatch(&self, actions: Vec<Action>, peer_hex: &str) {
        for action in actions {
            match action {
                Action::SetAccess { peer: _, level } => {
                    self.0.adapter.set_access(peer_hex, level);
                }

                Action::VerifyBootstrapToken { peer, token } => {
                    let token_str = match std::str::from_utf8(&token) {
                        Ok(s) => s.to_string(),
                        Err(_) => {
                            tracing::warn!(peer = peer_hex, "bootstrap token is not valid UTF-8");
                            self.send_bootstrap_result(peer, false, 0).await;
                            continue;
                        }
                    };

                    let wallet = self.0.wallet.clone_ref();
                    let driver = self.clone();
                    let peer_hex_owned = peer_hex.to_string();

                    // Verify against the mint without holding the session lock.
                    tokio::spawn(async move {
                        let (ok, amount) = match wallet.verify(&token_str).await {
                            Ok(amount) => {
                                tracing::info!(
                                    peer = %peer_hex_owned,
                                    amount_milli_sat = amount,
                                    "bootstrap token verified"
                                );
                                (true, amount)
                            }
                            Err(e) => {
                                tracing::warn!(
                                    peer = %peer_hex_owned,
                                    err = %e,
                                    "bootstrap token rejected"
                                );
                                (false, 0)
                            }
                        };
                        driver.send_bootstrap_result(peer, ok, amount).await;
                    });
                }

                Action::StartMetering { peer: _ } => {
                    tracing::debug!(peer = peer_hex, "start metering (stub)");
                    // TODO: subscribe to OS byte counters and emit MeterSample events.
                }

                Action::StopMetering { peer: _ } => {
                    tracing::debug!(peer = peer_hex, "stop metering");
                }

                Action::Send { peer: _, bytes: _ } => {
                    // TODO: route bytes to the connection's write half.
                    tracing::debug!(peer = peer_hex, "send (stub)");
                }

                Action::Disconnect { peer: _ } => {
                    tracing::debug!(peer = peer_hex, "disconnect (stub)");
                }
            }
        }
    }

    /// Feed a bootstrap verification result back into the session.
    /// Separate non-recursive method so it doesn't cause async recursion.
    async fn send_bootstrap_result(&self, peer: PeerId, ok: bool, amount: u64) {
        let peer_hex = hex::encode(peer.0.as_bytes());
        let now = now_millis();
        let actions = self
            .0
            .session
            .lock()
            .await
            .handle(Event::BootstrapVerified { peer, amount, ok }, now);
        // dispatch the resulting actions (SetAccess, StartMetering, etc.) directly,
        // without recursing through VerifyBootstrapToken again.
        for action in actions {
            match action {
                Action::SetAccess { peer: _, level } => {
                    self.0.adapter.set_access(&peer_hex, level);
                }
                Action::StartMetering { peer: _ } => {
                    tracing::debug!(peer = %peer_hex, "start metering (stub)");
                }
                Action::StopMetering { peer: _ } => {
                    tracing::debug!(peer = %peer_hex, "stop metering");
                }
                Action::Send { .. } | Action::Disconnect { .. } => {}
                // VerifyBootstrapToken cannot appear here — core only emits it
                // from MessageReceived, not from BootstrapVerified.
                Action::VerifyBootstrapToken { .. } => {
                    tracing::error!("unexpected VerifyBootstrapToken from BootstrapVerified");
                }
            }
        }
    }
}

/// Build a `PeerId` from a hex-encoded compressed public key.
/// Falls back to a zeroed key on bad input — the server validates before calling.
fn parse_peer(peer_hex: &str) -> PeerId {
    let bytes = hex::decode(peer_hex).unwrap_or_default();
    let arr: [u8; 33] = bytes.try_into().unwrap_or([0u8; 33]);
    PeerId(PublicKey::from_bytes(arr))
}

fn now_millis() -> Millis {
    Millis(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64,
    )
}
