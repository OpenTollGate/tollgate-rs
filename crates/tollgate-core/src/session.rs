//! The per-node TollGate state machine.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use tollgate_protocol::{BootstrapAck, BootstrapToken, MessageType, MeteringReport, Reject};

use crate::access::AccessLevel;
use crate::action::Action;
use crate::event::Event;
use crate::metering::{Counters, reconcile};
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

/// An immutable view of one peer's state, for status/monitoring tooling.
#[derive(Clone, Copy, Debug)]
pub struct PeerSnapshot {
    pub peer: PeerId,
    pub phase: PeerPhase,
    /// Scaled milli-unit balance, **signed**: `+` the peer has prepaid credit
    /// with us (net earner), `−` we owe them — we deliver at a negative price and
    /// pay them to attract their traffic (net spender). See [`Price::cost_scaled`].
    pub balance: i64,
    /// Units delivered to the peer since the metering baseline — matches the
    /// MeteringReport sent to the peer. Zero before the first meter sample.
    pub delivered: u64,
    /// Units received from the peer since the metering baseline.
    pub received: u64,
    /// When the current metering session began, or `None` until the first meter
    /// sample establishes the baseline. The host computes elapsed against its clock.
    pub metering_start: Option<Millis>,
    /// Metering drift on the delivery we charge for: how far the peer's reported
    /// `received` diverges from our own `delivered`, as a fraction (0.02 = 2%).
    /// `None` until the peer has sent a MeteringReport we can compare against.
    pub drift: Option<f64>,
}

/// How a node reconciles metering disagreement with a peer. Defaults follow
/// `docs/design/core/tollgate-metering.md`: 5% tolerance, close after 3
/// consecutive intervals over tolerance.
#[derive(Clone, Copy, Debug)]
pub struct MeteringPolicy {
    /// Drift tolerance in basis points (500 = 5%). Within it, both sides bill on
    /// the higher value and merely note the discrepancy; beyond it, the peer is
    /// warned (Reject: transit loss).
    pub tolerance_bps: u32,
    /// Consecutive over-tolerance intervals before the peer is suspended for
    /// renegotiation.
    pub max_streak: u8,
}

impl Default for MeteringPolicy {
    fn default() -> Self {
        Self {
            tolerance_bps: 500,
            max_streak: 3,
        }
    }
}

/// Per-peer bookkeeping.
#[derive(Clone, Debug)]
struct PeerSession {
    phase: PeerPhase,
    /// Scaled milli-unit balance (scale = 1000), **signed**. Bootstrap top-ups
    /// are additive; metering lowers it by the cost magnitude, so a negative
    /// price (we pay the peer) drives it below zero — the subsidy we owe.
    balance: i64,
    /// The last meter reading, or `None` until the first sample establishes the
    /// baseline (so a non-zero starting counter isn't charged retroactively).
    last_counters: Option<Counters>,
    /// When `last_counters` was taken.
    last_meter_at: Millis,
    /// Counter values at the metering baseline — subtracted out so the
    /// MeteringReport carries usage *cumulative since this metering session*.
    metering_baseline: Counters,
    /// When the metering baseline was established (for the report's `elapsed_ms`).
    metering_start: Millis,
    /// The peer's last reported counters (cumulative since its baseline), from its
    /// inbound MeteringReport. `received` is what it says it got from us — compared
    /// against our `delivered` to detect drift and bill on the higher value.
    peer_report: Option<Counters>,
    /// Units already billed (cumulative since baseline). Billing charges the
    /// reconciled higher value, so this tracks the running total to charge only
    /// the increment each interval — robust to reports arriving between samples.
    billed_units: u64,
    /// Consecutive metering intervals whose drift exceeded tolerance. Reset to 0
    /// on any in-tolerance interval; triggers suspension at `policy.max_streak`.
    over_tolerance_streak: u8,
}

impl PeerSession {
    fn new() -> Self {
        Self {
            phase: PeerPhase::New,
            balance: 0,
            last_counters: None,
            last_meter_at: Millis(0),
            metering_baseline: Counters::default(),
            metering_start: Millis(0),
            peer_report: None,
            billed_units: 0,
            over_tolerance_streak: 0,
        }
    }

