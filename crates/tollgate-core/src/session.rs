//! The per-node TollGate state machine.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use tollgate_protocol::{BootstrapAck, BootstrapToken, MessageType, Reject};

use crate::access::AccessLevel;
use crate::action::Action;
use crate::event::Event;
use crate::metering::Counters;
use crate::peer::PeerId;
use crate::pricing::Price;
use crate::time::Millis;

/// Where one peer relationship sits in its lifecycle.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PeerPhase {
    /// Connected, no payment yet — blocked.
    New,
    /// A bootstrap token was received and is being verified with the mint.
    BootstrapPending,
    /// Paid and allowed; metered.
    Active,
    /// Was active, balance exhausted — blocked, awaiting top-up.
    Suspended,
    /// Torn down.
    Closed,
}

/// Per-peer bookkeeping.
#[derive(Clone, Debug)]
struct PeerSession {
    phase: PeerPhase,
    /// Scaled milli-unit balance (scale = 1000). Bootstrap top-ups are additive.
    balance: u64,
    /// The last meter reading, or `None` until the first sample establishes the
    /// baseline (so a non-zero starting counter isn't charged retroactively).
    last_counters: Option<Counters>,
    /// When `last_counters` was taken.
    last_meter_at: Millis,
}

impl PeerSession {
    fn new() -> Self {
        Self {
            phase: PeerPhase::New,
            balance: 0,
            last_counters: None,
            last_meter_at: Millis(0),
        }
    }
}

/// Holds one [`PeerSession`] per peer and turns [`Event`]s into [`Action`]s.
/// Pure and synchronous — see the crate-level docs.
#[derive(Debug, Default)]
pub struct Session {
    peers: BTreeMap<PeerId, PeerSession>,
    /// The rate this node charges peers for delivery (v1: one rate for all).
    price: Price,
}

impl Session {
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the rate charged to peers. v1 uses a single price for all peers;
    /// per-peer pricing arrives with PriceSheet negotiation.
    pub fn set_price(&mut self, price: Price) {
        self.price = price;
    }

    /// Drive the state machine with one event. `now` is the host's monotonic
    /// clock; core never reads time on its own.
    ///
    /// This currently implements the bootstrap-only happy path (the first
    /// milestone: an IP deployment that sells access for plain Cashu tokens).
    /// Spilman channels, rollover, and netting are added on top later.
    pub fn handle(&mut self, event: Event, now: Millis) -> Vec<Action> {
        let mut actions = Vec::new();
        match event {
            Event::PeerConnected { peer } => {
                // Idempotent: the HTTP-polling transport re-sends Announce on every
                // exchange, so this fires repeatedly for an already-known peer. Only
                // a genuinely new peer is (re)initialised and blocked — re-announces
                // must not wipe a paid peer's balance back to zero.
                if let alloc::collections::btree_map::Entry::Vacant(slot) = self.peers.entry(peer) {
                    slot.insert(PeerSession::new());
                    // New peers start blocked until they pay (access-control default).
                    actions.push(Action::SetAccess {
                        peer,
                        level: AccessLevel::None,
                    });
                }
            }

            Event::MessageReceived { peer, bytes } => {
                // Decode the message type and dispatch. The bootstrap-only path
                // turns a BootstrapToken (0x07) into a token-verification request
                // carrying the actual Cashu token bytes.
                // TODO: handle Announce, PriceSheet, Accept, MeteringReport, etc.
                if let Some(MessageType::BootstrapToken) = tollgate_protocol::peek_type(&bytes) {
                    if let Ok(msg) = BootstrapToken::decode(&bytes) {
                        if let Some(peer_session) = self.peers.get_mut(&peer) {
                            peer_session.phase = PeerPhase::BootstrapPending;
                            actions.push(Action::VerifyBootstrapToken {
                                peer,
                                token: msg.token_bytes(),
                            });
                        }
                    }
                }
            }

            Event::BootstrapVerified { peer, amount, ok } => {
                if let Some(peer_session) = self.peers.get_mut(&peer) {
                    if ok {
                        // A token accepted while the peer was *not* already metered
                        // (New, or Suspended after exhaustion) starts a fresh
                        // metering session: drop the stale baseline so the gap since
                        // the last sample isn't charged on resume. Balance is always
                        // additive — top-ups never reset it.
                        let was_active = peer_session.phase == PeerPhase::Active;
                        peer_session.balance = peer_session.balance.saturating_add(amount);
                        peer_session.phase = PeerPhase::Active;
                        if !was_active {
                            peer_session.last_counters = None;
                            peer_session.last_meter_at = now;
                        }
                        actions.push(Action::Send {
                            peer,
                            bytes: BootstrapAck::accepted().encode(),
                        });
                        actions.push(Action::SetAccess {
                            peer,
                            level: AccessLevel::Active,
                        });
                        actions.push(Action::StartMetering { peer });
                    } else {
                        peer_session.phase = PeerPhase::New;
                        actions.push(Action::Send {
                            peer,
                            bytes: BootstrapAck::rejected("token verification failed").encode(),
                        });
                        actions.push(Action::SetAccess {
                            peer,
                            level: AccessLevel::None,
                        });
                    }
                }
            }

            Event::MeterSample { peer, counters } => {
                let price = self.price;
                if let Some(peer_session) = self.peers.get_mut(&peer) {
                    if peer_session.phase == PeerPhase::Active {
                        match peer_session.last_counters {
                            // First reading after activation: establish the
                            // baseline, charge nothing.
                            None => {
                                peer_session.last_counters = Some(counters);
                                peer_session.last_meter_at = now;
                            }
                            Some(prev) => {
                                let delivered = counters.delivered.saturating_sub(prev.delivered);
                                let elapsed = now.since(peer_session.last_meter_at);
                                let cost = price.cost_scaled(elapsed, delivered);
                                peer_session.last_counters = Some(counters);
                                peer_session.last_meter_at = now;
                                peer_session.balance = peer_session.balance.saturating_sub(cost);
                                if peer_session.balance == 0 {
                                    peer_session.phase = PeerPhase::Suspended;
                                    // Tell the peer *why* it's being cut so it can
                                    // top up (or upgrade to Spilman) rather than
                                    // silently losing connectivity.
                                    actions.push(Action::Send {
                                        peer,
                                        bytes: Reject::balance_exhausted().encode(),
                                    });
                                    actions.push(Action::SetAccess {
                                        peer,
                                        level: AccessLevel::Suspended,
                                    });
                                    actions.push(Action::StopMetering { peer });
                                }
                            }
                        }
                    }
                }
            }

            Event::PeerDisconnected { peer } => {
                if let Some(peer_session) = self.peers.get_mut(&peer) {
                    peer_session.phase = PeerPhase::Closed;
                }
                // Revoke firewall access and stop metering before forgetting the peer.
                actions.push(Action::SetAccess {
                    peer,
                    level: AccessLevel::None,
                });
                actions.push(Action::StopMetering { peer });
                self.peers.remove(&peer);
            }

            Event::Tick => {
                // TODO: per-interval netting, rollover checks, channel expiry.
            }
        }
        actions
    }

