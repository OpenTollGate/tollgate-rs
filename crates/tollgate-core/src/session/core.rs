//! The per-peer message lifecycle: events in, actions out.
//!
//! There is no handshake. Peers are already authenticated by the layer
//! underneath, so the sequence starts with Announce and runs symmetrically —
//! each side announces, each side offers, each side funds the channel it will
//! pay on. After that the two payment streams are unsynchronized: each side
//! tops up on its own schedule, for its own windows, and neither waits for the
//! other.
//!
//! A peer that drops without saying Disconnect is held for a while rather than
//! forgotten. If it comes back in time, the new session picks up the channels
//! the old one left — both sides still hold them, so there is nothing to fund.
//!
//! Silence is what marks a peer gone, so no live peer is left silent: a node
//! that has sent a peer nothing for a third of the stale timeout sends it the
//! Offer it last sent, unchanged. That matters most for a payer the provider never charges,
//! which otherwise hears nothing back after setup — TopUps are not answered.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use tollgate_protocol::{
    Accept, Announce, Balance, ChannelReady, Disconnect, Message, Offer, PROTOCOL_VERSION, PubKey,
    ReasonCode, RefusedUpdate, Reject, RolloverInit, RolloverReady, TopUp, TopUpReject,
};

use crate::access::AccessLevel;
use crate::action::Action;
use crate::buyer::{self, Buyer, BuyerPolicy, Demand, RolloverReason, Terms};
use crate::config::{NodePolicy, PeerPolicy};
use crate::event::Event;
use crate::grant::{self, Admission, Budget, Verdict};
use crate::session::state::{PeerOffer, PeerSession, Phase};
use crate::time::Millis;

/// All peers, and the policy they are served under.
#[derive(Debug)]
pub struct Sessions {
    local: PubKey,
    node: NodePolicy,
    buyer_policy: BuyerPolicy,
    peers: BTreeMap<PubKey, PeerSession>,
    /// Peers that went away uncleanly, held so they can resume their channels
    /// if they come back within the grace period.
    parked: BTreeMap<PubKey, Parked>,
    overrides: BTreeMap<PubKey, PeerPolicy>,
    /// The id the next funding request goes out under. One counter for every
    /// peer, so an id is never reused while the node runs.
    next_request: u64,
}

/// A session whose transport went away without a Disconnect.
#[derive(Debug)]
struct Parked {
    session: PeerSession,
    /// When it went.
    since: Millis,
}

impl Sessions {
    /// A node with no peers yet.
    pub fn new(local: PubKey, node: NodePolicy, buyer_policy: BuyerPolicy) -> Self {
        Self {
            local,
            node,
            buyer_policy,
            peers: BTreeMap::new(),
            parked: BTreeMap::new(),
            overrides: BTreeMap::new(),
            next_request: 1,
        }
    }

    /// Set an operator override for a peer, before or after it connects.
    ///
    /// For a peer already connected it takes effect at once: its access and
    /// speed are recomputed, and if the Offer it would now get differs from
    /// the one it last got — a change to whether we charge it — the revision
    /// is sent in what this returns. A changed from-payer weight is not part
    /// of that: the weight is fixed for a session, and the peer gets the new
    /// one in the Offer of its next. Before it connects there is nothing to
    /// do, and nothing is returned.
    ///
    /// Sent here rather than left for the keepalive, which only repeats the
    /// last Offer: an override is the operator's decision, and should reach
    /// the peer whether or not the link happens to be quiet.
    pub fn set_peer_policy(
        &mut self,
        peer: PubKey,
        policy: PeerPolicy,
        now: Millis,
    ) -> Vec<Action> {
        let mut out = Vec::new();
        self.overrides.insert(peer, policy);
        let Some(session) = self.peers.get_mut(&peer) else {
            return out;
        };
        session.policy = policy;
        if session.phase == Phase::Closing {
            return out;
        }
        // Only a peer that has had an Offer is owed a revision. One that has
        // not gets its first with the new policy in it.
        let weight = session.weight;
        let revised = self.offer_for(&policy, weight);
        if let Some(session) = self.peers.get(&peer)
            && session.offer_sent.is_some()
            && session.offer_sent.as_ref() != Some(&revised)
        {
            self.send_offer(peer, revised, &mut out);
        }
        self.refresh(peer, now, &mut out);
        self.note_sent(&out, now);
        out
    }

    /// Inspect a peer's session.
    pub fn peer(&self, peer: &PubKey) -> Option<&PeerSession> {
        self.peers.get(peer)
    }

    /// A peer that went away uncleanly and whose channels are being held in
    /// case it comes back.
    pub fn parked(&self, peer: &PubKey) -> Option<&PeerSession> {
        self.parked.get(peer).map(|p| &p.session)
    }

