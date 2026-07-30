//! The per-peer message lifecycle: events in, actions out.
//!
//! There is no handshake. Peers are already authenticated by the layer
//! underneath, so the sequence starts with Announce and runs symmetrically —
//! each side announces, each side offers, each side funds the channel it will
//! pay on. After that the two payment streams are unsynchronized: each side
//! tops up on its own schedule, for its own windows, and neither waits for the
//! other.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use tollgate_protocol::{
    Accept, Announce, ChannelReady, Disconnect, Message, Offer, PROTOCOL_VERSION, PubKey,
    ReasonCode, Reject, RolloverInit, RolloverReady, TopUpReject,
};

use crate::access::AccessLevel;
use crate::action::Action;
use crate::buyer::{self, BuyerPolicy, Demand, WindowBounds};
use crate::config::{NodePolicy, PeerPolicy};
use crate::event::Event;
use crate::grant::{self, Admission, Verdict};
use crate::session::state::{ChannelSlot, PeerOffer, PeerSession, Phase};
use crate::time::Millis;

/// All peers, and the policy they are served under.
#[derive(Debug)]
pub struct Sessions {
    local: PubKey,
    node: NodePolicy,
    buyer_policy: BuyerPolicy,
    peers: BTreeMap<PubKey, PeerSession>,
    overrides: BTreeMap<PubKey, PeerPolicy>,
}

impl Sessions {
    /// A node with no peers yet.
    pub fn new(local: PubKey, node: NodePolicy, buyer_policy: BuyerPolicy) -> Self {
        Self {
            local,
            node,
            buyer_policy,
            peers: BTreeMap::new(),
            overrides: BTreeMap::new(),
        }
    }

    /// Set an operator override for a peer, before or after it connects.
    pub fn set_peer_policy(&mut self, peer: PubKey, policy: PeerPolicy) {
        self.overrides.insert(peer, policy);
        if let Some(session) = self.peers.get_mut(&peer) {
            session.policy = policy;
        }
    }

    /// Inspect a peer's session.
    pub fn peer(&self, peer: &PubKey) -> Option<&PeerSession> {
        self.peers.get(peer)
    }

    /// Every peer we are tracking.
    pub fn peers(&self) -> impl Iterator<Item = &PeerSession> {
        self.peers.values()
    }

    /// The policy this node serves under.
    pub fn node_policy(&self) -> &NodePolicy {
        &self.node
    }

    /// Feed in something that happened; get back what to do about it.
    pub fn handle(&mut self, event: Event, now: Millis) -> Vec<Action> {
        let mut out = Vec::new();
        match event {
            Event::PeerConnected { peer } => self.on_connected(peer, now, &mut out),
            Event::PeerDisconnected { peer } => {
                self.peers.remove(&peer);
            }
            Event::MessageReceived { peer, msg } => self.on_message(peer, msg, now, &mut out),
            Event::OutgoingChannelFunded {
                peer,
                channel_id,
                capacity,
                funding,
            } => self.on_outgoing_funded(peer, channel_id, capacity, funding, now, &mut out),
            Event::IncomingFundingVerified {
                peer,
                channel_id,
                capacity,
            } => self.on_incoming_verified(peer, channel_id, capacity, now, &mut out),
            Event::IncomingFundingRejected { peer } => {
                self.reject(peer, ReasonCode::FundingInvalid, &mut out);
            }
            Event::Metered { peer, counters } => {
                let multiplier = match self.peers.get(&peer) {
                    Some(s) => s.policy.multiplier(&self.node),
                    None => return out,
                };
                if let Some(session) = self.peers.get_mut(&peer) {
                    let drawn = session.meter.observe(counters, multiplier);
                    session.grant.draw(drawn);
                }
                self.refresh(peer, now, &mut out);
            }
            Event::DemandObserved { peer, rate } => {
                if let Some(session) = self.peers.get_mut(&peer) {
                    session.demand = rate;
                }
                self.poll_buyer(peer, now, &mut out);
            }
            Event::Tick => {
                let peers: Vec<PubKey> = self.peers.keys().copied().collect();
                for peer in peers {
                    if let Some(session) = self.peers.get_mut(&peer) {
                        session.grant.expire_if_due(now);
                    }
                    self.refresh(peer, now, &mut out);
                    self.poll_buyer(peer, now, &mut out);
                    self.poll_rollover(peer, now, &mut out);
                }
            }
        }
        out
    }

    // -----------------------------------------------------------------------
    // Opening
    // -----------------------------------------------------------------------