    /// Number of peers currently tracked (inspection/test helper).
    pub fn peer_count(&self) -> usize {
        self.peers.len()
    }

    /// The lifecycle [`PeerPhase`] of a tracked peer, or `None` if unknown.
    ///
    /// The host uses this to decide reaping: an idle peer that is *not* `Active`
    /// (never paid, or suspended after exhaustion) can be torn down, whereas an
    /// `Active` peer holds paid balance and must be kept even while it is silent.
    pub fn peer_phase(&self, peer: &PeerId) -> Option<PeerPhase> {
        self.peers.get(peer).map(|p| p.phase)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tollgate_protocol::PublicKey;

    fn peer(byte: u8) -> PeerId {
        PeerId(PublicKey::from_bytes([byte; 33]))
    }

    #[test]
    fn bootstrap_only_happy_path() {
        let mut session = Session::new();
        let p = peer(1);

        let actions = session.handle(Event::PeerConnected { peer: p }, Millis(0));
        assert!(matches!(
            actions.as_slice(),
            [Action::SetAccess {
                level: AccessLevel::None,
                ..
            }]
        ));

        let token_frame =
            tollgate_protocol::BootstrapToken::new(b"cashuBtesttoken".to_vec()).encode();
        let actions = session.handle(
            Event::MessageReceived {
                peer: p,
                bytes: token_frame,
            },
            Millis(1),
        );
        match actions.as_slice() {
            [Action::VerifyBootstrapToken { token, .. }] => {
                assert_eq!(token, b"cashuBtesttoken");
            }
            other => panic!("expected VerifyBootstrapToken, got {other:?}"),
        }

        let actions = session.handle(
            Event::BootstrapVerified {
                peer: p,
                amount: 5000,
                ok: true,
            },
            Millis(2),
        );
        assert!(actions.iter().any(|a| matches!(
            a,
            Action::SetAccess {
                level: AccessLevel::Active,
                ..
            }
        )));
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, Action::StartMetering { .. }))
        );

        // An accepted BootstrapAck is queued for the wire.
        let ack_bytes = actions
            .iter()
            .find_map(|a| match a {
                Action::Send { bytes, .. } => Some(bytes.clone()),
                _ => None,
            })
            .expect("a Send action carrying a BootstrapAck");
        let ack = BootstrapAck::decode(&ack_bytes).expect("decode BootstrapAck");
        assert!(ack.is_accepted());

        assert_eq!(session.peer_count(), 1);
    }

    #[test]
    fn rejected_bootstrap_blocks_and_acks() {
        let mut session = Session::new();
        let p = peer(3);
        session.handle(Event::PeerConnected { peer: p }, Millis(0));

        let actions = session.handle(
            Event::BootstrapVerified {
                peer: p,
                amount: 0,
                ok: false,
            },
            Millis(1),
        );

        assert!(actions.iter().any(|a| matches!(
            a,
            Action::SetAccess {
                level: AccessLevel::None,
                ..
            }
        )));
        let ack_bytes = actions
            .iter()
            .find_map(|a| match a {
                Action::Send { bytes, .. } => Some(bytes.clone()),
                _ => None,
            })
            .expect("a Send action carrying a BootstrapAck");
        assert!(
            !BootstrapAck::decode(&ack_bytes)
                .expect("decode")
                .is_accepted()
        );
    }

    #[test]
    fn metering_charges_deltas_and_suspends_when_exhausted() {
        let mut session = Session::new();
        session.set_price(Price {
            per_second: 0,
            per_unit: 1,
        }); // 1 unit = 1 milli
        let p = peer(7);

        session.handle(Event::PeerConnected { peer: p }, Millis(0));
        session.handle(
            Event::BootstrapVerified {
                peer: p,
                amount: 10,
                ok: true,
            },
            Millis(0),
        );

        let sample = |delivered| Event::MeterSample {
            peer: p,
            counters: Counters {
                delivered,
                received: 0,
            },
        };

        // First sample establishes the baseline — no charge, no actions.
        assert!(session.handle(sample(0), Millis(1000)).is_empty());
        // Deliver 4 units → cost 4, balance 10 → 6, still active.
        assert!(session.handle(sample(4), Millis(2000)).is_empty());
        // Deliver 6 more (cumulative 10) → cost 6, balance 6 → 0 → suspend.
        let actions = session.handle(sample(10), Millis(3000));
        assert!(actions.iter().any(|a| matches!(
            a,
            Action::SetAccess {
                level: AccessLevel::Suspended,
                ..
            }
        )));
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, Action::StopMetering { .. }))
        );
        // The peer is told why it was cut: a balance-exhausted Reject.
        let reject_bytes = actions
            .iter()
            .find_map(|a| match a {
                Action::Send { bytes, .. } => Some(bytes.clone()),
                _ => None,
            })
            .expect("a Send action carrying a Reject");
        assert!(
            Reject::decode(&reject_bytes)
                .expect("decode Reject")
                .is_balance_exhausted()
        );

        // Once suspended, further samples are ignored (no more actions).
        assert!(session.handle(sample(99), Millis(4000)).is_empty());
    }

    #[test]
    fn disconnect_forgets_the_peer() {
        let mut session = Session::new();
        let p = peer(2);
        session.handle(Event::PeerConnected { peer: p }, Millis(0));
        let actions = session.handle(Event::PeerDisconnected { peer: p }, Millis(1));
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, Action::StopMetering { .. }))
        );
        assert_eq!(session.peer_count(), 0);
    }

    #[test]
    fn events_for_an_unknown_peer_are_ignored() {
        let mut session = Session::new();
        let p = peer(42);
        // No PeerConnected first: every per-peer event must hit its `get_mut`
        // guard and no-op, never creating a peer or emitting actions.
        let token = tollgate_protocol::BootstrapToken::new(b"cashuBx".to_vec()).encode();
        assert!(
            session
                .handle(
                    Event::MessageReceived {
                        peer: p,
                        bytes: token
                    },
                    Millis(0)
                )
                .is_empty()
        );
        assert!(
            session
                .handle(
                    Event::BootstrapVerified {
                        peer: p,
                        amount: 100,
                        ok: true,
                    },
                    Millis(0),
                )
                .is_empty()
        );
        assert!(
            session
                .handle(
                    Event::MeterSample {
                        peer: p,
                        counters: Counters::default(),
                    },
                    Millis(0),
                )
                .is_empty()
        );
        assert_eq!(session.peer_count(), 0);
        assert_eq!(session.peer_phase(&p), None);
    }

    #[test]
    fn non_bootstrap_messages_are_ignored() {
        let mut session = Session::new();
        let p = peer(43);
        session.handle(Event::PeerConnected { peer: p }, Millis(0));

        // Garbage bytes (not a CBOR message map) → peek_type None → ignored.
        let actions = session.handle(
            Event::MessageReceived {
                peer: p,
                bytes: alloc::vec![0xff, 0xff, 0xff],
            },
            Millis(1),
        );
        assert!(actions.is_empty());

        // A valid but not-yet-handled type (Announce) is also ignored, and the
        // peer stays New — only a BootstrapToken moves it to BootstrapPending.
        let announce = tollgate_protocol::Announce::new(
            1,
            tollgate_protocol::PublicKey::from_bytes([1u8; 33]),
            "bytes",
            0,
        )
        .encode();
        let actions = session.handle(
            Event::MessageReceived {
                peer: p,
                bytes: announce,
            },
            Millis(2),
        );
        assert!(actions.is_empty());
        assert_eq!(session.peer_phase(&p), Some(PeerPhase::New));
    }

    #[test]
    fn tick_is_a_noop() {
        let mut session = Session::new();
        assert!(session.handle(Event::Tick, Millis(0)).is_empty());
    }

    #[test]
    fn reannounce_is_idempotent_and_keeps_paid_balance() {
        let mut session = Session::new();
        session.set_price(Price {
            per_second: 0,
            per_unit: 1,
        });
        let p = peer(9);
        session.handle(Event::PeerConnected { peer: p }, Millis(0));
        session.handle(
            Event::BootstrapVerified {
                peer: p,
                amount: 10,
                ok: true,
            },
            Millis(0),
        );

        // A duplicate PeerConnected (the next poll re-sends Announce) must be a
        // no-op: no SetAccess(None), and the peer stays Active with its balance.
        let actions = session.handle(Event::PeerConnected { peer: p }, Millis(1));
        assert!(
            actions.is_empty(),
            "re-announce produced actions: {actions:?}"
        );
        assert_eq!(session.peer_phase(&p), Some(PeerPhase::Active));

        // Balance survived: baseline, then deliver 10 → exhaust → suspend. (A
        // wiped session would be New with balance 0 and ignore these samples.)
        let sample = |delivered| Event::MeterSample {
            peer: p,
            counters: Counters {
                delivered,
                received: 0,
            },
        };
        assert!(session.handle(sample(0), Millis(1)).is_empty());
        let actions = session.handle(sample(10), Millis(2));
        assert!(actions.iter().any(|a| matches!(
            a,
            Action::SetAccess {
                level: AccessLevel::Suspended,
                ..
            }
        )));
    }

    #[test]
    fn readmit_after_exhaustion_resets_metering_baseline() {
        let mut session = Session::new();
        // Time-based price: cost(scaled) == elapsed_ms (per_second 1000 → ms*1000/1000).
        session.set_price(Price {
            per_second: 1000,
            per_unit: 0,
        });
        let p = peer(8);
        let sample = || Event::MeterSample {
            peer: p,
            counters: Counters {
                delivered: 0,
                received: 0,
            },
        };

        session.handle(Event::PeerConnected { peer: p }, Millis(0));
        session.handle(
            Event::BootstrapVerified {
                peer: p,
                amount: 100,
                ok: true,
            },
            Millis(0),
        );

        // Baseline, then drain time-cost to exactly zero → suspend.
        assert!(session.handle(sample(), Millis(0)).is_empty());
        assert!(session.handle(sample(), Millis(50)).is_empty()); // 100 → 50
        let actions = session.handle(sample(), Millis(100)); // 50 → 0 → suspend
        assert!(actions.iter().any(|a| matches!(
            a,
            Action::SetAccess {
                level: AccessLevel::Suspended,
                ..
            }
        )));

        // Top up 9.9s later.
        let actions = session.handle(
            Event::BootstrapVerified {
                peer: p,
                amount: 100,
                ok: true,
            },
            Millis(10_000),
        );
        assert!(actions.iter().any(|a| matches!(
            a,
            Action::SetAccess {
                level: AccessLevel::Active,
                ..
            }
        )));

        // The first sample after re-admit re-establishes the baseline and charges
        // nothing — the suspended gap must NOT be billed. Without the reset, the
        // stale `last_meter_at` would charge ~9.9s and re-suspend immediately.
        let actions = session.handle(sample(), Millis(10_000));
        assert!(
            actions.is_empty(),
            "re-admit must not re-suspend on the gap: {actions:?}"
        );
        // Metering resumes from the new baseline: 50ms → cost 50, balance 100 → 50.
        assert!(session.handle(sample(), Millis(10_050)).is_empty());
        assert_eq!(session.peer_phase(&p), Some(PeerPhase::Active));
    }
}