    /// How long a peer that went away uncleanly is held for.
    ///
    /// The stale timeout, because a bare FIN is treated as an unclean
    /// disconnect and gets the same cleanup as a timeout — so it gets it on the
    /// same clock. Zero, which switches the timeout off, holds nothing: holding
    /// state forever for a peer that never returns is a leak.
    fn resume_grace_ms(&self) -> u64 {
        self.node.stale_timeout_ms
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
    pub fn shutdown(&mut self, now: Millis) -> Vec<Action> {
        let mut out = Vec::new();
        for (peer, session) in &mut self.peers {
            session.phase = Phase::Closing;

            // The budget outlives this node's run: kept on disk, it is there
            // for the payer when we come back.
            Self::save_budget(*peer, session, now, &mut out);

            // Every channel a peer paid us on holds value we can still claim.
            // Settling is the point at which our own spent-proof set stops
            // growing, so it is worth doing before we go.
            Self::discard(*peer, session, &mut out);

            out.push(Action::Send {
                peer: *peer,
                msg: Message::Disconnect(Disconnect {
                    reason: ReasonCode::Other,
                }),
            });
        }

        // A peer held after an unclean disconnect will not find us when it
        // comes back, so what it paid us is claimed now or not at all.
        for (peer, mut parked) in core::mem::take(&mut self.parked) {
            Self::save_budget(peer, &parked.session, now, &mut out);
            Self::discard(peer, &mut parked.session, &mut out);
        }
        out
    }

    /// Whether a TopUp from `peer` arriving at `now` comes sooner after its
    /// last than our gap between purchases allows, and if so the refusal to
    /// send it.
    ///
    /// For the host to ask **before it verifies any signature** on the
    /// TopUp: the gap exists to bound how many signature checks a payer can
    /// cause, so it has to be checked before any are made. A TopUp refused
    /// here goes no further and does not move the time of the payer's last
    /// one. Asking changes nothing; core checks the gap again when the TopUp
    /// reaches it, against the same time.
    pub fn too_soon(&self, peer: PubKey, topup: &TopUp, now: Millis) -> Option<Action> {
        let session = self.peers.get(&peer)?;
        if !session
            .grant
            .too_soon(now, self.node.grants.min_topup_gap_ms)
        {
            return None;
        }
        Some(Action::Send {
            peer,
            msg: Message::TopUpReject(TopUpReject {
                refused: refused_updates(topup),
                max_reserved_rate: self.admission(&peer).rate_available(),
                reason: ReasonCode::TooSoon,
            }),
        })
    }

    /// End whatever this node still holds for `peer`, live or held after an
    /// unclean disconnect: its budget is kept, its incoming channels settled,
    /// and nothing is left to resume.
    ///
    /// For a host that sees a peer come back as a different payer — under
    /// `enforcer.identity: address`, the same key from another address. The
    /// budget and channels belong to the address it left from, so the new
    /// connection must start from nothing rather than resume them.
    pub fn forget(&mut self, peer: PubKey, now: Millis) -> Vec<Action> {
        let mut out = Vec::new();
        if let Some(mut session) = self.peers.remove(&peer) {
            Self::save_budget(peer, &session, now, &mut out);
            Self::discard(peer, &mut session, &mut out);
        }
        if let Some(mut parked) = self.parked.remove(&peer) {
            Self::save_budget(peer, &parked.session, now, &mut out);
            Self::discard(peer, &mut parked.session, &mut out);
        }
        out
    }

    /// Feed in something that happened; get back what to do about it.
    pub fn handle(&mut self, event: Event, now: Millis) -> Vec<Action> {
        let mut out = Vec::new();
        let tick = matches!(event, Event::Tick);
        match event {
            Event::PeerConnected { peer, budget } => self.on_connected(peer, budget, now, &mut out),
            Event::PeerDisconnected { peer } => self.on_disconnected(peer, now, &mut out),
            Event::MessageReceived { peer, msg } => self.on_message(peer, msg, now, &mut out),
            Event::TopUpSignatureInvalid { peer, channel_id } => {
                // Still something heard from them, even if it bought nothing.
                // Its signatures were checked, so it counts toward the gap.
                if let Some(session) = self.peers.get_mut(&peer) {
                    session.last_seen = now;
                    session.grant.topup_checked(now);
                }
                self.on_unverified_topup(peer, &[channel_id], now, &mut out);
            }
            Event::OutgoingChannelFunded {
                peer,
                request,
                channel_id,
                capacity,
                expires_at,
                funding,
            } => {
                let Some(session) = self
                    .peers
                    .get_mut(&peer)
                    .filter(|s| s.buyer.answers(request))
                else {
                    // Superseded: another channel came back first, or the
                    // peer is gone. Taking this one as well would open a
                    // second channel on top of the first, or overwrite one
                    // the peer is about to confirm and strand it. Nothing has
                    // been sent about it, so it is simply not used — and its
                    // funds are handed back to the host.
                    out.push(Action::ReclaimChannel {
                        peer,
                        channel_id,
                        capacity,
                        expires_at,
                    });
                    return out;
                };
                // Nothing is signed on it until the peer confirms it
                // verified the funding — a grant signed here could otherwise
                // be against a channel that never opens.
                session.buyer.funded(channel_id, capacity, expires_at);
                self.on_outgoing_funded(peer, funding, now, &mut out)
            }
            Event::OutgoingFundingFailed { peer, request } => {
                // Nothing was funded, so nothing is sent. The request is
                // cleared, and a channel still wanted — the first one, or a
                // rollover still due — is asked for again on the next tick.
                if let Some(session) = self.peers.get_mut(&peer) {
                    session.buyer.funding_failed(request);
                }
            }
            Event::IncomingFundingVerified {
                peer,
                channel_id,
                capacity,
                expires_at,
                mint_url,
            } => {
                // A channel is funded in a mint we list, or not at all: the
                // mint is the credit risk we took on deliberately. Checked here
                // rather than trusted to the backend, so it holds for every one.
                if self.node.accepted_mints.contains(&mint_url) {
                    self.on_incoming_verified(
                        peer, channel_id, capacity, expires_at, now, &mut out,
                    );
                } else {
                    self.reject(peer, ReasonCode::MintNotAccepted, &mut out);
                }
            }
            Event::IncomingFundingRejected { peer, reason } => {
                self.reject(peer, reason, &mut out);
            }
            Event::Metered {
                peer,
                counters,
                carried,
            } => {
                self.on_metered(peer, counters, carried, now);
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
                    // *paying* — its budget runs out and it drops to the minimum
                    // flow allowance — so this only catches silence.
                    if self.node.stale_timeout_ms > 0
                        && let Some(session) = self.peers.get(&peer)
                        && now.saturating_since(session.last_seen) > self.node.stale_timeout_ms
                    {
                        // Silence is an unclean disconnect too: a link that
                        // died without a FIN looks exactly like this.
                        self.park(peer, now, &mut out);
                        out.push(Action::DropPeer { peer });
                        continue;
                    }
                    if let Some(session) = self.peers.get_mut(&peer) {
                        session.grant.expire_if_due(now);
                    }
                    self.settle_expiring(peer, now, &mut out);
                    self.refresh(peer, now, &mut out);
                    self.poll_buyer(peer, now, &mut out);
                    self.poll_rollover(peer, now, &mut out);
                    self.poll_opening(peer, now, &mut out);
                }
                self.expire_parked(now, &mut out);
            }
        }
        self.note_sent(&out, now);
        if tick {
            self.keep_alive(now, &mut out);
        }
        out
    }

