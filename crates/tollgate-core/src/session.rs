//! The per-node TollGate state machine.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use crate::access::AccessLevel;
use crate::action::Action;
use crate::event::Event;
use crate::metering::Counters;
use crate::peer::PeerId;
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
    counters: Counters,
}

impl PeerSession {
    fn new() -> Self {
        Self { phase: PeerPhase::New, balance: 0, counters: Counters::default() }
    }
}

/// Holds one [`PeerSession`] per peer and turns [`Event`]s into [`Action`]s.
/// Pure and synchronous — see the crate-level docs.
#[derive(Debug, Default)]
pub struct Session {
    peers: BTreeMap<PeerId, PeerSession>,
}

impl Session {
    pub fn new() -> Self {
        Self::default()
    }

    /// Drive the state machine with one event. `now` is the host's monotonic
    /// clock; core never reads time on its own.
    ///
    /// This currently implements the bootstrap-only happy path (the first
    /// milestone: an IP deployment that sells access for plain Cashu tokens).
    /// Spilman channels, rollover, and netting are added on top later.
    pub fn handle(&mut self, event: Event, _now: Millis) -> Vec<Action> {
        let mut actions = Vec::new();
        match event {
            Event::PeerConnected { peer } => {
                self.peers.insert(peer, PeerSession::new());
                // New peers start blocked until they pay (access-control default).
                actions.push(Action::SetAccess { peer, level: AccessLevel::None });
            }

            Event::MessageReceived { peer, bytes } => {
                // Skeleton: a real implementation decodes the frame with
                // `tollgate-protocol` and dispatches on the message type. The
                // bootstrap-only path turns a BootstrapPayment (0x06) into a
                // token-verification request.
                // TODO: decode `bytes` and branch on MessageType.
                if let Some(peer_session) = self.peers.get_mut(&peer) {
                    peer_session.phase = PeerPhase::BootstrapPending;
                    actions.push(Action::VerifyBootstrapToken { peer, token: bytes });
                }
            }

            Event::BootstrapVerified { peer, amount, ok } => {
                if let Some(peer_session) = self.peers.get_mut(&peer) {
                    if ok {
                        peer_session.balance = peer_session.balance.saturating_add(amount);
                        peer_session.phase = PeerPhase::Active;
                        actions.push(Action::SetAccess { peer, level: AccessLevel::Active });
                        actions.push(Action::StartMetering { peer });
                    } else {
                        peer_session.phase = PeerPhase::New;
                        actions.push(Action::SetAccess { peer, level: AccessLevel::None });
                    }
                }
            }

            Event::MeterSample { peer, counters } => {
                // TODO: charge the balance from the delta, suspend when it is
                // exhausted, and emit a MeteringReport. The skeleton just records
                // the latest reading.
                if let Some(peer_session) = self.peers.get_mut(&peer) {
                    peer_session.counters = counters;
                }
            }

            Event::PeerDisconnected { peer } => {
                if let Some(peer_session) = self.peers.get_mut(&peer) {
                    peer_session.phase = PeerPhase::Closed;
                }
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;
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
            [Action::SetAccess { level: AccessLevel::None, .. }]
        ));

        let actions =
            session.handle(Event::MessageReceived { peer: p, bytes: vec![0x06] }, Millis(1));
        assert!(matches!(actions.as_slice(), [Action::VerifyBootstrapToken { .. }]));

        let actions =
            session.handle(Event::BootstrapVerified { peer: p, amount: 5000, ok: true }, Millis(2));
        assert!(actions
            .iter()
            .any(|a| matches!(a, Action::SetAccess { level: AccessLevel::Active, .. })));
        assert!(actions.iter().any(|a| matches!(a, Action::StartMetering { .. })));
        assert_eq!(session.peer_count(), 1);
    }

    #[test]
    fn disconnect_forgets_the_peer() {
        let mut session = Session::new();
        let p = peer(2);
        session.handle(Event::PeerConnected { peer: p }, Millis(0));
        let actions = session.handle(Event::PeerDisconnected { peer: p }, Millis(1));
        assert!(actions.iter().any(|a| matches!(a, Action::StopMetering { .. })));
        assert_eq!(session.peer_count(), 0);
    }
}