    fn on_connected(&mut self, peer: PubKey, now: Millis, out: &mut Vec<Action>) {
        let policy = self.overrides.get(&peer).copied().unwrap_or_default();

        if policy.blocked {
            out.push(Action::Send {
                peer,
                msg: Message::Disconnect(Disconnect {
                    reason: ReasonCode::Other,
                }),
            });
            out.push(Action::DropPeer { peer });
            return;
        }

        self.peers.insert(peer, PeerSession::new(peer, policy, now));

        // Blocked until they pay — but the minimum flow allowance still applies
        // as a shaping floor, which is what lets a peer that holds no vouchers
        // yet reach a mint and acquire some.
        out.push(Action::SetAccess {
            peer,
            access: AccessLevel::None,
        });
        out.push(Action::Send {
            peer,
            msg: Message::Announce(Announce {
                version: PROTOCOL_VERSION,
                pubkey: self.local,
                unit: self.node.unit.clone(),
                capabilities: 0,
            }),
        });
        out.push(Action::Send {
            peer,
            msg: self.our_offer(),
        });
        self.refresh(peer, now, out);
    }

    /// What this node advertises: which mints it will take, the unit, the
    /// window range, and one unsigned multiplier. No price anywhere.
    fn our_offer(&self) -> Message {
        Message::Offer(Offer {
            accepted_mints: self.node.accepted_mints.clone(),
            unit: self.node.unit.clone(),
            min_window_ms: self.node.grants.min_window_ms,
            max_window_ms: self.node.grants.max_window_ms,
            received_multiplier: self.node.received_multiplier,
        })
    }

    fn on_message(&mut self, peer: PubKey, msg: Message, now: Millis, out: &mut Vec<Action>) {
        if let Some(session) = self.peers.get_mut(&peer) {
            session.last_seen = now;
        } else {
            return;
        }

        match msg {
            Message::Announce(m) => self.on_announce(peer, m, out),
            Message::Offer(m) => self.on_offer(peer, m, now, out),
            Message::Accept(m) => self.on_accept(peer, m, now, out),
            Message::ChannelReady(m) => {
                // The channel we pay them on is live. Nothing to confirm back —
                // they sent this because they verified our funding.
                if let Some(session) = self.peers.get_mut(&peer) {
                    if let Some(slot) = session.outgoing.as_mut() {
                        slot.id = m.channel_id;
                    }
                }
                self.poll_buyer(peer, now, out);
            }
            Message::TopUp(m) => self.on_topup(peer, m, now, out),
            Message::TopUpReject(m) => {
                if let Some(session) = self.peers.get_mut(&peer) {
                    session.buyer.record_reject(m.cumulative_rejected, m.max_rate_available);
                }
                self.poll_buyer(peer, now, out);
            }
            Message::RolloverInit(m) => {
                // Their outgoing channel is filling up. Verify the replacement
                // funding exactly as we did the first one.
                out.push(Action::VerifyFunding {
                    peer,
                    funding: m.funding,
                });
            }
            Message::RolloverReady(m) => {
                if let Some(session) = self.peers.get_mut(&peer) {
                    if let Some(slot) = session.outgoing.as_mut() {
                        slot.id = m.new_channel_id;
                        slot.rolling_over = false;
                    }
                }
            }
            Message::ChannelClose(m) => {
                out.push(Action::Send {
                    peer,
                    msg: Message::CloseAck(tollgate_protocol::CloseAck {
                        channel_id: m.channel_id,
                        accepted_balance: m.final_balance,
                    }),
                });
                out.push(Action::SettleChannel {
                    peer,
                    channel_id: m.channel_id,
                });
            }
            Message::CloseAck(m) => {
                out.push(Action::SettleChannel {
                    peer,
                    channel_id: m.channel_id,
                });
            }
            Message::Reject(_) => {
                // Nothing to unwind: we never advanced state on a proposal that
                // had not been confirmed.
            }
            Message::Disconnect(_) => {
                if let Some(session) = self.peers.get_mut(&peer) {
                    session.phase = Phase::Closing;
                }
                out.push(Action::DropPeer { peer });
            }
        }
    }

    fn on_announce(&mut self, peer: PubKey, m: Announce, out: &mut Vec<Action>) {
        if m.version != PROTOCOL_VERSION {
            out.push(Action::Send {
                peer,
                msg: Message::Reject(Reject {
                    rejected_type: tollgate_protocol::MsgType::Announce as u8,
                    reason: ReasonCode::VersionUnsupported,
                    text: None,
                }),
            });
            out.push(Action::DropPeer { peer });
            return;
        }

        // A unit mismatch means we are not selling the same thing. There is no
        // conversion to negotiate — the unit is fixed by the resource.
        if m.unit != self.node.unit {
            self.reject(peer, ReasonCode::UnitNotAccepted, out);
        }
    }