    // -----------------------------------------------------------------------
    // Keeping a quiet link alive
    // -----------------------------------------------------------------------

    /// Remember when we last sent each peer anything, from the actions about
    /// to be carried out.
    fn note_sent(&mut self, out: &[Action], now: Millis) {
        for action in out {
            let (Action::Send { peer, .. } | Action::SignAndSendTopUp { peer, .. }) = action else {
                continue;
            };
            if let Some(session) = self.peers.get_mut(peer) {
                session.last_sent = now;
            }
        }
    }

    /// Send our Offer again to every peer we have sent nothing for a while.
    ///
    /// The peer drops us after `stale_timeout` of silence, and the payment
    /// streams do not guarantee it hears from us: a TopUp is never answered,
    /// so a provider that does not buy from its payer — one that payer asked
    /// not to charge — says nothing at all after setup, and the payer would
    /// drop a healthy session every minute.
    ///
    /// The Offer, because it is the one message every node already takes again
    /// at any time and to no effect: a revision that changes nothing. So a node
    /// that predates this still counts it as hearing from us.
    ///
    /// The Offer last sent, byte for byte, and not one rebuilt from the policy
    /// in force: an override made since is sent when it is made, by
    /// [`Self::set_peer_policy`], and a keepalive must never be the thing that
    /// changes a peer's terms.
    fn keep_alive(&mut self, now: Millis, out: &mut Vec<Action>) {
        let interval = self.node.keepalive_interval_ms();
        for (peer, session) in &mut self.peers {
            if session.phase == Phase::Closing || now.saturating_since(session.last_sent) < interval
            {
                continue;
            }
            // Every live session was sent an Offer when it connected.
            let Some(offer) = session.offer_sent.clone() else {
                continue;
            };
            out.push(Action::Send {
                peer: *peer,
                msg: Message::Offer(offer),
            });
            session.last_sent = now;
        }
    }

    // -----------------------------------------------------------------------
    // Going away and coming back
    // -----------------------------------------------------------------------

    /// The transport went away.
    ///
    /// After a Disconnect, sent or received, the session is over: it goes, and
    /// what the peer paid us is settled. Without one it is held: a Wi-Fi blip
    /// should not cost both sides a new channel each.
    fn on_disconnected(&mut self, peer: PubKey, now: Millis, out: &mut Vec<Action>) {
        let Some(session) = self.peers.get(&peer) else {
            return;
        };
        if session.phase == Phase::Closing {
            if let Some(mut session) = self.peers.remove(&peer) {
                Self::save_budget(peer, &session, now, out);
                Self::discard(peer, &mut session, out);
            }
            return;
        }
        self.park(peer, now, out);
    }

    /// Hold a live session for the grace period, or let it go if there is none.
    fn park(&mut self, peer: PubKey, now: Millis, out: &mut Vec<Action>) {
        let Some(mut session) = self.peers.remove(&peer) else {
            return;
        };
        // The session is over, if not yet given up on: nothing is drawn while
        // it is down, no capacity is set aside for it, and its budget is kept
        // on disk in case it is this node that goes next.
        session.grant.end_reservation();
        session.drawn_at = None;
        Self::save_budget(peer, &session, now, out);
        if self.resume_grace_ms() == 0 {
            Self::discard(peer, &mut session, out);
            return;
        }
        self.parked.insert(
            peer,
            Parked {
                session,
                since: now,
            },
        );
    }

    /// Let go of every held session whose peer did not come back in time.
    fn expire_parked(&mut self, now: Millis, out: &mut Vec<Action>) {
        let grace = self.resume_grace_ms();
        let expired: Vec<PubKey> = self
            .parked
            .iter()
            .filter(|(_, p)| now.saturating_since(p.since) > grace)
            .map(|(peer, _)| *peer)
            .collect();
        for peer in expired {
            if let Some(mut parked) = self.parked.remove(&peer) {
                Self::discard(peer, &mut parked.session, out);
            }
        }

        // One still inside its grace keeps its channels, but not past the
        // point where the peer could take them back through the refund path:
        // a grace period longer than what is left of a channel would otherwise
        // hand the peer what it already paid us on it. Settled here, the
        // channel is no longer held, so a resume does not affirm it and the
        // peer funds afresh.
        let lead_ms = self.settle_lead_ms();
        for (peer, parked) in &mut self.parked {
            Self::settle_expiring_in(*peer, &mut parked.session, now, lead_ms, out);
        }
    }

