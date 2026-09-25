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
    ReasonCode, RefusedUpdate, Reject, RolloverInit, RolloverReady, TopUpReject,
};

use crate::access::AccessLevel;
use crate::action::Action;
use crate::buyer::{self, BuyerPolicy, Demand, WindowBounds};
use crate::config::{NodePolicy, PeerPolicy};
use crate::event::Event;
use crate::grant::{self, Admission, Verdict};
use crate::session::state::{PeerOffer, PeerSession, Phase};
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

    /// Wind the node down: tell every peer we are going, and settle what we can.
    ///
    /// A bare FIN is treated as an unclean disconnect and triggers the same
    /// cleanup as a timeout, so saying so first is the difference between a peer
    /// tearing our state down on a timer and doing it at once.
    pub fn shutdown(&mut self) -> Vec<Action> {
        let mut out = Vec::new();
        for (peer, session) in &mut self.peers {
            session.phase = Phase::Closing;

            // Every channel a peer paid us on holds value we can still claim.
            // Settling is the point at which our own spent-proof set stops
            // growing, so it is worth doing before we go.
            for channel in session.grant.channels() {
                out.push(Action::SettleChannel {
                    peer: *peer,
                    channel_id: channel.id,
                });
            }

            out.push(Action::Send {
                peer: *peer,
                msg: Message::Disconnect(Disconnect {
                    reason: ReasonCode::Other,
                }),
            });
        }
        out
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
            Event::TopUpSignatureInvalid { peer, channel_id } => {
                // Still something heard from them, even if it bought nothing.
                if let Some(session) = self.peers.get_mut(&peer) {
                    session.last_seen = now;
                }
                self.on_unverified_topup(peer, &[channel_id], now, &mut out);
            }
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
                mint_url,
            } => {
                // A channel is funded in a mint we list, or not at all: the
                // mint is the credit risk we took on deliberately. Checked here
                // rather than trusted to the backend, so it holds for every one.
                if self.node.accepted_mints.contains(&mint_url) {
                    self.on_incoming_verified(peer, channel_id, capacity, now, &mut out);
                } else {
                    self.reject(peer, ReasonCode::MintNotAccepted, &mut out);
                }
            }
            Event::IncomingFundingRejected { peer, reason } => {
                self.reject(peer, reason, &mut out);
            }
            Event::Metered { peer, counters } => {
                let multiplier = match self.peers.get(&peer) {
                    Some(s) => s.policy.multiplier(&self.node),
                    None => return out,
                };
                if let Some(session) = self.peers.get_mut(&peer) {
                    // What we have been pushing at them, as a rate. Read before
                    // `observe` consumes the reading.
                    let grew = counters.delta_since(session.meter.totals());
                    let elapsed = now.saturating_since(session.last_meter_at);
                    if elapsed > 0 {
                        session.upload_rate = crate::grant::rate_from(
                            grew.delivered,
                            elapsed.min(u32::MAX as u64) as u32,
                        );
                        session.last_meter_at = now;
                    }

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
                    // A peer that has said nothing at all for this long is
                    // gone. Nothing needs to detect a peer that merely stops
                    // *paying* — its grant lapses and it drops to the minimum
                    // flow allowance — so this only catches silence.
                    if self.node.stale_timeout_ms > 0
                        && let Some(session) = self.peers.get(&peer)
                        && now.saturating_since(session.last_seen) > self.node.stale_timeout_ms
                    {
                        self.peers.remove(&peer);
                        out.push(Action::DropPeer { peer });
                        continue;
                    }
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
            msg: self.our_offer(&policy),
        });
        self.refresh(peer, now, out);
    }

    /// What this node advertises to one peer: which mints it will take, the
    /// unit, the window range, one unsigned multiplier, and whether we charge
    /// it at all. No price anywhere.
    ///
    /// The multiplier is the one that peer's grant will be drawn down under,
    /// override included. The payer sizes its purchases from it, so
    /// advertising the node-wide value to a peer we surcharge harder would
    /// have it under-buy and be shaped below what it needs.
    ///
    /// Not charging is our decision alone, but the peer has to hear it —
    /// otherwise its buyer funds a channel and tops up toward a node that was
    /// never going to meter it.
    fn our_offer(&self, policy: &PeerPolicy) -> Message {
        Message::Offer(Offer {
            accepted_mints: self.node.accepted_mints.clone(),
            unit: self.node.unit.clone(),
            min_window_ms: self.node.grants.min_window_ms,
            max_window_ms: self.node.grants.max_window_ms,
            received_multiplier: policy.multiplier(&self.node),
            no_charge: policy.no_charge,
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
                    session.buyer.confirmed(m.channel_id);
                }
                self.poll_buyer(peer, now, out);
            }
            Message::TopUp(m) => self.on_topup(peer, m, now, out),
            Message::TopUpReject(m) => {
                if let Some(session) = self.peers.get_mut(&peer) {
                    let refused: Vec<(tollgate_protocol::ChannelId, u64)> = m
                        .refused
                        .iter()
                        .map(|r| (r.channel_id, r.cumulative))
                        .collect();
                    let hold = self.buyer_policy.cap_hold_ms;
                    session
                        .buyer
                        .record_reject(&refused, m.max_rate_available, now, hold);
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
                // The replacement is open. It waits behind the channel in use
                // rather than displacing it: the old one drains to its capacity
                // first, and a purchase that overflows is signed across both.
                if let Some(session) = self.peers.get_mut(&peer) {
                    session.buyer.confirmed(m.new_channel_id);
                }
                self.poll_buyer(peer, now, out);
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
            no_charge: m.no_charge,
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
        //
        // Not if they will not charge us: there is nothing to buy, so no
        // channel, and the empty Accept tells them so.
        let mint = m.accepted_mints.first().cloned();
        match mint {
            Some(mint_url) if !m.no_charge && self.buyer_policy.max_rate > 0 => {
                out.push(Action::FundChannel {
                    peer,
                    mint_url,
                    capacity: self.node.initial_channel_capacity,
                });
            }
            _ => {
                // We are not buying from this peer — it does not charge us,
                // or we buy nothing at all. Accept without funding, which is
                // what free peering looks like from this side.
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

        // Nothing is signed on it until the peer confirms it verified the
        // funding — a grant signed here could otherwise be against a channel
        // that never opens.
        let replacing = session.buyer.active().map(|c| c.id);
        session.buyer.funded(channel_id, capacity);

        out.push(Action::Send {
            peer,
            msg: match replacing {
                Some(old_channel_id) => Message::RolloverInit(RolloverInit {
                    old_channel_id,
                    funding,
                }),
                None => Message::Accept(Accept { funding }),
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

        // Recognised alongside whatever is already open rather than replacing
        // it: the peer drains the old one to its capacity, and a purchase that
        // spans the boundary ratchets both in one message.
        let replacing = session
            .grant
            .channels()
            .iter()
            .find(|c| c.id != channel_id)
            .map(|c| c.id);
        session.grant.open_channel(channel_id, capacity);
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

        let verdict = grant::evaluate_topup(
            &session.grant,
            Admission {
                policy: &self.node.grants,
                committed_elsewhere,
            },
            &m.updates,
            m.window_ms,
        );

        match verdict {
            Verdict::Accept {
                ratchets, grant, ..
            } => {
                session.grant.apply(&ratchets, grant, m.window_ms, now);

                // A channel drained to its capacity carries nothing further.
                // Settling it is what keeps our spent-proof set bounded, which
                // is the reason channels exist at all.
                let done: Vec<_> = session.grant.exhausted_channels().collect();
                for channel_id in done {
                    session.grant.close_channel(channel_id);
                    out.push(Action::SettleChannel { peer, channel_id });
                }
            }
            Verdict::Reject {
                reason: ReasonCode::GrantInvalid,
                ..
            } => {
                // Not a purchase we decline but one that failed verification —
                // the total did not increase. That is answered with Reject, and
                // counted against the channel it named.
                let failed = not_increasing(&session.grant, &m.updates);
                self.on_unverified_topup(peer, &failed, now, out);
                return;
            }
            Verdict::Reject {
                reason,
                max_rate_available,
            } => {
                // Echoed in full: a purchase may span several channels, so one
                // channel id no longer identifies which one was refused.
                out.push(Action::Send {
                    peer,
                    msg: Message::TopUpReject(TopUpReject {
                        refused: m
                            .updates
                            .iter()
                            .map(|u| RefusedUpdate {
                                channel_id: u.channel_id,
                                cumulative: u.cumulative,
                            })
                            .collect(),
                        max_rate_available,
                        reason,
                    }),
                });
            }
        }
        self.refresh(peer, now, out);
    }

    /// Answer a TopUp that failed verification: a signature the host could not
    /// verify, or a total that did not increase.
    ///
    /// The payer is told with a Reject, since either can be transient — a
    /// reordered message, or a payer that lost track of its own total — and
    /// the channel stays open. Failures are counted per channel, though, and a
    /// channel that fails [`MAX_VERIFICATION_FAILURES`] times in a row is
    /// closed: we stop recognising it and settle the last state that did
    /// verify, so nothing it had already paid for is lost.
    ///
    /// [`MAX_VERIFICATION_FAILURES`]: crate::grant::MAX_VERIFICATION_FAILURES
    fn on_unverified_topup(
        &mut self,
        peer: PubKey,
        channels: &[tollgate_protocol::ChannelId],
        now: Millis,
        out: &mut Vec<Action>,
    ) {
        let Some(session) = self.peers.get_mut(&peer) else {
            return;
        };

        out.push(Action::Send {
            peer,
            msg: Message::Reject(Reject {
                rejected_type: tollgate_protocol::MsgType::TopUp as u8,
                reason: ReasonCode::GrantInvalid,
                text: None,
            }),
        });

        for &channel_id in channels {
            let failures = session.grant.record_failure(channel_id);
            if failures.is_some_and(|n| n >= grant::MAX_VERIFICATION_FAILURES) {
                session.grant.close_channel(channel_id);
                out.push(Action::SettleChannel { peer, channel_id });
            }
        }
        self.refresh(peer, now, out);
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

    /// Ask the buyer whether to top up, and turn a decision into signing
    /// requests. Core holds no keys, so the host signs and sends.
    ///
    /// A purchase that overflows the channel in use produces two: a cumulative
    /// total only means anything against the channel it was signed on, so the
    /// old channel is topped to exactly its capacity and the remainder starts
    /// the replacement.
    fn poll_buyer(&mut self, peer: PubKey, now: Millis, out: &mut Vec<Action>) {
        let Some(session) = self.peers.get_mut(&peer) else {
            return;
        };
        let Some(offer) = session.offer.as_ref() else {
            return;
        };
        if offer.no_charge {
            // Nothing to buy, and no channel to sign against.
            return;
        }

        // A unit we download draws one from our grant; a unit we upload draws
        // the peer's multiplier. Buying for the download alone would leave us
        // shaped for the difference, which is exactly what the surcharge is for.
        let surcharged = session
            .upload_rate
            .saturating_mul(offer.received_multiplier as u64);
        let demand = Demand {
            observed_rate: session.demand.saturating_add(surcharged),
            bounds: offer.bounds,
        };
        let Some(purchase) = buyer::poll(&session.buyer, &self.buyer_policy, demand, now) else {
            return;
        };

        let retired = session.buyer.record(purchase, now);

        // One message, however many channels it draws from: the grant is their
        // combined increase, and splitting it would leave the provider unable
        // to tell one purchase from two.
        out.push(Action::SignAndSendTopUp {
            peer,
            ratchets: [Some(purchase.first), purchase.second]
                .into_iter()
                .flatten()
                .map(|leg| (leg.channel_id, leg.cumulative))
                .collect(),
            window_ms: purchase.window_ms,
        });

        // A channel drained to its capacity has nothing left to carry, and its
        // replacement is already holding the overflow. Settling it now is what
        // keeps the issuer's spent-proof set bounded, which is the whole reason
        // channels exist.
        if let Some(channel_id) = retired {
            out.push(Action::SettleChannel { peer, channel_id });
        }
    }

    /// Open a replacement channel when the one we fund approaches exhaustion.
    ///
    /// Only for the direction we fund: rollover is initiated by the funder
    /// alone, since only the party putting up new funds decides when.
    fn poll_rollover(&mut self, peer: PubKey, _now: Millis, out: &mut Vec<Action>) {
        let Some(session) = self.peers.get(&peer) else {
            return;
        };
        if !session
            .buyer
            .needs_rollover(self.node.rollover_threshold_pct)
        {
            return;
        }
        let Some(offer) = session.offer.as_ref() else {
            return;
        };
        let Some(mint_url) = offer.accepted_mints.first().cloned() else {
            return;
        };

        // `needs_rollover` stays false from here until the peer confirms, so
        // this cannot fire again and fund a channel on every tick.
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
        } else if !session.grant.channels().is_empty() || session.grant.is_live(now) {
            // Paying, or still delivering what was paid for after the last
            // channel filled — the session lasts until that grant runs out.
            AccessLevel::Active
        } else {
            // Never paid, or every channel it paid us on has been drained and
            // settled with nothing replacing it. Either way there is no session
            // — only the allowance — and the peer can still negotiate one
            // without reconnecting.
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

/// The channels a refused purchase failed to ratchet: those whose total did not
/// increase, including a channel named a second time, whose second reading
/// would be counted from the wrong base.
///
/// Only channels we recognise are named — one we do not has no ratchet for the
/// total to fail against.
fn not_increasing(
    grant: &grant::GrantState,
    updates: &[tollgate_protocol::ChannelUpdate],
) -> Vec<tollgate_protocol::ChannelId> {
    let mut seen = Vec::with_capacity(updates.len());
    let mut failed = Vec::new();
    for update in updates {
        let Some(channel) = grant.channel(update.channel_id) else {
            continue;
        };
        let repeated = seen.contains(&update.channel_id);
        seen.push(update.channel_id);
        if (repeated || update.cumulative <= channel.signed) && !failed.contains(&channel.id) {
            failed.push(channel.id);
        }
    }
    failed
}