    fn on_offer(&mut self, peer: PubKey, m: Offer, now: Millis, out: &mut Vec<Action>) {
        let Some(session) = self.peers.get_mut(&peer) else {
            return;
        };

        // The multiplier may change mid-session; a revised Offer replaces the
        // one we hold and takes effect on our *next* grant. One already bought
        // keeps the multiplier it was bought under.
        let was_established = session.offer.is_some();
        session.offer = Some(PeerOffer {
            accepted_mints: m.accepted_mints.clone(),
            unit: m.unit,
            bounds: WindowBounds {
                min_ms: m.min_window_ms,
                max_ms: m.max_window_ms,
            },
            received_multiplier: m.received_multiplier,
        });
        if session.phase == Phase::Opening {
            session.phase = Phase::Establishing;
        }

        if was_established {
            // A revision, not the opening Offer. Nothing to fund.
            return;
        }

        // Fund the channel we will pay them on, against the earliest mint in
        // their list we can use. Their own mint need not be in it: a
        // pass-through relay may name only its upstream's, so it can spend what
        // it receives without converting.
        let mint = m.accepted_mints.first().cloned();
        match mint {
            Some(mint_url) if self.buyer_policy.max_rate > 0 => {
                out.push(Action::FundChannel {
                    peer,
                    mint_url,
                    capacity: self.node.initial_channel_capacity,
                });
            }
            _ => {
                // We are not buying from this peer. Accept without funding,
                // which is what free peering looks like from this side.
                out.push(Action::Send {
                    peer,
                    msg: Message::Accept(Accept {
                        funding: Vec::new(),
                    }),
                });
            }
        }
        self.refresh(peer, now, out);
    }

    fn on_outgoing_funded(
        &mut self,
        peer: PubKey,
        channel_id: tollgate_protocol::ChannelId,
        capacity: u64,
        funding: Vec<u8>,
        now: Millis,
        out: &mut Vec<Action>,
    ) {
        let Some(session) = self.peers.get_mut(&peer) else {
            return;
        };

        let rolling = session.outgoing.is_some();
        let old = session.outgoing.map(|s| s.id);
        session.outgoing = Some(ChannelSlot {
            id: channel_id,
            capacity,
            rolling_over: rolling,
        });

        out.push(Action::Send {
            peer,
            msg: if let (true, Some(old_channel_id)) = (rolling, old) {
                Message::RolloverInit(RolloverInit {
                    old_channel_id,
                    funding,
                })
            } else {
                Message::Accept(Accept { funding })
            },
        });
        self.refresh(peer, now, out);
    }

    fn on_accept(&mut self, peer: PubKey, m: Accept, now: Millis, out: &mut Vec<Action>) {
        if m.funding.is_empty() {
            // They funded nothing, so they will not be paying us. That is only
            // acceptable if the operator said not to charge them; otherwise
            // they stay blocked and can fund later without reconnecting.
            self.refresh(peer, now, out);
            return;
        }
        out.push(Action::VerifyFunding {
            peer,
            funding: m.funding,
        });
    }

    fn on_incoming_verified(
        &mut self,
        peer: PubKey,
        channel_id: tollgate_protocol::ChannelId,
        capacity: u64,
        now: Millis,
        out: &mut Vec<Action>,
    ) {
        let Some(session) = self.peers.get_mut(&peer) else {
            return;
        };

        let replacing = session.incoming.map(|s| s.id);
        session.incoming = Some(ChannelSlot {
            id: channel_id,
            capacity,
            rolling_over: false,
        });
        session.phase = Phase::Established;

        // The party that verified the funding is the party that will be paid on
        // that channel, so who sent this says which direction it is for and no
        // field has to restate it.
        out.push(Action::Send {
            peer,
            msg: match replacing {
                Some(old_channel_id) => Message::RolloverReady(RolloverReady {
                    old_channel_id,
                    new_channel_id: channel_id,
                }),
                None => Message::ChannelReady(ChannelReady { channel_id }),
            },
        });
        self.refresh(peer, now, out);
    }

    // -----------------------------------------------------------------------
    // Payment
    // -----------------------------------------------------------------------

    fn on_topup(
        &mut self,
        peer: PubKey,
        m: tollgate_protocol::TopUp,
        now: Millis,
        out: &mut Vec<Action>,
    ) {
        // Sum what we have already promised everyone else. Being able to do
        // this at all is what admission control rests on: the payer states the
        // rate it wants up front, for a bounded horizon, so we can refuse
        // before taking the money rather than shaping below what we sold.
        let committed_elsewhere = self.committed_rate_excluding(&peer, now);

        let Some(session) = self.peers.get_mut(&peer) else {
            return;
        };
        let capacity = session.incoming.map(|s| s.capacity).unwrap_or(0);

        let verdict = grant::evaluate_topup(
            &session.grant,
            Admission {
                policy: &self.node.grants,
                committed_elsewhere,
                channel_capacity: capacity,
            },
            m.cumulative,
            m.window_ms,
        );

        match verdict {
            Verdict::Accept { .. } => {
                session.grant.apply(m.cumulative, m.window_ms, now);
            }
            Verdict::Reject {
                reason,
                max_rate_available,
            } => {
                out.push(Action::Send {
                    peer,
                    msg: Message::TopUpReject(TopUpReject {
                        channel_id: m.channel_id,
                        cumulative_rejected: m.cumulative,
                        max_rate_available,
                        reason,
                    }),
                });
            }
        }
        self.refresh(peer, now, out);
        self.poll_rollover(peer, now, out);
    }