    /// Give up on a session for good.
    ///
    /// Every channel the peer paid us on holds value we can still claim, and
    /// nobody will turn its ratchet again, so it is settled now rather than
    /// lost with the state — and closed, so it is settled once.
    fn discard(peer: PubKey, session: &mut PeerSession, out: &mut Vec<Action>) {
        let settled: Vec<_> = session.grant.channels().iter().map(|c| c.id).collect();
        for channel_id in settled {
            let expires_at = session
                .grant
                .close_channel(channel_id)
                .and_then(|c| c.expires_at);
            out.push(Action::SettleChannel {
                peer,
                channel_id,
                expires_at,
            });
        }
    }

    /// Have the host keep a payer's budget, as it stands at `now`.
    ///
    /// Only for a peer we charge: one we carry free has no budget.
    fn save_budget(peer: PubKey, session: &PeerSession, now: Millis, out: &mut Vec<Action>) {
        if session.policy.no_charge {
            return;
        }
        out.push(Action::SaveBudget {
            peer,
            budget: session.grant.budget(now),
        });
    }

    // -----------------------------------------------------------------------
    // Opening
    // -----------------------------------------------------------------------

    fn on_connected(
        &mut self,
        peer: PubKey,
        budget: Option<Budget>,
        now: Millis,
        out: &mut Vec<Action>,
    ) {
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

        // A new connection is a fresh session with a fresh Announce. If we still
        // hold state for this key — the old transport died without a
        // Disconnect, or has not been noticed dying yet — the session starts
        // over those channels instead of none.
        let kept = match self.peers.remove(&peer) {
            Some(session) if session.phase != Phase::Closing => Some(session),
            closing => {
                // One that was ending is over; the new connection does not
                // revive it, but what the peer paid on it is still ours.
                if let Some(mut session) = closing {
                    Self::save_budget(peer, &session, now, out);
                    Self::discard(peer, &mut session, out);
                }
                self.parked.remove(&peer).and_then(|mut p| {
                    if now.saturating_since(p.since) <= self.resume_grace_ms() {
                        Some(p.session)
                    } else {
                        Self::discard(peer, &mut p.session, out);
                        None
                    }
                })
            }
        };
        let mut session = match kept {
            Some(mut session) => {
                // What we still hold of its budget is at least as current as
                // what the host kept on disk.
                session.resume(policy, now);
                // A channel already due to be settled is not one to carry on
                // with: settle it now rather than affirm it and settle it on
                // the next tick.
                let lead_ms = self.settle_lead_ms();
                Self::settle_expiring_in(peer, &mut session, now, lead_ms, out);
                session
            }
            None => {
                let mut session = PeerSession::new(peer, policy, now);
                if let Some(budget) = budget {
                    session.grant.restore(budget, now);
                }
                session
            }
        };
        // Fixed for the session: an override made since applies from the
        // next.
        session.weight = policy.weight(&self.node);
        let held: Vec<tollgate_protocol::ChannelId> =
            session.grant.channels().iter().map(|c| c.id).collect();
        self.peers.insert(peer, session);

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

        // The friendly path, for a blip rather than a reboot: say which of the
        // channels they pay us on we still hold. ChannelReady already means
        // "the channel you pay me on is live", and sending it before our Offer
        // means it arrives before theirs is answered — which is where the peer
        // decides whether to fund. A peer that kept nothing ignores it, since
        // it has funded nothing yet for it to confirm.
        for channel_id in held {
            out.push(Action::Send {
                peer,
                msg: Message::ChannelReady(ChannelReady { channel_id }),
            });
        }

        let offer = self.offer_for(&policy, policy.weight(&self.node));
        self.send_offer(peer, offer, out);

        // What it left behind with us, if anything, and that its reservation
        // did not come with it. A peer we do not charge has no budget to hear
        // about.
        if !policy.no_charge
            && let Some(session) = self.peers.get_mut(&peer)
        {
            session.budget_live = session.grant.is_live(now);
            out.push(Action::Send {
                peer,
                msg: Message::Balance(balance_of(session, now)),
            });
        }
        self.refresh(peer, now, out);
    }

    /// Send a peer our Offer, and remember it as the one the keepalive
    /// repeats.
    fn send_offer(&mut self, peer: PubKey, offer: Offer, out: &mut Vec<Action>) {
        if let Some(session) = self.peers.get_mut(&peer) {
            session.offer_sent = Some(offer.clone());
        }
        out.push(Action::Send {
            peer,
            msg: Message::Offer(offer),
        });
    }