    /// Reset per-metering-session state when a fresh baseline is established (first
    /// sample after activation or re-admit) so usage, billing, and drift all start
    /// from zero and aren't compared across baselines.
    fn reset_metering(&mut self, counters: Counters, now: Millis) {
        self.last_counters = Some(counters);
        self.last_meter_at = now;
        self.metering_baseline = counters;
        self.metering_start = now;
        self.peer_report = None;
        self.billed_units = 0;
        self.over_tolerance_streak = 0;
    }
}

/// Holds one [`PeerSession`] per peer and turns [`Event`]s into [`Action`]s.
/// Pure and synchronous — see the crate-level docs.
#[derive(Debug, Default)]
pub struct Session {
    peers: BTreeMap<PeerId, PeerSession>,
    /// The rate this node charges peers for delivery (v1: one rate for all).
    price: Price,
    /// How metering disagreement with a peer is reconciled.
    policy: MeteringPolicy,
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

    /// Set the metering reconciliation policy (drift tolerance + escalation).
    pub fn set_metering_policy(&mut self, policy: MeteringPolicy) {
        self.policy = policy;
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
                // TODO: handle Announce, PriceSheet, Accept, etc.
                match tollgate_protocol::peek_type(&bytes) {
                    Some(MessageType::BootstrapToken) => {
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
                    // The peer's metering report: its own (cumulative) tally of what
                    // it delivered to and received from us. We store it; the next
                    // MeterSample reconciles it against our counters (drift + bill on
                    // the higher value). The peer's `received` is what it got from us,
                    // so it lines up with our `delivered`.
                    Some(MessageType::MeteringReport) => {
                        if let Ok(report) = MeteringReport::decode(&bytes) {
                            if let Some(peer_session) = self.peers.get_mut(&peer) {
                                peer_session.peer_report = Some(Counters {
                                    delivered: report.delivered,
                                    received: report.received,
                                });
                            }
                        }
                    }
                    _ => {}
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
                        peer_session.balance = peer_session.balance.saturating_add(amount as i64);
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
                let policy = self.policy;
                if let Some(peer_session) = self.peers.get_mut(&peer) {
                    if peer_session.phase == PeerPhase::Active {
                        match peer_session.last_counters {
                            // First reading after activation: establish the
                            // baseline, charge nothing.
                            None => {
                                peer_session.reset_metering(counters, now);
                            }
                            Some(_prev) => {
                                let elapsed = now.since(peer_session.last_meter_at);
                                let baseline = peer_session.metering_baseline;
                                // Our own tallies, cumulative since the baseline.
                                let our_delivered =
                                    counters.delivered.saturating_sub(baseline.delivered);
                                let our_received =
                                    counters.received.saturating_sub(baseline.received);

                                // The peer's tally of what it received from us — the
                                // same flow as our `delivered`. Bill on the higher of
                                // the two (transit-loss resolution); charge only the
                                // increment since we last billed so a report arriving
                                // between samples doesn't double-charge.
                                let peer_received = peer_session.peer_report.map(|r| r.received);
                                let billable = match peer_received {
                                    Some(r) => reconcile(our_delivered, r),
                                    None => our_delivered,
                                };
                                let new_units = billable.saturating_sub(peer_session.billed_units);
                                let cost = price.cost_scaled(elapsed, new_units);
                                peer_session.billed_units = billable;
                                peer_session.last_counters = Some(counters);
                                peer_session.last_meter_at = now;
                                // Apply the signed cost by magnitude. Charging
                                // (cost ≥ 0) draws down the peer's prepaid credit;
                                // paying it (negative price, to attract its traffic)
                                // drives the balance negative — the subsidy we owe it.
                                // tolltop reads the sign: `+` earner, `−` spender.
                                let charging = cost >= 0;
                                peer_session.balance =
                                    peer_session.balance.saturating_sub(cost.saturating_abs());

                                // Drift = |our_delivered − peer_received| / our_delivered.
                                // Compared as integers (diff·10000 vs delivered·bps) to
                                // keep the hot path float-free; over tolerance warns the
                                // peer, and persistence suspends for renegotiation.
                                if let Some(r) = peer_received {
                                    let diff = our_delivered.abs_diff(r);
                                    let over = our_delivered > 0
                                        && (diff as u128) * 10_000
                                            > (our_delivered as u128)
                                                * (policy.tolerance_bps as u128);
                                    if over {
                                        peer_session.over_tolerance_streak =
                                            peer_session.over_tolerance_streak.saturating_add(1);
                                        actions.push(Action::Send {
                                            peer,
                                            bytes: Reject::transit_loss().encode(),
                                        });
                                    } else {
                                        peer_session.over_tolerance_streak = 0;
                                    }
                                }

                                // Report our usage cumulative since the baseline so the
                                // peer can track its own balance and top up before
                                // exhaustion. Coalesced on the host (latest wins).
                                actions.push(Action::Send {
                                    peer,
                                    bytes: MeteringReport::new(
                                        now.since(peer_session.metering_start),
                                        our_delivered,
                                        our_received,
                                    )
                                    .encode(),
                                });

                                // Cut a *charged* peer whose prepaid credit is spent
                                // (balance non-positive), or any peer after persistent
                                // over-tolerance drift (close & renegotiate). A peer we
                                // PAY (negative price) is never cut for exhaustion — its
                                // negative balance is the subsidy we chose, not a debt
                                // it must clear. The transit-loss warning was already
                                // sent above.
                                let exhausted = charging && peer_session.balance <= 0;
                                let drift_persisted =
                                    peer_session.over_tolerance_streak >= policy.max_streak;
                                if exhausted || drift_persisted {
                                    peer_session.phase = PeerPhase::Suspended;
                                    if exhausted {
                                        // Tell the peer *why* it's being cut so it can
                                        // top up rather than silently losing access.
                                        actions.push(Action::Send {
                                            peer,
                                            bytes: Reject::balance_exhausted().encode(),
                                        });
                                    }
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

    /// Snapshot every tracked peer's state for inspection (status CLI / TUI).
    /// Pure read — the host combines this with its own per-peer data (IP, idle
    /// time) to render a full picture.
    pub fn snapshot(&self) -> Vec<PeerSnapshot> {
        self.peers
            .iter()
            .map(|(peer, s)| {
                // Usage cumulative since the baseline (same basis as MeteringReport).
                let (delivered, received, metering_start) = match s.last_counters {
                    Some(c) => (
                        c.delivered.saturating_sub(s.metering_baseline.delivered),
                        c.received.saturating_sub(s.metering_baseline.received),
                        Some(s.metering_start),
                    ),
                    None => (0, 0, None),
                };
                // Drift of the peer's reported `received` against our `delivered` on
                // the flow we charge for. Pure arithmetic (no_std-safe): the integer
                // `abs_diff` keeps it exact before the single divide.
                let drift = match s.peer_report {
                    Some(r) if delivered > 0 => {
                        Some(delivered.abs_diff(r.received) as f64 / delivered as f64)
                    }
                    _ => None,
                };
                PeerSnapshot {
                    peer: *peer,
                    phase: s.phase,
                    balance: s.balance,
                    delivered,
                    received,
                    metering_start,
                    drift,
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tollgate_protocol::PublicKey;

    fn peer(byte: u8) -> PeerId {
        PeerId(PublicKey::from_bytes([byte; 33]))
    }

    /// Decode the first MeteringReport among a list of actions, if any.
    fn find_metering_report(actions: &[Action]) -> Option<MeteringReport> {
        actions.iter().find_map(|a| match a {
            Action::Send { bytes, .. }
                if matches!(
                    tollgate_protocol::peek_type(bytes),
                    Some(MessageType::MeteringReport)
                ) =>
            {
                MeteringReport::decode(bytes).ok()
            }
            _ => None,
        })
    }

    /// Decode the first Reject among a list of actions, if any.
    fn find_reject(actions: &[Action]) -> Option<Reject> {
        actions.iter().find_map(|a| match a {
            Action::Send { bytes, .. }
                if matches!(
                    tollgate_protocol::peek_type(bytes),
                    Some(MessageType::Reject)
                ) =>
            {
                Reject::decode(bytes).ok()
            }
            _ => None,
        })
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

        // First sample establishes the baseline — no charge, no report.
        assert!(session.handle(sample(0), Millis(1000)).is_empty());

        // Deliver 4 units → cost 4, balance 10 → 6, still active. A MeteringReport
        // is sent carrying usage cumulative since the baseline.
        let actions = session.handle(sample(4), Millis(2000));
        let report = find_metering_report(&actions).expect("a MeteringReport");
        assert_eq!(report.delivered, 4);
        assert_eq!(report.elapsed_ms, 1000); // 2000 − baseline at 1000
        assert!(!actions.iter().any(|a| matches!(
            a,
            Action::SetAccess {
                level: AccessLevel::Suspended,
                ..
            }
        )));

        // Deliver 6 more (cumulative 10) → cost 6, balance 6 → 0 → suspend, with a
        // final MeteringReport and a balance-exhausted Reject.
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
        assert_eq!(
            find_metering_report(&actions).expect("report").delivered,
            10
        );
        assert!(
            find_reject(&actions)
                .expect("a Reject")
                .is_balance_exhausted()
        );

        // Once suspended, further samples are ignored (no more actions).
        assert!(session.handle(sample(99), Millis(4000)).is_empty());
    }

    #[test]
    fn metering_report_is_cumulative_since_the_baseline() {
        let mut session = Session::new();
        session.set_price(Price {
            per_second: 0,
            per_unit: 1,
        });
        let p = peer(11);
        session.handle(Event::PeerConnected { peer: p }, Millis(0));
        session.handle(
            Event::BootstrapVerified {
                peer: p,
                amount: 1_000_000,
                ok: true,
            },
            Millis(0),
        );

        let sample = |d, r| Event::MeterSample {
            peer: p,
            counters: Counters {
                delivered: d,
                received: r,
            },
        };

        // Baseline at non-zero counters (the meter had prior traffic); subtracted out.
        assert!(session.handle(sample(100, 50), Millis(1000)).is_empty());
        let actions = session.handle(sample(130, 70), Millis(6000));
        let report = find_metering_report(&actions).expect("a MeteringReport");
        assert_eq!(report.delivered, 30); // 130 − 100
        assert_eq!(report.received, 20); // 70 − 50
        assert_eq!(report.elapsed_ms, 5000); // 6000 − baseline at 1000
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

        // Baseline (no report), then drain time-cost to exactly zero → suspend.
        assert!(session.handle(sample(), Millis(0)).is_empty());
        let mid = session.handle(sample(), Millis(50)); // 100 → 50 (report, no suspend)
        assert!(!mid.iter().any(|a| matches!(
            a,
            Action::SetAccess {
                level: AccessLevel::Suspended,
                ..
            }
        )));
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
        // Metering resumes from the new baseline: 50ms → cost 50, balance 100 → 50
        // (a report is sent, but no re-suspend — the suspended gap was not billed).
        let resumed = session.handle(sample(), Millis(10_050));
        assert!(!resumed.iter().any(|a| matches!(
            a,
            Action::SetAccess {
                level: AccessLevel::Suspended,
                ..
            }
        )));
        assert_eq!(session.peer_phase(&p), Some(PeerPhase::Active));
    }

    #[test]
    fn snapshot_reports_phase_balance_and_counters() {
        let mut session = Session::new();
        session.set_price(Price {
            per_second: 0,
            per_unit: 1,
        });
        let p = peer(5);
        session.handle(Event::PeerConnected { peer: p }, Millis(0));
        session.handle(
            Event::BootstrapVerified {
                peer: p,
                amount: 5000,
                ok: true,
            },
            Millis(0),
        );
        let sample = |delivered, received| Event::MeterSample {
            peer: p,
            counters: Counters {
                delivered,
                received,
            },
        };
        session.handle(sample(0, 0), Millis(1000)); // baseline
        session.handle(sample(3, 1), Millis(2000)); // deliver 3 → cost 3

        let snap = session.snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].peer, p);
        assert_eq!(snap[0].phase, PeerPhase::Active);
        assert_eq!(snap[0].balance, 5000 - 3);
        assert_eq!(snap[0].delivered, 3);
        assert_eq!(snap[0].received, 1);
        assert_eq!(snap[0].metering_start, Some(Millis(1000))); // baseline sample time
    }

    /// Whether any action is a transit-loss Reject (the drift warning).
    fn has_transit_loss(actions: &[Action]) -> bool {
        actions.iter().any(|a| {
            matches!(a, Action::Send { bytes, .. }
                if matches!(
                    tollgate_protocol::peek_type(bytes),
                    Some(MessageType::Reject)
                ) && Reject::decode(bytes).map(|r| r.is_transit_loss()).unwrap_or(false))
        })
    }

    /// Whether any action suspends the peer's access.
    fn suspends_access(actions: &[Action]) -> bool {
        actions.iter().any(|a| {
            matches!(
                a,
                Action::SetAccess {
                    level: AccessLevel::Suspended,
                    ..
                }
            )
        })
    }

    /// A peer's MeteringReport claiming it received `r` units from us (cumulative).
    fn peer_received(p: PeerId, r: u64) -> Event {
        Event::MessageReceived {
            peer: p,
            bytes: MeteringReport::new(0, 0, r).encode(),
        }
    }

    #[test]
    fn drift_within_tolerance_bills_higher_value_and_reports_drift() {
        let mut session = Session::new();
        session.set_price(Price {
            per_second: 0,
            per_unit: 1,
        });
        let p = peer(20);
        session.handle(Event::PeerConnected { peer: p }, Millis(0));
        session.handle(
            Event::BootstrapVerified {
                peer: p,
                amount: 1000,
                ok: true,
            },
            Millis(0),
        );
        let sample = |d| Event::MeterSample {
            peer: p,
            counters: Counters {
                delivered: d,
                received: 0,
            },
        };

        session.handle(sample(0), Millis(1000)); // baseline

        // The peer claims it received MORE than we delivered (103 vs 100): 3% drift,
        // within the 5% tolerance. We bill on the higher (peer's) value, no warning.
        session.handle(peer_received(p, 103), Millis(1500));
        let actions = session.handle(sample(100), Millis(2000));
        assert!(
            !has_transit_loss(&actions),
            "3% is within tolerance — no warning"
        );
        assert!(!suspends_access(&actions));

        let snap = &session.snapshot()[0];
        assert_eq!(
            snap.balance,
            1000 - 103,
            "billed the higher (peer's) value, not our 100"
        );
        assert_eq!(snap.delivered, 100);
        let drift = snap.drift.expect("drift known once the peer has reported");
        assert!((drift - 0.03).abs() < 1e-9, "drift {drift} ≈ 0.03");
    }

    #[test]
    fn drift_over_tolerance_warns_but_one_interval_stays_active() {
        let mut session = Session::new();
        session.set_price(Price {
            per_second: 0,
            per_unit: 1,
        });
        let p = peer(21);
        session.handle(Event::PeerConnected { peer: p }, Millis(0));
        session.handle(
            Event::BootstrapVerified {
                peer: p,
                amount: 1_000_000,
                ok: true,
            },
            Millis(0),
        );
        let sample = |d| Event::MeterSample {
            peer: p,
            counters: Counters {
                delivered: d,
                received: 0,
            },
        };

        session.handle(sample(0), Millis(1000)); // baseline
        // Peer admits only 80 of our 100 delivered → 20% drift, over the 5% tolerance.
        session.handle(peer_received(p, 80), Millis(1500));
        let actions = session.handle(sample(100), Millis(2000));
        assert!(
            has_transit_loss(&actions),
            "over tolerance → transit-loss warning"
        );
        assert!(
            !suspends_access(&actions),
            "one bad interval doesn't cut the peer"
        );
        assert_eq!(session.peer_phase(&p), Some(PeerPhase::Active));
    }

    #[test]
    fn persistent_drift_suspends_after_three_intervals() {
        let mut session = Session::new();
        session.set_price(Price {
            per_second: 0,
            per_unit: 1,
        });
        let p = peer(22);
        session.handle(Event::PeerConnected { peer: p }, Millis(0));
        session.handle(
            Event::BootstrapVerified {
                peer: p,
                amount: 1_000_000,
                ok: true,
            },
            Millis(0),
        );
        let sample = |d| Event::MeterSample {
            peer: p,
            counters: Counters {
                delivered: d,
                received: 0,
            },
        };

        session.handle(sample(0), Millis(1000)); // baseline

        // A lying/malfunctioning peer that consistently under-reports its received
        // count by ~20% (we deliver 100 more each interval; it only ever admits 80).
        session.handle(peer_received(p, 80), Millis(1500));
        let a1 = session.handle(sample(100), Millis(2000));
        assert!(has_transit_loss(&a1) && !suspends_access(&a1), "strike 1");

        session.handle(peer_received(p, 160), Millis(2500));
        let a2 = session.handle(sample(200), Millis(3000));
        assert!(has_transit_loss(&a2) && !suspends_access(&a2), "strike 2");
        assert_eq!(session.peer_phase(&p), Some(PeerPhase::Active));

        session.handle(peer_received(p, 240), Millis(3500));
        let a3 = session.handle(sample(300), Millis(4000));
        // Strike 3: the peer is cut for renegotiation — access suspended, metering
        // stopped — even though its balance is nowhere near exhausted.
        assert!(
            suspends_access(&a3),
            "3rd over-tolerance interval suspends the peer"
        );
        assert!(a3.iter().any(|x| matches!(x, Action::StopMetering { .. })));
        assert_eq!(session.peer_phase(&p), Some(PeerPhase::Suspended));
        assert!(
            find_reject(&a3).is_none_or(|r| !r.is_balance_exhausted()),
            "suspension is from drift, not balance exhaustion"
        );

        // Once suspended, further samples are ignored — the peer is effectively cut.
        session.handle(peer_received(p, 320), Millis(4500));
        assert!(session.handle(sample(400), Millis(5000)).is_empty());
    }

    #[test]
    fn an_in_tolerance_interval_resets_the_drift_streak() {
        let mut session = Session::new();
        session.set_price(Price {
            per_second: 0,
            per_unit: 1,
        });
        let p = peer(23);
        session.handle(Event::PeerConnected { peer: p }, Millis(0));
        session.handle(
            Event::BootstrapVerified {
                peer: p,
                amount: 1_000_000,
                ok: true,
            },
            Millis(0),
        );
        let sample = |d| Event::MeterSample {
            peer: p,
            counters: Counters {
                delivered: d,
                received: 0,
            },
        };

        session.handle(sample(0), Millis(1000)); // baseline

        // Two over-tolerance intervals (strikes 1, 2)…
        session.handle(peer_received(p, 80), Millis(1500));
        session.handle(sample(100), Millis(2000));
        session.handle(peer_received(p, 160), Millis(2500));
        session.handle(sample(200), Millis(3000));

        // …then a clean interval (295 of 300 → 1.7% drift) resets the streak.
        session.handle(peer_received(p, 295), Millis(3500));
        let clean = session.handle(sample(300), Millis(4000));
        assert!(!has_transit_loss(&clean), "1.7% is within tolerance");

        // Two more bad intervals only climb back to strike 2 — never suspended.
        session.handle(peer_received(p, 375), Millis(4500));
        let a4 = session.handle(sample(400), Millis(5000));
        session.handle(peer_received(p, 470), Millis(5500));
        let a5 = session.handle(sample(500), Millis(6000));
        assert!(has_transit_loss(&a4) && has_transit_loss(&a5));
        assert!(!suspends_access(&a4) && !suspends_access(&a5));
        assert_eq!(session.peer_phase(&p), Some(PeerPhase::Active));
    }

    #[test]
    fn negative_pricing_pays_the_peer_and_never_suspends() {
        let mut session = Session::new();
        // We PAY the peer 2 milli-sat per unit we deliver to it (negative pricing —
        // we subsidise delivery to attract its traffic).
        session.set_price(Price {
            per_second: 0,
            per_unit: -2,
        });
        let p = peer(24);
        session.handle(Event::PeerConnected { peer: p }, Millis(0));
        // It still bootstraps a small amount to open the session.
        session.handle(
            Event::BootstrapVerified {
                peer: p,
                amount: 10,
                ok: true,
            },
            Millis(0),
        );
        let sample = |d| Event::MeterSample {
            peer: p,
            counters: Counters {
                delivered: d,
                received: 0,
            },
        };

        session.handle(sample(0), Millis(1000)); // baseline

        // Deliver 100 units at −2 → we owe the peer 200; balance 10 → −190. We are a
        // net spender, and the peer is NOT suspended — paying it is deliberate.
        let actions = session.handle(sample(100), Millis(2000));
        assert!(
            !suspends_access(&actions),
            "we are paying — never cut for that"
        );
        assert_eq!(session.peer_phase(&p), Some(PeerPhase::Active));
        assert_eq!(
            session.snapshot()[0].balance,
            10 - 200,
            "negative pricing drives the balance negative (we owe the peer)"
        );

        // Keep delivering: the balance only goes more negative, still never cut.
        let actions = session.handle(sample(250), Millis(3000));
        assert!(!suspends_access(&actions));
        assert_eq!(session.snapshot()[0].balance, 10 - 500); // 250 units × −2
        assert_eq!(session.peer_phase(&p), Some(PeerPhase::Active));
    }
}