    /// Rate committed to every peer but this one, for admission control.
    fn committed_rate_excluding(&self, exclude: &PubKey, now: Millis) -> u64 {
        self.peers
            .iter()
            .filter(|(p, _)| *p != exclude)
            .filter(|(_, s)| s.grant.is_live(now))
            .map(|(_, s)| s.grant.rate())
            .fold(0u64, u64::saturating_add)
    }

    /// Ask the buyer whether to top up, and turn a decision into a signing
    /// request. Core holds no keys, so the host signs and sends.
    fn poll_buyer(&mut self, peer: PubKey, now: Millis, out: &mut Vec<Action>) {
        let Some(session) = self.peers.get_mut(&peer) else {
            return;
        };
        let (Some(slot), Some(offer)) = (session.outgoing, session.offer.as_ref()) else {
            return;
        };

        let demand = Demand {
            observed_rate: session.demand,
            bounds: offer.bounds,
        };
        let Some(purchase) = buyer::poll(&session.buyer, &self.buyer_policy, demand, now) else {
            return;
        };

        // Do not sign past what the channel can carry — it could not be settled.
        // The rollover poll below opens the replacement.
        if purchase.cumulative > slot.capacity {
            return;
        }

        session.buyer.record(purchase, now);
        out.push(Action::SignAndSendTopUp {
            peer,
            channel_id: slot.id,
            cumulative: purchase.cumulative,
            window_ms: purchase.window_ms,
        });
    }

    /// Open a replacement channel when the one we fund approaches exhaustion.
    ///
    /// Only for the direction we fund: rollover is initiated by the funder
    /// alone, since only the party putting up new funds decides when.
    fn poll_rollover(&mut self, peer: PubKey, _now: Millis, out: &mut Vec<Action>) {
        let Some(session) = self.peers.get(&peer) else {
            return;
        };
        let threshold = self.node.rollover_threshold_pct;
        if !session.needs_rollover(session.outgoing, session.buyer.cumulative(), threshold) {
            return;
        }
        let Some(offer) = session.offer.as_ref() else {
            return;
        };
        let Some(mint_url) = offer.accepted_mints.first().cloned() else {
            return;
        };

        if let Some(session) = self.peers.get_mut(&peer) {
            if let Some(slot) = session.outgoing.as_mut() {
                slot.rolling_over = true;
            }
        }
        out.push(Action::FundChannel {
            peer,
            mint_url,
            capacity: self.node.initial_channel_capacity,
        });
    }

    // -----------------------------------------------------------------------
    // Gate and shaper
    // -----------------------------------------------------------------------

    /// Recompute a peer's access level and shaping rate, emitting an action
    /// only where something actually changed.
    fn refresh(&mut self, peer: PubKey, now: Millis, out: &mut Vec<Action>) {
        let minimum_flow = self.node.minimum_flow;
        let Some(session) = self.peers.get_mut(&peer) else {
            return;
        };

        let access = if session.policy.no_charge {
            AccessLevel::Free
        } else if session.incoming.is_some() {
            // Exhausted channel with no rollover under way: blocked, but still
            // able to negotiate, so it recovers without reconnecting.
            let exhausted = session
                .incoming
                .map(|s| session.grant.authorized() >= s.capacity && !s.rolling_over)
                .unwrap_or(false);
            if exhausted {
                AccessLevel::Suspended
            } else {
                AccessLevel::Active
            }
        } else {
            AccessLevel::None
        };

        if access != session.access {
            session.access = access;
            out.push(Action::SetAccess { peer, access });
        }

        let rate = if access == AccessLevel::Free {
            // Unmetered: no grant exists in either direction.
            u64::MAX
        } else {
            session.grant.shaping_rate(now, minimum_flow)
        };

        if session.applied_rate != Some(rate) {
            session.applied_rate = Some(rate);
            out.push(Action::SetShapingRate { peer, rate });
        }
    }

    fn reject(&mut self, peer: PubKey, reason: ReasonCode, out: &mut Vec<Action>) {
        out.push(Action::Send {
            peer,
            msg: Message::Disconnect(Disconnect { reason }),
        });
        out.push(Action::DropPeer { peer });
    }
}