    /// What this node advertises to one peer: which mints it will take, the
    /// unit, the terms a purchase must fit, the from-payer weight, and whether
    /// we charge it at all. No price anywhere.
    ///
    /// The weight is the one that peer's budget is drawn at, override
    /// included, fixed when its session started. The payer sizes its purchases
    /// from it, so advertising the node-wide value to a peer we weight
    /// differently would have it buy the wrong amount.
    ///
    /// Not charging is our decision alone, but the peer has to hear it —
    /// otherwise its buyer funds a channel and tops up toward a node that was
    /// never going to meter it.
    fn offer_for(&self, policy: &PeerPolicy, weight: u16) -> Offer {
        let grants = &self.node.grants;
        Offer {
            accepted_mints: self.node.accepted_mints.clone(),
            unit: self.node.unit.clone(),
            min_window_ms: grants.min_window_ms,
            max_window_ms: grants.max_window_ms,
            from_payer_weight: weight,
            no_charge: policy.no_charge,
            min_reserved_rate: grants.min_reserved_rate,
            min_topup_gap_ms: grants.min_topup_gap_ms,
        }
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
                // they sent this because they verified our funding, or, for the
                // channel we are already draining, because they came back
                // still holding it.
                if let Some(session) = self.peers.get_mut(&peer) {
                    if !session.buyer.is_active(m.channel_id) {
                        session.buyer.confirmed(m.channel_id);
                    }
                    // Only read at their opening Offer, and in a session that
                    // started from nothing no ChannelReady can arrive before it.
                    session.kept_by_peer |= session.buyer.is_active(m.channel_id);
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
                        .record_reject(&refused, m.reason, m.max_reserved_rate, now, hold);
                }
                self.poll_buyer(peer, now, out);
            }
            Message::Balance(m) => {
                // Information, not an instruction: kept for the operator, and
                // never a reason to buy more than our own count says.
                if let Some(session) = self.peers.get_mut(&peer) {
                    session
                        .buyer
                        .note_balance(m.remaining, m.expires_in_ms, now);
                    session.balance = Some((m, now));
                }
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
                let expires_at = self.expiry_of(peer, m.channel_id);
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
                    expires_at,
                });
            }
            Message::CloseAck(m) => {
                out.push(Action::SettleChannel {
                    peer,
                    channel_id: m.channel_id,
                    expires_at: self.expiry_of(peer, m.channel_id),
                });
            }
            Message::Reject(_) => {
                // Nothing to unwind: we never advanced state on a proposal that
                // had not been confirmed.
            }
            Message::Disconnect(_) => {
                // Orderly: the session ends here, and is not held for a return.
                if let Some(session) = self.peers.get_mut(&peer) {
                    session.phase = Phase::Closing;
                }
                out.push(Action::DropPeer { peer });
            }
        }
    }

    fn on_announce(&mut self, peer: PubKey, m: Announce, out: &mut Vec<Action>) {
        if m.version != PROTOCOL_VERSION {
            if let Some(session) = self.peers.get_mut(&peer) {
                session.phase = Phase::Closing;
            }
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

        // A revised Offer replaces the terms we hold, and they apply to our
        // next purchase. The from-payer weight is the exception: it is fixed
        // for the session, so the one the opening Offer carried stands.
        let was_established = session.offer.is_some();
        let weight = session
            .offer
            .as_ref()
            .map_or(m.from_payer_weight, |o| o.terms.from_payer_weight);
        session.offer = Some(PeerOffer {
            accepted_mints: m.accepted_mints.clone(),
            unit: m.unit,
            terms: Terms {
                min_window_ms: m.min_window_ms,
                max_window_ms: m.max_window_ms,
                min_reserved_rate: m.min_reserved_rate,
                min_topup_gap_ms: m.min_topup_gap_ms,
                from_payer_weight: weight,
            },
            no_charge: m.no_charge,
        });
        if session.phase == Phase::Opening {
            session.phase = Phase::Establishing;
        }

        if was_established {
            // A revision, not the opening Offer. Nothing to fund — unless it
            // is what makes a channel wanted: a peer that did not charge us
            // and now does. That is the same check the tick makes.
            self.poll_opening(peer, now, out);
            return;
        }

        // A session resumed after an unclean disconnect. If the peer said it
        // still holds the channel we pay it on, carry on paying on it: nothing
        // to fund, and nothing to Accept, since the peer already knows.
        // Otherwise it has lost it, and what we hold is worth only its refund,
        // so start over exactly as a new peer would. There is nothing for us
        // to settle: only the receiver can, and what is left in a channel we
        // funded comes back only through its refund timelock.
        //
        // Nor if there is nothing to buy any more — the peer stopped charging
        // us while we were away, or we stopped buying. Then we let the channel
        // go and answer with the empty Accept below, which also tells the peer
        // to settle what we signed on it.
        // A weight above what we buy at is refused before any money moves:
        // we say so, fund nothing and pay nothing.
        if !m.no_charge && !self.buyer_policy.accepts_weight(m.from_payer_weight) {
            session.refused_terms = true;
            out.push(Action::Send {
                peer,
                msg: Message::Reject(Reject {
                    rejected_type: tollgate_protocol::MsgType::Offer as u8,
                    reason: ReasonCode::FromPayerWeightUnacceptable,
                    text: None,
                }),
            });
        }
        let buying = !m.no_charge && !session.refused_terms && self.buyer_policy.buying();
        if session.buyer.active().is_some() && session.kept_by_peer && buying {
            // A channel it never confirmed is one it does not know about.
            session.buyer.forget_pending();
            session.phase = Phase::Established;
            self.refresh(peer, now, out);
            self.poll_buyer(peer, now, out);
            return;
        }
        // From nothing, then. That includes whatever the last session had
        // asked the host for and not had back: the channel funded below
        // replaces it, so a late answer to one of those is superseded.
        session.buyer = Buyer::new();

        // Fund the channel we will pay them on, against the earliest mint in
        // their list we can use. Their own mint need not be in it: a
        // pass-through relay may name only its upstream's, so it can spend what
        // it receives without converting.
        //
        // Not if they will not charge us: there is nothing to buy, so no
        // channel, and the empty Accept tells them so.
        let mint = m.accepted_mints.first().cloned();
        match mint {
            _ if session.refused_terms => {}
            Some(mint_url) if buying => {
                // Start small: a new peering may not last. The capacity grows
                // as rollovers show that it does.
                let capacity = self.node.first_channel_capacity();
                self.request_funding(peer, mint_url, capacity, now, out);
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
        funding: Vec<u8>,
        now: Millis,
        out: &mut Vec<Action>,
    ) {
        let Some(session) = self.peers.get_mut(&peer) else {
            return;
        };

        let replacing = session.buyer.active().map(|c| c.id);

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
        // An Accept starts the peer's payment stream from nothing. Any channel
        // it paid us on before is one it has forgotten — it came back without
        // its state — so settle what it signed rather than wait on a ratchet
        // nobody will turn again.
        if let Some(session) = self.peers.get_mut(&peer) {
            Self::discard(peer, session, out);
        }

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
        expires_at: Option<Millis>,
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
        session.grant.open_channel(channel_id, capacity, expires_at);
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

    fn on_topup(&mut self, peer: PubKey, m: TopUp, now: Millis, out: &mut Vec<Action>) {
        // The gap first. The host asked the same question before it checked
        // any signature, against an earlier clock, so this only refuses what
        // it did too.
        if let Some(refusal) = self.too_soon(peer, &m, now) {
            out.push(refusal);
            return;
        }

        // Sum what everyone else has reserved. Being able to do this at all is
        // what admission control rests on: the payer states the rate it wants
        // set aside up front, so we can refuse before taking the money rather
        // than shaping below what we promised.
        let reserved_elsewhere = self.reserved_elsewhere(&peer);

        let Some(session) = self.peers.get_mut(&peer) else {
            return;
        };
        session.grant.topup_checked(now);
        let admission = Admission {
            policy: &self.node.grants,
            reserved_elsewhere,
        };

        let verdict = grant::evaluate_topup(
            &session.grant,
            admission,
            &m.updates,
            m.window_ms,
            m.reserved_rate,
        );

        match verdict {
            Verdict::Accept { ratchets, grant } => {
                // The time since the last reading was carried at the old
                // reserved rate; draw it before the new one takes over.
                if let Some(at) = session.drawn_at {
                    session.grant.draw(0, now.saturating_since(at));
                }
                session
                    .grant
                    .apply(&ratchets, grant, m.window_ms, m.reserved_rate, now);
                // A reservation is drawn from the moment it is bought.
                session.drawn_at = Some(now);

                // Only now does the backend keep them: before this, the
                // purchase could still have been refused, and a backend that
                // had recorded part of it would settle a state we never
                // granted.
                out.push(Action::RecordUpdates {
                    peer,
                    updates: m.updates,
                });

                // A channel drained to its capacity carries nothing further.
                // Settling it is what keeps our spent-proof set bounded, which
                // is the reason channels exist at all. The budget it paid for
                // stays.
                let done: Vec<_> = session.grant.exhausted_channels().collect();
                for channel_id in done {
                    let expires_at = session
                        .grant
                        .close_channel(channel_id)
                        .and_then(|c| c.expires_at);
                    out.push(Action::SettleChannel {
                        peer,
                        channel_id,
                        expires_at,
                    });
                }

                // Kept at every purchase, so a crash loses at most what was
                // drawn since — in the payer's favor.
                Self::save_budget(peer, session, now, out);
                session.budget_live = session.grant.is_live(now);
                out.push(Action::Send {
                    peer,
                    msg: Message::Balance(balance_of(session, now)),
                });
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
                max_reserved_rate,
            } => {
                // Echoed in full: a purchase may span several channels, so one
                // channel id no longer identifies which one was refused.
                out.push(Action::Send {
                    peer,
                    msg: Message::TopUpReject(TopUpReject {
                        refused: refused_updates(&m),
                        max_reserved_rate,
                        reason,
                    }),
                });
            }
        }
        self.refresh(peer, now, out);
    }

    /// A meter reading for a peer: draw its budget with us, and our own count
    /// of our budget with it.
    ///
    /// Its budget is drawn by the one rule, `max(moved, reserved × time)`, for
    /// the time since the last reading — but only if we were carrying it all
    /// that time. Ours is drawn the same way, from the same counts read the
    /// other way round and weighted by its from-payer weight.
    fn on_metered(
        &mut self,
        peer: PubKey,
        counters: crate::meter::Counters,
        carried: bool,
        now: Millis,
    ) {
        let Some(session) = self.peers.get_mut(&peer) else {
            return;
        };

        // What we have been pushing at them, as a rate. Read before `observe`
        // consumes the reading.
        let grew = session.meter.observe(counters);
        let elapsed = now.saturating_since(session.last_meter_at);
        if elapsed > 0 {
            session.upload_rate = grant::rate_from(grew.to_payer, elapsed);
            session.last_meter_at = now;
        }

        if carried && session.access.metered() {
            let tick_ms = session.drawn_at.map_or(0, |at| now.saturating_since(at));
            session.grant.draw(grew.weighted(session.weight), tick_ms);
            session.drawn_at = Some(now);
        } else {
            session.drawn_at = None;
        }

        if let Some(offer) = session.offer.as_ref()
            && !offer.no_charge
        {
            let weight = offer.terms.from_payer_weight;
            session.buyer.draw(grew.swapped().weighted(weight), now);
        }
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
                let expires_at = session
                    .grant
                    .close_channel(channel_id)
                    .and_then(|c| c.expires_at);
                out.push(Action::SettleChannel {
                    peer,
                    channel_id,
                    expires_at,
                });
            }
        }
        self.refresh(peer, now, out);
    }

    /// The terms a purchase from `payer` is judged against, and what every
    /// other connected payer has reserved. A reservation ends with its
    /// session, so a payer held after an unclean disconnect reserves nothing.
    fn admission(&self, payer: &PubKey) -> Admission<'_> {
        Admission {
            policy: &self.node.grants,
            reserved_elsewhere: self.reserved_elsewhere(payer),
        }
    }

    /// What every connected payer but `payer` has reserved.
    fn reserved_elsewhere(&self, payer: &PubKey) -> u64 {
        self.peers
            .iter()
            // A session that is ending sets nothing aside: its peer said
            // Disconnect, and its link is on its way down.
            .filter(|(p, s)| *p != payer && s.phase != Phase::Closing)
            .map(|(_, s)| s.grant.reserved_rate())
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
        if offer.no_charge || session.refused_terms {
            // Nothing to buy, and no channel to sign against.
            return;
        }

        // A channel inside the safety margin is about to be settled by the
        // peer, so nothing more is signed on it once there is somewhere else to
        // go — and nothing at all once the peer is due to settle it.
        session
            .buyer
            .retire_expiring(now, self.node.safety_margin_ms, self.node.settle_lead_ms());

        // A unit we download draws one from our budget; a unit we upload draws
        // the peer's from-payer weight. Buying for the download alone would
        // leave the budget running out early.
        let weighted_upload = session
            .upload_rate
            .saturating_mul(offer.terms.from_payer_weight as u64);
        let demand = Demand {
            observed_rate: session.demand.saturating_add(weighted_upload),
            terms: offer.terms,
        };
        let Some(purchase) = buyer::poll(&session.buyer, &self.buyer_policy, demand, now) else {
            return;
        };

        // The channel a purchase retires is always the one it started on.
        let drained = session.buyer.active();
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
            reserved_rate: purchase.reserved_rate,
        });

        // A channel drained to its capacity has nothing left to carry, and its
        // replacement is already holding the overflow. Settling it now is what
        // keeps the issuer's spent-proof set bounded, which is the whole reason
        // channels exist.
        if let Some(channel_id) = retired {
            out.push(Action::SettleChannel {
                peer,
                channel_id,
                expires_at: drained
                    .filter(|c| c.id == channel_id)
                    .and_then(|c| c.expires_at),
            });
        }
    }

    /// Open a replacement channel when the one we fund approaches exhaustion,
    /// or enters the safety margin before its expiry.
    ///
    /// Only for the direction we fund: rollover is initiated by the funder
    /// alone, since only the party putting up new funds decides when.
    fn poll_rollover(&mut self, peer: PubKey, now: Millis, out: &mut Vec<Action>) {
        let Some(session) = self.peers.get(&peer) else {
            return;
        };
        let Some(offer) = session.offer.as_ref() else {
            return;
        };
        // The margin is not advertised, so both ends of a channel arrive at
        // the same one by being configured alike.
        let margin_ms = self.node.safety_margin_ms;
        let Some(reason) =
            session
                .buyer
                .rollover_due(self.node.rollover_threshold_pct, now, margin_ms)
        else {
            return;
        };
        let Some(active) = session.buyer.active() else {
            return;
        };
        let Some(mint_url) = offer.accepted_mints.first().cloned() else {
            return;
        };

        // Only a channel that filled up earns a bigger successor. One that
        // merely ran out of time was big enough, and growing it would lock more
        // away for the same traffic.
        let capacity = match reason {
            RolloverReason::Capacity => self.node.grown_capacity(active.capacity),
            RolloverReason::Expiry => self.node.clamp_capacity(active.capacity),
        };

        self.request_funding(peer, mint_url, capacity, now, out);
    }

    /// Fund a channel to pay this peer on, if we buy from it and have none —
    /// neither in use, nor confirmed and waiting, nor funded and awaiting
    /// confirmation, nor asked for and still being waited on.
    ///
    /// The peer's opening Offer asks for the first channel at once, so this
    /// is for the cases that one misses: the host could not fund it, or never
    /// answered; the peer charged nothing at first and has started to; or the
    /// channel in use reached its settle point before a replacement came, and
    /// was given up with nothing behind it. Without it a session in any of
    /// those stays connected — the keepalive sees to that — and never pays.
    ///
    /// Funded as at the opening, and announced with Accept: with no channel in
    /// use there is nothing to roll over from.
    fn poll_opening(&mut self, peer: PubKey, now: Millis, out: &mut Vec<Action>) {
        let Some(session) = self.peers.get(&peer) else {
            return;
        };
        if session.phase == Phase::Closing {
            return;
        }
        let Some(offer) = session.offer.as_ref() else {
            return;
        };
        if offer.no_charge || session.refused_terms || !self.buyer_policy.buying() {
            return;
        }
        let buyer = &session.buyer;
        if buyer.active().is_some()
            || buyer.next_channel().is_some()
            || buyer.awaiting_confirmation()
            || buyer.funding_in_flight(now)
        {
            return;
        }
        let Some(mint_url) = offer.accepted_mints.first().cloned() else {
            return;
        };
        let capacity = self.node.first_channel_capacity();
        self.request_funding(peer, mint_url, capacity, now, out);
    }

    /// Ask the host to fund a channel for this peer, under a fresh request id.
    ///
    /// Marked now, not when the channel comes back: funding is a mint round
    /// trip, many ticks long, and every check in between would otherwise ask
    /// for another channel. Nothing more is asked for until the host answers
    /// and the peer confirms, or the host reports a failure, or the request
    /// times out.
    fn request_funding(
        &mut self,
        peer: PubKey,
        mint_url: alloc::string::String,
        capacity: u64,
        now: Millis,
        out: &mut Vec<Action>,
    ) {
        let request = self.next_request;
        self.next_request = self.next_request.wrapping_add(1);
        if let Some(session) = self.peers.get_mut(&peer) {
            session.buyer.funding_requested(request, now);
        }
        out.push(Action::FundChannel {
            peer,
            request,
            mint_url,
            capacity,
        });
    }

    /// Settle every channel this peer pays us on that is about to expire.
    ///
    /// Past its expiry the peer can reclaim the whole channel through the
    /// refund path, including what it has already paid us on it, so this is
    /// the receiver protecting earnings it already has. Only the receiver
    /// settles; the funder rolls over earlier and has moved on by now.
    fn settle_expiring(&mut self, peer: PubKey, now: Millis, out: &mut Vec<Action>) {
        let lead_ms = self.settle_lead_ms();
        if let Some(session) = self.peers.get_mut(&peer) {
            Self::settle_expiring_in(peer, session, now, lead_ms, out);
        }
    }

    /// How long before a channel's expiry we, as its receiver, settle it.
    fn settle_lead_ms(&self) -> u64 {
        self.node.settle_lead_ms()
    }

    /// Settle, and stop recognising, every channel in `session` within
    /// `lead_ms` of its expiry — live or held, the refund path opens on the
    /// same clock.
    fn settle_expiring_in(
        peer: PubKey,
        session: &mut PeerSession,
        now: Millis,
        lead_ms: u64,
        out: &mut Vec<Action>,
    ) {
        let due: Vec<_> = session.grant.expiring_channels(now, lead_ms).collect();
        for channel_id in due {
            let expires_at = session
                .grant
                .close_channel(channel_id)
                .and_then(|c| c.expires_at);
            out.push(Action::SettleChannel {
                peer,
                channel_id,
                expires_at,
            });
        }
    }

    /// When a channel with this peer expires, whichever of us funded it, as
    /// far as we still know it.
    fn expiry_of(&self, peer: PubKey, channel_id: tollgate_protocol::ChannelId) -> Option<Millis> {
        let session = self.peers.get(&peer)?;
        if let Some(channel) = session.grant.channel(channel_id) {
            return channel.expires_at;
        }
        [session.buyer.active(), session.buyer.next_channel()]
            .into_iter()
            .flatten()
            .find(|c| c.id == channel_id)
            .and_then(|c| c.expires_at)
    }

    // -----------------------------------------------------------------------
    // Gate and shaper
    // -----------------------------------------------------------------------

    /// Recompute a peer's access level and shaping rate, emitting an action
    /// only where something actually changed.
    fn refresh(&mut self, peer: PubKey, now: Millis, out: &mut Vec<Action>) {
        let minimum_flow = self.node.minimum_flow;
        let tick_ms = self.node.tick_ms;
        let Some(session) = self.peers.get_mut(&peer) else {
            return;
        };
        let burst = session.policy.burst(&self.node);

        let access = if session.policy.no_charge {
            AccessLevel::Free
        } else if !session.grant.channels().is_empty() || session.grant.is_live(now) {
            // Paying, or still delivering what was paid for after the last
            // channel filled or was settled — or what the payer brought with
            // it from an earlier session.
            AccessLevel::Active
        } else {
            // Never paid, or every channel it paid us on has been drained and
            // settled with nothing left of its budget. Either way there is no
            // session — only the allowance — and the peer can still negotiate
            // one without reconnecting.
            AccessLevel::None
        };

        if access != session.access {
            session.access = access;
            out.push(Action::SetAccess { peer, access });
        }

        // The moment its budget runs out or expires, the payer hears so, and
        // the host can let its record go.
        let live = session.grant.is_live(now);
        if session.budget_live && !live && access != AccessLevel::Free {
            out.push(Action::Send {
                peer,
                msg: Message::Balance(balance_of(session, now)),
            });
            out.push(Action::SaveBudget {
                peer,
                budget: Budget::NONE,
            });
        }
        session.budget_live = live;

        let rate = if access == AccessLevel::Free {
            // Unmetered: no budget exists in either direction.
            u64::MAX
        } else {
            session
                .grant
                .shaping_rate(now, burst, minimum_flow, tick_ms)
        };

        if session.applied_rate != Some(rate) {
            session.applied_rate = Some(rate);
            out.push(Action::SetShapingRate { peer, rate });
        }
    }

    fn reject(&mut self, peer: PubKey, reason: ReasonCode, out: &mut Vec<Action>) {
        if let Some(session) = self.peers.get_mut(&peer) {
            session.phase = Phase::Closing;
        }
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

/// The states a refused purchase would have ratcheted to, echoed in full: a
/// purchase may span several channels, so one channel id does not identify it.
fn refused_updates(topup: &TopUp) -> Vec<RefusedUpdate> {
    topup
        .updates
        .iter()
        .map(|u| RefusedUpdate {
            channel_id: u.channel_id,
            cumulative: u.cumulative,
        })
        .collect()
}

/// What we tell a payer about its budget with us.
fn balance_of(session: &PeerSession, now: Millis) -> Balance {
    let budget = session.grant.budget(now);
    Balance {
        remaining: budget.remaining,
        expires_in_ms: budget.expires_in_ms(now),
        reserved_rate: session.grant.reserved_rate(),
    }
}
