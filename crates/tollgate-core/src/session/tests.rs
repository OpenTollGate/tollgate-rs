//! Two nodes talking to each other, entirely in memory.
//!
//! This is what sans-IO buys: the full opening sequence, both payment streams,
//! admission control and the shaper all run here with no socket, no clock and
//! no wallet — the harness below stands in for all three in about eighty lines.
//! `tollgate-net` runs the same state machine against real ones.

use alloc::collections::{BTreeMap, VecDeque};
use alloc::vec;
use alloc::vec::Vec;

use tollgate_protocol::{
    ChannelId, ChannelUpdate, Disconnect, Message, PubKey, ReasonCode, Reject, Signature, TopUp,
    TopUpReject,
};

use super::*;
use crate::access::AccessLevel;
use crate::action::Action;
use crate::buyer::{BuyerPolicy, FUNDING_TIMEOUT_MS};
use crate::config::{GrantPolicy, NodePolicy, PeerPolicy};
use crate::event::Event;
use crate::grant::MAX_VERIFICATION_FAILURES;
use crate::meter::Counters;
use crate::time::Millis;

const CHANNEL_CAPACITY: u64 = 1_000_000_000;

fn pubkey(seed: u8) -> PubKey {
    let mut b = [seed; 33];
    b[0] = 0x02;
    PubKey(b)
}

fn node_policy(mint: &str) -> NodePolicy {
    NodePolicy {
        unit: "byte".into(),
        accepted_mints: vec![mint.into()],
        received_multiplier: 0,
        minimum_flow: 4_096,
        grants: GrantPolicy {
            min_window_ms: 200,
            max_window_ms: 30_000,
            max_rate: None,
        },
        initial_channel_capacity: CHANNEL_CAPACITY,
        min_channel_capacity: 1,
        max_channel_capacity: u64::MAX,
        capacity_growth_pct: 200,
        safety_margin_floor_ms: 60_000,
        stale_timeout_ms: 0,
        rollover_threshold_pct: 80,
    }
}

/// The grace period for an unclean disconnect is the stale timeout.
fn grace_policy(mint: &str, grace_ms: u64) -> NodePolicy {
    NodePolicy {
        stale_timeout_ms: grace_ms,
        ..node_policy(mint)
    }
}

fn buyer_policy() -> BuyerPolicy {
    BuyerPolicy {
        window_ms: 2_000,
        ..BuyerPolicy::default()
    }
}

/// One node plus the effects the harness has to stand in for.
struct Node {
    id: PubKey,
    sessions: Sessions,
    /// Shaping rate last applied per peer — what the enforcer would be
    /// enforcing.
    shaping: BTreeMap<PubKey, u64>,
    /// Access level last applied per peer.
    access: BTreeMap<PubKey, AccessLevel>,
    /// Next channel id to hand out, so ids stay distinguishable in failures.
    next_channel: u8,
    /// Capacity of every channel this node funded, in order.
    funded: Vec<u64>,
    /// Every channel this node was asked to settle, in order.
    settled: Vec<ChannelId>,
    /// The expiry each settlement was asked for with, which the host retries
    /// it until.
    settle_deadlines: BTreeMap<ChannelId, Option<Millis>>,
    /// How long a channel this node funds lives, or `None` for channels that
    /// never expire — which is what every test not about expiry wants.
    ttl_ms: Option<u64>,
    /// Hold funding requests until the test answers them, rather than funding
    /// at once — a real wallet takes a mint round trip, many ticks long.
    defer_funding: bool,
    /// Funding requests held under `defer_funding`, oldest first: the peer,
    /// the request id and the capacity.
    held_funding: VecDeque<(PubKey, u64, u64)>,
    /// Channels core handed back unused, to reclaim through the refund path.
    reclaimed: Vec<ChannelId>,
    /// Offers this node sent each peer, opening one included.
    offers_sent: BTreeMap<PubKey, usize>,
    /// The last Offer this node sent each peer.
    last_offer: BTreeMap<PubKey, tollgate_protocol::Offer>,
}

impl Node {
    fn new(id: PubKey, policy: NodePolicy) -> Self {
        Self {
            id,
            sessions: Sessions::new(id, policy, buyer_policy()),
            shaping: BTreeMap::new(),
            access: BTreeMap::new(),
            next_channel: id.0[1],
            funded: Vec::new(),
            settled: Vec::new(),
            settle_deadlines: BTreeMap::new(),
            ttl_ms: None,
            defer_funding: false,
            held_funding: VecDeque::new(),
            reclaimed: Vec::new(),
            offers_sent: BTreeMap::new(),
            last_offer: BTreeMap::new(),
        }
    }

    /// What the wallet reports once it has funded a channel toward `peer`. The
    /// blob carries what the other side's wallet would read out of real
    /// funding: the channel, its capacity and its expiry.
    fn funding_done(&mut self, peer: PubKey, request: u64, capacity: u64, now: Millis) -> Event {
        self.next_channel = self.next_channel.wrapping_add(1);
        let channel_id = ChannelId([self.next_channel; 32]);
        let expires_at = self.ttl_ms.map(|ttl| now + ttl);
        let mut funding = vec![self.next_channel];
        funding.extend_from_slice(&capacity.to_be_bytes());
        funding.extend_from_slice(&expires_at.map_or(0, |e| e.0).to_be_bytes());
        Event::OutgoingChannelFunded {
            peer,
            request,
            channel_id,
            capacity,
            expires_at,
            funding,
        }
    }
}

/// Run a set of events, each addressed to a node by id, and everything they
/// set off, to quiescence. A message goes to the node its `peer` names.
///
/// FIFO, because messages arrive in the order they were sent and a LIFO
/// drain would reorder a node's own Announce behind its Offer.
fn run(nodes: &mut [&mut Node], now: Millis, initial: impl IntoIterator<Item = (PubKey, Event)>) {
    let mut queue: VecDeque<(PubKey, Event)> = initial.into_iter().collect();

    while let Some((to, event)) = queue.pop_front() {
        let node = nodes
            .iter_mut()
            .find(|n| n.id == to)
            .expect("event addressed to a node in the harness");
        let actions = node.sessions.handle(event, now);
        carry_out(node, actions, now, &mut queue);
    }
}

/// Carry out what one node asked for, queueing whatever it sets off.
fn carry_out(
    node: &mut Node,
    actions: Vec<Action>,
    now: Millis,
    queue: &mut VecDeque<(PubKey, Event)>,
) {
    let from = node.id;
    for action in actions {
        match action {
            // The wire: what one node sends, the other receives.
            Action::Send { peer, msg } => {
                if let Message::Offer(offer) = &msg {
                    *node.offers_sent.entry(peer).or_default() += 1;
                    node.last_offer.insert(peer, offer.clone());
                }
                queue.push_back((peer, Event::MessageReceived { peer: from, msg }))
            }

            // The wallet: funding always succeeds, instantly, unless the
            // test holds it to answer later.
            Action::FundChannel {
                peer,
                request,
                capacity,
                ..
            } => {
                node.funded.push(capacity);
                if node.defer_funding {
                    node.held_funding.push_back((peer, request, capacity));
                } else {
                    let done = node.funding_done(peer, request, capacity, now);
                    queue.push_back((from, done));
                }
            }
            Action::VerifyFunding { peer, funding } => {
                let number = |at: usize| {
                    u64::from_be_bytes(funding[at..at + 8].try_into().expect("8 bytes"))
                };
                queue.push_back((
                    from,
                    Event::IncomingFundingVerified {
                        peer,
                        channel_id: ChannelId([funding[0]; 32]),
                        capacity: number(1),
                        expires_at: Some(Millis(number(9))).filter(|e| e.0 > 0),
                        mint_url: node.sessions.node_policy().accepted_mints[0].clone(),
                    },
                ))
            }

            // The signer: core decides what to sign, we produce bytes.
            Action::SignAndSendTopUp {
                peer,
                ratchets,
                window_ms,
            } => queue.push_back((
                peer,
                Event::MessageReceived {
                    peer: from,
                    msg: Message::TopUp(TopUp {
                        updates: ratchets
                            .into_iter()
                            .map(|(channel_id, cumulative)| ChannelUpdate {
                                channel_id,
                                cumulative,
                                signature: Signature([0; 64]),
                            })
                            .collect(),
                        window_ms,
                    }),
                },
            )),

            // The enforcer.
            Action::SetShapingRate { peer, rate } => {
                node.shaping.insert(peer, rate);
            }
            Action::SetAccess { peer, access } => {
                node.access.insert(peer, access);
            }

            Action::SettleChannel {
                channel_id,
                expires_at,
                ..
            } => {
                node.settled.push(channel_id);
                node.settle_deadlines.insert(channel_id, expires_at);
            }
            Action::ReclaimChannel { channel_id, .. } => {
                node.reclaimed.push(channel_id);
            }
            // The channel backend's record, which only settlement reads.
            Action::RecordUpdates { .. } | Action::DropPeer { .. } => {}
        }
    }
}

/// Two nodes and the wire between them.
struct Link {
    a: Node,
    b: Node,
    now: Millis,
}

impl Link {
    fn new() -> Self {
        Self {
            a: Node::new(pubkey(0xA1), node_policy("https://a.example/mint")),
            b: Node::new(pubkey(0xB2), node_policy("https://b.example/mint")),
            now: Millis(0),
        }
    }

    /// Two nodes that hold a peer's state for `grace_ms` after it drops
    /// without a Disconnect.
    fn with_grace(grace_ms: u64) -> Self {
        let mut link = Self::new();
        link.a = Node::new(link.a.id, grace_policy("https://a.example/mint", grace_ms));
        link.b = Node::new(link.b.id, grace_policy("https://b.example/mint", grace_ms));
        link
    }

    /// Feed one event and run everything it sets off to completion, including
    /// whatever the other node does in reply.
    fn deliver(&mut self, to_a: bool, event: Event) {
        self.pump([(to_a, event)]);
    }

    /// Run a set of events and everything they set off, to quiescence.
    fn pump(&mut self, initial: impl IntoIterator<Item = (bool, Event)>) {
        let (a, b) = (self.a.id, self.b.id);
        let initial = initial
            .into_iter()
            .map(|(to_a, event)| (if to_a { a } else { b }, event));
        run(&mut [&mut self.a, &mut self.b], self.now, initial);
    }

    /// Bring both sides up and run the opening sequence to quiescence.
    /// Both sides learn about each other before any bytes flow, which is what
    /// a real transport does: the link comes up, then it carries messages.
    fn connect(&mut self) {
        let (a, b) = (self.a.id, self.b.id);
        self.pump([
            (true, Event::PeerConnected { peer: b }),
            (false, Event::PeerConnected { peer: a }),
        ]);
    }

    /// Both transports go away with no Disconnect — a Wi-Fi blip, a bare FIN.
    fn blip(&mut self) {
        let (a, b) = (self.a.id, self.b.id);
        self.pump([
            (true, Event::PeerDisconnected { peer: b }),
            (false, Event::PeerDisconnected { peer: a }),
        ]);
    }

    /// An operator override on one node, made while the link runs, and
    /// everything it sets off.
    fn set_policy(&mut self, on_a: bool, peer: PubKey, policy: PeerPolicy) {
        let now = self.now;
        let node = if on_a { &mut self.a } else { &mut self.b };
        let actions = node.sessions.set_peer_policy(peer, policy, now);
        let mut queue = VecDeque::new();
        carry_out(node, actions, now, &mut queue);
        run(&mut [&mut self.a, &mut self.b], now, queue);
    }

    /// Advance the clock to `until` and tick both nodes a second at a time
    /// on the way, as a host would — so neither side's stale timeout sees a
    /// jump it would read as silence.
    fn advance_to(&mut self, until: Millis) {
        while self.now < until {
            let step = until.saturating_since(self.now).min(1_000);
            self.advance(step);
        }
    }

    /// Advance the clock and tick both nodes.
    fn advance(&mut self, ms: u64) {
        self.now = self.now + ms;
        self.deliver(true, Event::Tick);
        self.deliver(false, Event::Tick);
    }

    /// What A is shaping B to.
    fn a_shapes_b(&self) -> u64 {
        self.a.shaping.get(&self.b.id).copied().unwrap_or(0)
    }

    /// What B is shaping A to.
    fn b_shapes_a(&self) -> u64 {
        self.b.shaping.get(&self.a.id).copied().unwrap_or(0)
    }
}

// ---------------------------------------------------------------------------
// Opening
// ---------------------------------------------------------------------------

#[test]
fn both_sides_open_a_channel_and_reach_active() {
    // Two channels is the default, not an exception: each side pays for what it
    // received, so both owe and both fund.
    let mut link = Link::new();
    link.connect();

    assert_eq!(link.a.access.get(&link.b.id), Some(&AccessLevel::Active));
    assert_eq!(link.b.access.get(&link.a.id), Some(&AccessLevel::Active));

    let a_to_b = link.a.sessions.peer(&link.b.id).expect("session");
    assert_eq!(a_to_b.phase, Phase::Established);
    assert!(!a_to_b.grant.channels().is_empty(), "B pays A on this one");
    assert!(a_to_b.buyer.active().is_some(), "A pays B on this one");
}

#[test]
fn an_unpaid_peer_gets_the_allowance_and_nothing_more() {
    // The allowance is what lets a peer holding no vouchers reach a mint and
    // acquire some. Without it the bootstrap circle never breaks.
    let mut link = Link::new();
    link.connect();

    assert_eq!(link.a_shapes_b(), 4_096, "nobody has bought anything yet");
    assert_eq!(link.b_shapes_a(), 4_096);
}

// ---------------------------------------------------------------------------
// The algorithm: demand drives purchases, purchases drive the shaper
// ---------------------------------------------------------------------------

#[test]
fn traffic_on_one_side_raises_the_rate_the_other_side_shapes_to() {
    // This is the whole loop: A wants throughput, A buys it, B shapes A to what
    // A bought. Nothing was negotiated and nothing was acknowledged.
    let mut link = Link::new();
    link.connect();

    let b = link.b.id;
    link.deliver(
        true,
        Event::DemandObserved {
            peer: b,
            rate: 1_000_000,
        },
    );

    assert_eq!(
        link.b_shapes_a(),
        1_250_000,
        "B shapes A to the 1 M/s A wanted plus its 25% headroom"
    );
    assert_eq!(
        link.a_shapes_b(),
        4_096,
        "B bought nothing, so B is unchanged"
    );
}

#[test]
fn a_traffic_spike_is_answered_within_one_message() {
    // Under postpaid settlement a rate change waited for a settlement boundary.
    // A grant takes effect when it arrives, so the answer is one message.
    let mut link = Link::new();
    link.connect();

    let b = link.b.id;
    link.deliver(
        true,
        Event::DemandObserved {
            peer: b,
            rate: 1_000_000,
        },
    );
    assert_eq!(link.b_shapes_a(), 1_250_000);

    // Mid-window, without waiting for anything.
    link.now = link.now + 500;
    link.deliver(
        true,
        Event::DemandObserved {
            peer: b,
            rate: 16_000_000,
        },
    );
    assert_eq!(link.b_shapes_a(), 20_000_000, "raised mid-window");
}

#[test]
fn the_two_directions_are_independent() {
    // Different mints, different windows, bought at different moments, neither
    // waiting for the other.
    let mut link = Link::new();
    link.connect();

    let (a, b) = (link.a.id, link.b.id);
    link.deliver(
        true,
        Event::DemandObserved {
            peer: b,
            rate: 8_000_000,
        },
    );
    link.deliver(
        false,
        Event::DemandObserved {
            peer: a,
            rate: 250_000,
        },
    );

    assert_eq!(link.b_shapes_a(), 10_000_000);
    assert_eq!(link.a_shapes_b(), 312_500);
}

#[test]
fn an_expired_grant_drops_the_peer_to_the_allowance_not_to_silence() {
    let mut link = Link::new();
    link.connect();

    let b = link.b.id;
    link.deliver(
        true,
        Event::DemandObserved {
            peer: b,
            rate: 1_000_000,
        },
    );
    assert_eq!(link.b_shapes_a(), 1_250_000);

    // A goes quiet: demand drops to nothing and it stops buying.
    link.deliver(true, Event::DemandObserved { peer: b, rate: 0 });
    link.advance(3_000);

    assert_eq!(
        link.b_shapes_a(),
        4_096,
        "back to the allowance, which is what leaves A able to buy again"
    );
    assert_eq!(
        link.b.access.get(&link.a.id),
        Some(&AccessLevel::Active),
        "an expired grant is not a suspension"
    );
}

#[test]
fn with_the_allowance_disabled_a_lapsed_peer_is_not_carried() {
    // The allowance is the only thing that carries a peer that stopped paying,
    // so with none there is nothing left: every enforcer closes the gate.
    let mut policy = node_policy("https://b.example/mint");
    policy.minimum_flow = 0;

    let mut link = Link::new();
    link.b = Node::new(link.b.id, policy);
    link.connect();

    let (a, b) = (link.a.id, link.b.id);
    let carried = |link: &Link| link.b.access[&a].carried(link.b_shapes_a());
    assert!(!carried(&link), "nothing bought and no allowance");

    link.deliver(
        true,
        Event::DemandObserved {
            peer: b,
            rate: 1_000_000,
        },
    );
    assert!(carried(&link), "carried at what it bought");

    link.deliver(true, Event::DemandObserved { peer: b, rate: 0 });
    link.advance(3_000);
    assert_eq!(link.b_shapes_a(), 0);
    assert!(!carried(&link), "the grant lapsed and there is no floor");
}

#[test]
fn traffic_draws_the_grant_down_and_exhausting_it_falls_back_to_the_allowance() {
    let mut link = Link::new();
    link.connect();

    let (a, b) = (link.a.id, link.b.id);
    link.deliver(
        true,
        Event::DemandObserved {
            peer: b,
            rate: 1_000_000,
        },
    );
    let bought = link
        .b
        .sessions
        .peer(&a)
        .expect("session")
        .grant
        .authorized();
    assert_eq!(bought, 2_500_000, "1.25 M/s over a 2 s window");

    // B delivers the whole grant to A.
    link.deliver(
        false,
        Event::Metered {
            peer: a,
            counters: Counters {
                delivered: bought,
                received: 0,
            },
        },
    );

    let grant = &link.b.sessions.peer(&a).expect("session").grant;
    assert_eq!(grant.remaining(), 0);
    assert_eq!(link.b_shapes_a(), 4_096);
}

#[test]
fn a_lapsed_payment_ends_the_session_and_leaves_the_allowance() {
    // The allowance is not a session. Once the last channel is full and the
    // grant it paid for has run out, the peer is back where an unpaid one
    // starts: None, on the allowance, able to pay its way back in.
    let mut link = Link::new();
    link.connect();

    let a = link.a.id;
    let channel_id = link.b.sessions.peer(&a).expect("session").grant.channels()[0].id;

    // A fills its channel in one purchase, and nothing replaces it.
    link.deliver(
        false,
        Event::MessageReceived {
            peer: a,
            msg: Message::TopUp(TopUp {
                updates: vec![ChannelUpdate {
                    channel_id,
                    cumulative: CHANNEL_CAPACITY,
                    signature: Signature([0; 64]),
                }],
                window_ms: 1_000,
            }),
        },
    );
    let session = link.b.sessions.peer(&a).expect("session");
    assert!(
        session.grant.channels().is_empty(),
        "the full channel closed"
    );
    assert_eq!(
        link.b.access.get(&a),
        Some(&AccessLevel::Active),
        "still delivering what the last channel paid for"
    );

    link.now = link.now + 1_001;
    link.deliver(false, Event::Tick);

    assert_eq!(link.b.access.get(&a), Some(&AccessLevel::None));
    assert_eq!(link.b_shapes_a(), 4_096, "the allowance, not silence");
}

fn topup(channel_id: ChannelId, cumulative: u64, window_ms: u32) -> Event {
    Event::MessageReceived {
        peer: pubkey(0xA1),
        msg: Message::TopUp(TopUp {
            updates: vec![ChannelUpdate {
                channel_id,
                cumulative,
                signature: Signature([7; 64]),
            }],
            window_ms,
        }),
    }
}

fn records_anything(actions: &[Action]) -> bool {
    actions
        .iter()
        .any(|x| matches!(x, Action::RecordUpdates { .. }))
}

#[test]
fn an_accepted_purchase_is_recorded_before_the_channel_it_filled_settles() {
    // Settlement submits whatever the backend recorded, so the fill has to be
    // recorded before the settle that follows it, or the channel settles one
    // purchase short.
    let mut link = Link::new();
    link.connect();
    let a = link.a.id;
    let channel_id = link.b.sessions.peer(&a).expect("session").grant.channels()[0].id;

    let actions = link
        .b
        .sessions
        .handle(topup(channel_id, CHANNEL_CAPACITY, 1_000), link.now);

    let record = actions
        .iter()
        .position(|x| matches!(x, Action::RecordUpdates { .. }))
        .expect("an accepted purchase is recorded");
    let settle = actions
        .iter()
        .position(|x| matches!(x, Action::SettleChannel { .. }))
        .expect("the full channel settles");
    assert!(record < settle, "recorded before it settles: {actions:?}");

    assert_eq!(
        actions[record],
        Action::RecordUpdates {
            peer: a,
            updates: vec![ChannelUpdate {
                channel_id,
                cumulative: CHANNEL_CAPACITY,
                signature: Signature([7; 64]),
            }],
        },
        "exactly what the peer signed, signature included"
    );
}

#[test]
fn a_refused_purchase_records_nothing() {
    // The host has already checked the signatures, but core can still refuse:
    // a window outside what we advertised is one way. Nothing may be recorded
    // then, or the backend would settle a state no grant paid for.
    let mut link = Link::new();
    link.connect();
    let a = link.a.id;
    let channel_id = link.b.sessions.peer(&a).expect("session").grant.channels()[0].id;

    let actions = link
        .b
        .sessions
        .handle(topup(channel_id, 1_000, 30_001), link.now);

    assert!(
        actions.iter().any(|x| matches!(
            x,
            Action::Send {
                msg: Message::TopUpReject(r),
                ..
            } if r.reason == ReasonCode::WindowOutOfRange
        )),
        "refused for its window: {actions:?}"
    );
    assert!(!records_anything(&actions), "{actions:?}");
}

#[test]
fn a_purchase_refused_for_one_of_its_updates_records_none_of_them() {
    // One purchase spanning two channels, the second of which we do not know.
    // The first is fine on its own, and that is exactly the case that must not
    // leave it recorded: the purchase is refused as a whole.
    let mut link = Link::new();
    link.connect();
    let a = link.a.id;
    let channel_id = link.b.sessions.peer(&a).expect("session").grant.channels()[0].id;

    let actions = link.b.sessions.handle(
        Event::MessageReceived {
            peer: a,
            msg: Message::TopUp(TopUp {
                updates: vec![
                    ChannelUpdate {
                        channel_id,
                        cumulative: 1_000,
                        signature: Signature([7; 64]),
                    },
                    ChannelUpdate {
                        channel_id: ChannelId([0xEE; 32]),
                        cumulative: 1_000,
                        signature: Signature([7; 64]),
                    },
                ],
                window_ms: 1_000,
            }),
        },
        link.now,
    );

    assert!(
        actions.iter().any(|x| matches!(
            x,
            Action::Send {
                msg: Message::TopUpReject(r),
                ..
            } if r.reason == ReasonCode::FundingInvalid
        )),
        "{actions:?}"
    );
    assert!(!records_anything(&actions), "{actions:?}");
    assert_eq!(
        link.b.sessions.peer(&a).expect("session").grant.channels()[0].signed,
        0,
        "and the grant never moved either"
    );
}

#[test]
fn a_peers_uploads_draw_its_own_grant_when_the_multiplier_is_set() {
    // m = 2 charges an uploaded unit the same as a downloaded one.
    let mut policy = node_policy("https://b.example/mint");
    policy.received_multiplier = 2;

    let mut link = Link::new();
    link.b = Node::new(link.b.id, policy);
    link.connect();

    let (a, b) = (link.a.id, link.b.id);
    link.deliver(
        true,
        Event::DemandObserved {
            peer: b,
            rate: 1_000_000,
        },
    );
    let authorized = link
        .b
        .sessions
        .peer(&a)
        .expect("session")
        .grant
        .authorized();

    // A uploads a quarter of its grant's worth. At m = 2 that draws half.
    let upload = authorized / 4;
    link.deliver(
        false,
        Event::Metered {
            peer: a,
            counters: Counters {
                delivered: 0,
                received: upload,
            },
        },
    );

    let grant = &link.b.sessions.peer(&a).expect("session").grant;
    assert_eq!(grant.consumed(), upload * 2, "each uploaded unit drew two");
}

// ---------------------------------------------------------------------------
// Admission control and policy
// ---------------------------------------------------------------------------

#[test]
fn a_rate_beyond_capacity_is_refused_and_the_payer_re_buys_at_what_was_offered() {
    // The payer learns in one round trip instead of inferring a shortfall from
    // throughput that never arrived.
    let mut policy = node_policy("https://b.example/mint");
    policy.grants.max_rate = Some(5_000_000);

    let mut link = Link::new();
    link.b = Node::new(link.b.id, policy);
    link.connect();

    let b = link.b.id;
    link.deliver(
        true,
        Event::DemandObserved {
            peer: b,
            rate: 100_000_000,
        },
    );

    // A asks for 125 M/s, B refuses and names 5 M/s, A re-buys at 5 M/s and B
    // honors it — the whole exchange inside one round trip, which is the point
    // of the refusal carrying a rate rather than just a "no".
    assert_eq!(link.b_shapes_a(), 5_000_000, "re-bought at what B offered");

    // A's own ratchet is where B's is: the refused purchase left no trace.
    let authorized = link
        .b
        .sessions
        .peer(&link.a.id)
        .expect("session")
        .grant
        .authorized();
    let signed = link
        .a
        .sessions
        .peer(&b)
        .expect("session")
        .buyer
        .cumulative();
    assert_eq!(signed, authorized, "payer and provider agree on the total");
}

#[test]
fn max_rate_is_shared_by_every_buyer_not_given_to_each() {
    // grants.max_rate is a node-wide ceiling: what one buyer holds is gone for
    // the next, and the refusal names only what is left.
    let mut policy = node_policy("https://s.example/mint");
    policy.grants.max_rate = Some(5_000_000);

    let mut seller = Node::new(pubkey(0x5E), policy);
    let mut a = Node::new(pubkey(0xA1), node_policy("https://a.example/mint"));
    let mut c = Node::new(pubkey(0xC3), node_policy("https://c.example/mint"));
    let (s, a_id, c_id) = (seller.id, a.id, c.id);
    let mut nodes = [&mut seller, &mut a, &mut c];

    run(
        &mut nodes,
        Millis(0),
        [
            (a_id, Event::PeerConnected { peer: s }),
            (s, Event::PeerConnected { peer: a_id }),
            (c_id, Event::PeerConnected { peer: s }),
            (s, Event::PeerConnected { peer: c_id }),
        ],
    );

    // A wants 3.2 M/s and buys 4 M/s with its headroom — within the cap.
    run(
        &mut nodes,
        Millis(0),
        [(
            a_id,
            Event::DemandObserved {
                peer: s,
                rate: 3_200_000,
            },
        )],
    );
    assert_eq!(nodes[0].shaping.get(&a_id), Some(&4_000_000));

    // C asks for 5 M/s. Alone it would get all of it; with A holding 4 M/s
    // the seller refuses and names 1 M/s, and C re-buys at that.
    run(
        &mut nodes,
        Millis(0),
        [(
            c_id,
            Event::DemandObserved {
                peer: s,
                rate: 4_000_000,
            },
        )],
    );
    assert_eq!(
        nodes[0].shaping.get(&c_id),
        Some(&1_000_000),
        "C gets what A left of the cap"
    );
    assert_eq!(
        nodes[0].shaping.get(&a_id),
        Some(&4_000_000),
        "A's grant is untouched by C's purchase"
    );
}

#[test]
fn a_channel_funded_in_a_mint_we_do_not_list_is_refused() {
    // Whatever the backend checked, core only opens a channel in a mint it
    // takes payment in: the mint is the credit risk, and that is ours to pick.
    let mut link = Link::new();
    let a = link.a.id;
    link.b
        .sessions
        .handle(Event::PeerConnected { peer: a }, Millis(0));

    let actions = link.b.sessions.handle(
        Event::IncomingFundingVerified {
            peer: a,
            channel_id: ChannelId([0xEE; 32]),
            capacity: CHANNEL_CAPACITY,
            expires_at: None,
            mint_url: "https://elsewhere.example/mint".into(),
        },
        Millis(0),
    );

    assert!(
        actions.iter().any(|action| matches!(
            action,
            Action::Send {
                msg: Message::Disconnect(Disconnect {
                    reason: ReasonCode::MintNotAccepted
                }),
                ..
            }
        )),
        "the peer is told why: {actions:?}"
    );
    assert!(matches!(actions.last(), Some(Action::DropPeer { .. })));
    assert!(
        link.b
            .sessions
            .peer(&a)
            .is_none_or(|s| s.grant.channels().is_empty()),
        "no channel was opened"
    );
}

#[test]
fn a_peer_the_operator_does_not_charge_is_free_and_unmetered() {
    let mut link = Link::new();
    let a = link.a.id;
    link.b.sessions.set_peer_policy(
        a,
        PeerPolicy {
            no_charge: true,
            ..PeerPolicy::default()
        },
        link.now,
    );
    link.connect();

    assert_eq!(link.b.access.get(&a), Some(&AccessLevel::Free));
    assert_eq!(link.b_shapes_a(), u64::MAX, "unmetered");

    // One-sided: it controls only whether B charges, never whether A does.
    assert_eq!(link.a.access.get(&link.b.id), Some(&AccessLevel::Active));
}

/// The Offer a node sends a freshly connected peer.
fn opening_offer(node: &mut Node, peer: PubKey) -> tollgate_protocol::Offer {
    node.sessions
        .handle(Event::PeerConnected { peer }, Millis(0))
        .into_iter()
        .find_map(|a| match a {
            Action::Send {
                msg: Message::Offer(o),
                ..
            } => Some(o),
            _ => None,
        })
        .expect("an Offer on connect")
}

#[test]
fn an_offer_says_whether_the_sender_charges_that_peer() {
    let mut link = Link::new();
    let (a, b) = (link.a.id, link.b.id);
    link.b.sessions.set_peer_policy(
        a,
        PeerPolicy {
            no_charge: true,
            ..PeerPolicy::default()
        },
        link.now,
    );

    assert!(opening_offer(&mut link.b, a).no_charge);
    // Default unchanged: a node charges unless the operator said otherwise.
    assert!(!opening_offer(&mut link.a, b).no_charge);
}

#[test]
fn a_peer_that_is_not_charged_funds_nothing_toward_the_node_that_said_so() {
    // One-sided: B does not charge A, A still charges B. So one channel, the
    // one B funds toward A — "a peering can legitimately run with one channel".
    let mut link = Link::new();
    let (a, b) = (link.a.id, link.b.id);
    link.b.sessions.set_peer_policy(
        a,
        PeerPolicy {
            no_charge: true,
            ..PeerPolicy::default()
        },
        link.now,
    );
    link.connect();

    let a_view = link.a.sessions.peer(&b).expect("session");
    assert!(a_view.buyer.active().is_none(), "A funds nothing toward B");
    assert!(!a_view.grant.channels().is_empty(), "B still pays A");
    let b_view = link.b.sessions.peer(&a).expect("session");
    assert!(b_view.grant.channels().is_empty(), "nothing to receive on");
    assert!(b_view.buyer.active().is_some(), "B pays A on this one");

    assert_eq!(link.b.access.get(&a), Some(&AccessLevel::Free));
    assert_eq!(link.a.access.get(&b), Some(&AccessLevel::Active));

    // Demand toward a peer that does not charge buys nothing: no TopUp.
    let actions = link.a.sessions.handle(
        Event::DemandObserved {
            peer: b,
            rate: 1_000_000,
        },
        Millis(0),
    );
    assert!(
        !actions.iter().any(|x| matches!(
            x,
            Action::SignAndSendTopUp { .. } | Action::FundChannel { .. }
        )),
        "A bought from a peer that does not charge it: {actions:?}"
    );

    // B's demand toward A is still bought, and A shapes B to it.
    link.deliver(
        false,
        Event::DemandObserved {
            peer: a,
            rate: 1_000_000,
        },
    );
    assert_eq!(link.a_shapes_b(), 1_250_000);
    assert_eq!(link.b_shapes_a(), u64::MAX, "unmetered");
}

#[test]
fn when_neither_side_charges_there_are_no_channels_and_no_topups() {
    let mut link = Link::new();
    let (a, b) = (link.a.id, link.b.id);
    let free = PeerPolicy {
        no_charge: true,
        ..PeerPolicy::default()
    };
    link.a.sessions.set_peer_policy(b, free, link.now);
    link.b.sessions.set_peer_policy(a, free, link.now);
    link.connect();

    for (node, peer) in [(&link.a, b), (&link.b, a)] {
        let session = node.sessions.peer(&peer).expect("session");
        assert!(session.buyer.active().is_none(), "nothing funded");
        assert!(session.grant.channels().is_empty(), "nothing received");
        assert_eq!(node.access.get(&peer), Some(&AccessLevel::Free));
        assert_eq!(node.shaping.get(&peer), Some(&u64::MAX));
    }

    for (node, peer) in [(&mut link.a, b), (&mut link.b, a)] {
        let mut actions = node.sessions.handle(
            Event::DemandObserved {
                peer,
                rate: 1_000_000,
            },
            Millis(1_000),
        );
        actions.extend(node.sessions.handle(Event::Tick, Millis(2_000)));
        assert!(
            !actions.iter().any(|x| matches!(
                x,
                Action::SignAndSendTopUp { .. } | Action::FundChannel { .. }
            )),
            "free peering bought something: {actions:?}"
        );
    }
}

#[test]
fn a_blocked_peer_is_dropped_without_a_session() {
    let mut link = Link::new();
    let a = link.a.id;
    link.b.sessions.set_peer_policy(
        a,
        PeerPolicy {
            blocked: true,
            ..PeerPolicy::default()
        },
        link.now,
    );

    let actions = link
        .b
        .sessions
        .handle(Event::PeerConnected { peer: a }, Millis(0));

    assert!(matches!(actions.last(), Some(Action::DropPeer { .. })));
    assert!(link.b.sessions.peer(&a).is_none());
}

#[test]
fn a_version_mismatch_is_rejected() {
    let mut link = Link::new();
    let b = link.b.id;
    link.a
        .sessions
        .handle(Event::PeerConnected { peer: b }, Millis(0));

    let actions = link.a.sessions.handle(
        Event::MessageReceived {
            peer: b,
            msg: Message::Announce(tollgate_protocol::Announce {
                version: 99,
                pubkey: b,
                unit: "byte".into(),
                capabilities: 0,
            }),
        },
        Millis(0),
    );

    assert!(actions.iter().any(|a| matches!(
        a,
        Action::Send {
            msg: Message::Reject(_),
            ..
        }
    )));
    assert!(matches!(actions.last(), Some(Action::DropPeer { .. })));
}

#[test]
fn the_shaper_is_only_told_when_something_actually_changed() {
    // A shaper call per meter reading would be a great deal of churn for a
    // number that mostly stays put.
    let mut link = Link::new();
    link.connect();
    let (a, b) = (link.a.id, link.b.id);
    link.deliver(
        true,
        Event::DemandObserved {
            peer: b,
            rate: 1_000_000,
        },
    );

    let steady = link.b.sessions.handle(
        Event::Metered {
            peer: a,
            counters: Counters {
                delivered: 1_000,
                received: 0,
            },
        },
        link.now,
    );

    assert!(
        !steady
            .iter()
            .any(|x| matches!(x, Action::SetShapingRate { .. })),
        "rate did not change, so the enforcer should not have been told"
    );
}

// ---------------------------------------------------------------------------
// Winding down and going quiet
// ---------------------------------------------------------------------------

#[test]
fn shutting_down_tells_every_peer_and_offers_its_channels_for_settlement() {
    // A bare FIN reads as an unclean disconnect and starts the same cleanup a
    // timeout would, so saying so is the difference between the peer tearing
    // our state down on a timer and doing it now.
    let mut link = Link::new();
    link.connect();

    let actions = link.b.sessions.shutdown();
    let a = link.a.id;

    assert!(
        actions.iter().any(|x| matches!(
            x,
            Action::Send {
                msg: Message::Disconnect(_),
                peer,
            } if *peer == a
        )),
        "the peer should be told"
    );
    assert!(
        actions
            .iter()
            .any(|x| matches!(x, Action::SettleChannel { peer, .. } if *peer == a)),
        "the channel it paid us on still holds value we can claim"
    );
}

#[test]
fn a_peer_that_goes_completely_silent_is_dropped() {
    let mut policy = node_policy("https://b.example/mint");
    policy.stale_timeout_ms = 5_000;

    let mut link = Link::new();
    link.b = Node::new(link.b.id, policy);
    link.connect();
    let a = link.a.id;
    assert!(link.b.sessions.peer(&a).is_some());

    // Ticking alone does not refresh `last_seen` — only hearing from them does.
    link.now = Millis(5_001);
    let actions = link.b.sessions.handle(Event::Tick, link.now);

    assert!(
        link.b.sessions.peer(&a).is_none(),
        "the peer should be gone"
    );
    assert!(actions.iter().any(|x| matches!(x, Action::DropPeer { .. })));
}

#[test]
fn a_peer_that_has_merely_stopped_paying_is_kept() {
    // Non-payment enforces itself — the grant lapses and the peer falls to the
    // allowance. A link costs nothing to hold open, so there is nothing here
    // for a timer to do.
    let mut policy = node_policy("https://b.example/mint");
    policy.stale_timeout_ms = 5_000;

    let mut link = Link::new();
    link.b = Node::new(link.b.id, policy);
    link.connect();
    let (a, b) = (link.a.id, link.b.id);

    link.deliver(
        true,
        Event::DemandObserved {
            peer: b,
            rate: 1_000_000,
        },
    );

    // It keeps talking, but buys nothing.
    for step in 1..=10u64 {
        link.now = Millis(step * 1_000);
        link.deliver(
            false,
            Event::MessageReceived {
                peer: a,
                msg: Message::Offer(tollgate_protocol::Offer {
                    accepted_mints: vec!["https://a.example/mint".into()],
                    unit: "byte".into(),
                    min_window_ms: 200,
                    max_window_ms: 30_000,
                    received_multiplier: 0,
                    no_charge: false,
                }),
            },
        );
        link.deliver(false, Event::Tick);
    }

    assert!(link.b.sessions.peer(&a).is_some(), "still a peer");
}

#[test]
fn a_payer_its_provider_never_answers_is_kept_alive_and_a_silent_one_still_dropped() {
    // A payer that asked not to be charged — a proxy buying for a phone —
    // hears nothing back from its provider after setup: TopUps are never
    // answered, and the provider buys nothing from it. Silence is what the
    // stale timeout reads as gone, so without a keepalive the payer dropped a
    // healthy session every minute, and the grant it had just paid for with it.
    const STALE_MS: u64 = 60_000;
    let mut link = Link::with_grace(STALE_MS);
    let (a, b) = (link.a.id, link.b.id);
    link.a.sessions.set_peer_policy(
        b,
        PeerPolicy {
            no_charge: true,
            ..PeerPolicy::default()
        },
        link.now,
    );
    link.connect();
    assert!(
        link.b
            .sessions
            .peer(&a)
            .expect("session")
            .buyer
            .active()
            .is_none(),
        "B buys nothing from A, so has nothing of its own to send"
    );

    // A buys steadily for more than three stale timeouts.
    for _ in 0..200 {
        link.deliver(
            true,
            Event::DemandObserved {
                peer: b,
                rate: 1_000_000,
            },
        );
        link.advance(1_000);
    }
    assert!(link.now > Millis(3 * STALE_MS));
    assert!(link.a.sessions.peer(&b).is_some(), "A still holds B");
    assert!(link.a.sessions.parked(&b).is_none(), "and never dropped it");
    assert_eq!(link.b.access.get(&a), Some(&AccessLevel::Active));

    // What kept it alive: B's Offer again, every third of the timeout. A was
    // topping up all along, so it never needed to.
    assert_eq!(link.b.offers_sent[&a], 1 + 200 / 20, "opening + keepalives");
    assert_eq!(link.a.offers_sent[&b], 1, "the opening Offer alone");

    // B hangs — its process stops, its socket stays open. A keeps ticking and
    // buying, and hears nothing at all.
    let last_heard = link.a.sessions.peer(&b).expect("session").last_seen;
    let mut dropped_at = None;
    for _ in 0..STALE_MS / 1_000 + 2 {
        link.now = link.now + 1_000;
        let actions = link.a.sessions.handle(Event::Tick, link.now);
        if actions
            .iter()
            .any(|x| matches!(x, Action::DropPeer { peer } if *peer == b))
        {
            dropped_at = Some(link.now);
            break;
        }
    }
    let dropped_at = dropped_at.expect("a silent peer is still dropped");
    let silent_for = dropped_at.saturating_since(last_heard);
    assert!(
        silent_for > STALE_MS && silent_for <= STALE_MS + 1_000,
        "dropped {silent_for} ms after it last spoke"
    );
    assert!(link.a.sessions.peer(&b).is_none());
    assert!(
        link.a.sessions.parked(&b).is_some(),
        "held for a return, like any peer that fell silent"
    );
}

#[test]
fn an_opening_channel_that_fails_or_goes_unanswered_is_asked_for_again() {
    // The first channel is asked for once, when the peer's Offer arrives.
    // Before the keepalive, a failure there cost a reconnect: the payer sent
    // nothing, the provider's stale timeout dropped it, and it dialled again
    // and asked again. Now neither side ever goes quiet, so a payer whose
    // opening funding failed stayed connected and unpaid for good — the
    // mint not up yet at boot was enough. The tick asks again instead.
    const STALE_MS: u64 = 60_000;
    let mut link = Link::with_grace(STALE_MS);
    let (a, b) = (link.a.id, link.b.id);
    link.a.defer_funding = true;
    link.connect();
    assert_eq!(link.a.funded.len(), 1, "asked for when B's Offer arrived");
    assert_eq!(link.b.access.get(&a), Some(&AccessLevel::None));

    // The wallet fails. The channel is asked for again on the next tick, and
    // only once while that request is out.
    let (_, failed, _) = link.a.held_funding.pop_front().expect("held");
    link.deliver(
        true,
        Event::OutgoingFundingFailed {
            peer: b,
            request: failed,
        },
    );
    link.advance(100);
    assert_eq!(link.a.funded.len(), 2, "asked again after the failure");
    let asked = link.now;
    for _ in 0..10 {
        link.advance(100);
    }
    assert_eq!(link.a.funded.len(), 2);

    // That one is never answered. It is given up on after the funding
    // timeout, and the channel asked for once more.
    link.advance_to(asked + FUNDING_TIMEOUT_MS - 1);
    assert_eq!(link.a.funded.len(), 2, "not a moment before");
    link.advance(1);
    assert_eq!(link.a.funded.len(), 3, "then asked for again");
    assert_eq!(link.b.access.get(&a), Some(&AccessLevel::None), "unpaid");

    // Kept alive all along, so this is the only way out.
    assert!(link.a.sessions.peer(&b).is_some() && link.b.sessions.peer(&a).is_some());

    // The latest is answered. A announces it, B verifies it, and A pays.
    let (_, unanswered, capacity) = link.a.held_funding.pop_front().expect("held");
    let (_, latest, _) = link.a.held_funding.pop_front().expect("held");
    let done = link.a.funding_done(b, latest, capacity, link.now);
    link.deliver(true, done);
    let opened = link
        .a
        .sessions
        .peer(&b)
        .expect("session")
        .buyer
        .active()
        .map(|c| c.id);
    assert!(opened.is_some(), "B confirmed it");
    assert_eq!(link.b.access.get(&a), Some(&AccessLevel::Active));
    link.deliver(
        true,
        Event::DemandObserved {
            peer: b,
            rate: 1_000_000,
        },
    );
    assert_eq!(link.b_shapes_a(), 1_250_000, "service begins");

    // The request given up on lands after all. The channel it opens is not
    // wanted: it is handed back to be reclaimed, not sent as a second Accept.
    let done = link.a.funding_done(b, unanswered, capacity, link.now);
    let Event::OutgoingChannelFunded { channel_id, .. } = done else {
        unreachable!()
    };
    link.deliver(true, done);
    assert_eq!(link.a.reclaimed, vec![channel_id]);
    assert_eq!(
        link.a
            .sessions
            .peer(&b)
            .expect("session")
            .buyer
            .active()
            .map(|c| c.id),
        opened
    );
    assert_eq!(
        link.b
            .sessions
            .peer(&a)
            .expect("session")
            .grant
            .channels()
            .len(),
        1
    );

    // And with a channel in use, the tick asks for nothing more.
    for _ in 0..50 {
        link.advance(100);
    }
    assert_eq!(link.a.funded.len(), 3);
}

#[test]
fn an_override_reaches_the_peer_when_made_and_the_keepalive_changes_nothing() {
    // The keepalive repeats the Offer last sent, byte for byte. Rebuilt from
    // the policy in force instead, it carried an override out whenever the
    // link happened to go quiet — and a payer told there that it is now
    // charged had no channel and would never fund one. So an override is
    // sent when it is made, and the payer funds on hearing it.
    let mut link = Link::with_grace(60_000);
    let (a, b) = (link.a.id, link.b.id);
    let free = PeerPolicy {
        no_charge: true,
        ..PeerPolicy::default()
    };
    link.b.sessions.set_peer_policy(a, free, link.now);
    link.connect();
    let opening = link.b.last_offer[&a].clone();
    assert!(opening.no_charge);
    assert!(
        link.a
            .sessions
            .peer(&b)
            .expect("session")
            .buyer
            .active()
            .is_none(),
        "A has nothing to pay B for"
    );

    // Quiet: B keeps the link alive with exactly the Offer it opened with.
    link.advance_to(Millis(25_000));
    assert_eq!(link.b.offers_sent[&a], 2, "the opening and one keepalive");
    assert_eq!(link.b.last_offer[&a], opening);

    // B's operator starts charging A. The revision goes out at once, not
    // with the next keepalive, and A funds a channel on hearing it.
    link.set_policy(false, a, PeerPolicy::default());
    assert_eq!(link.b.offers_sent[&a], 3, "the revision, once");
    let revised = link.b.last_offer[&a].clone();
    assert!(!revised.no_charge);
    assert!(
        link.a
            .sessions
            .peer(&b)
            .expect("session")
            .buyer
            .active()
            .is_some(),
        "A pays B now"
    );
    assert_eq!(link.b.access.get(&a), Some(&AccessLevel::Active));

    // The same override again changes nothing, so sends nothing.
    link.set_policy(false, a, PeerPolicy::default());
    assert_eq!(link.b.offers_sent[&a], 3);

    // From here the keepalive repeats the revision.
    link.advance_to(link.now + 25_000);
    assert_eq!(link.b.offers_sent[&a], 4);
    assert_eq!(link.b.last_offer[&a], revised);
    assert_eq!(
        link.b.sessions.peer(&a).expect("session").offer_sent,
        Some(revised)
    );
    assert!(link.a.sessions.peer(&b).is_some() && link.b.sessions.peer(&a).is_some());
}

#[test]
fn a_node_uploading_to_a_surcharging_peer_buys_for_the_surcharge_too() {
    // At m = 2 an uploaded unit draws the same as a downloaded one, so a node
    // that sized its purchase on download alone would be shaped for the
    // difference — which is exactly what the surcharge is meant to make it feel.
    let mut policy = node_policy("https://b.example/mint");
    policy.received_multiplier = 2;

    let mut link = Link::new();
    link.b = Node::new(link.b.id, policy);
    link.connect();
    let b = link.b.id;

    // A wants 1 M/s down, and is pushing 500 k/s up.
    link.deliver(
        true,
        Event::DemandObserved {
            peer: b,
            rate: 1_000_000,
        },
    );
    link.now = Millis(1_000);
    link.deliver(
        true,
        Event::Metered {
            peer: b,
            counters: Counters {
                delivered: 500_000,
                received: 0,
            },
        },
    );
    link.deliver(
        true,
        Event::DemandObserved {
            peer: b,
            rate: 1_000_000,
        },
    );

    // 1 M down + 500 k up x 2 = 2 M/s of draw, at 125% headroom.
    assert_eq!(
        link.b_shapes_a(),
        2_500_000,
        "the purchase should cover the surcharge on what A uploads"
    );
}

#[test]
fn a_peer_that_does_not_surcharge_is_bought_for_on_download_alone() {
    let mut link = Link::new();
    link.connect();
    let (a, b) = (link.a.id, link.b.id);
    let _ = a;

    link.deliver(
        true,
        Event::DemandObserved {
            peer: b,
            rate: 1_000_000,
        },
    );
    link.now = Millis(1_000);
    link.deliver(
        true,
        Event::Metered {
            peer: b,
            counters: Counters {
                delivered: 500_000,
                received: 0,
            },
        },
    );
    link.deliver(
        true,
        Event::DemandObserved {
            peer: b,
            rate: 1_000_000,
        },
    );

    assert_eq!(
        link.b_shapes_a(),
        1_250_000,
        "at m = 0 the peer pays for our uploads out of its own grant"
    );
}

#[test]
fn a_peer_with_an_overridden_multiplier_is_offered_it_and_buys_for_it() {
    // The node-wide multiplier is 0, but B surcharges A at m = 2. The Offer is
    // how A learns that, so it has to carry the override: advertise 0 and A
    // buys for its download alone while B draws its uploads at two apiece.
    let mut link = Link::new();
    let (a, b) = (link.a.id, link.b.id);
    link.b.sessions.set_peer_policy(
        a,
        PeerPolicy {
            received_multiplier: Some(2),
            ..PeerPolicy::default()
        },
        link.now,
    );
    link.connect();

    let offer = link
        .a
        .sessions
        .peer(&b)
        .expect("session")
        .offer
        .as_ref()
        .expect("B offered");
    assert_eq!(
        offer.received_multiplier, 2,
        "the override, not the default"
    );

    // A wants 1 M/s down, and is pushing 500 k/s up.
    link.deliver(
        true,
        Event::DemandObserved {
            peer: b,
            rate: 1_000_000,
        },
    );
    link.now = Millis(1_000);
    link.deliver(
        true,
        Event::Metered {
            peer: b,
            counters: Counters {
                delivered: 500_000,
                received: 0,
            },
        },
    );
    link.deliver(
        true,
        Event::DemandObserved {
            peer: b,
            rate: 1_000_000,
        },
    );

    // 1 M down + 500 k up x 2 = 2 M/s of draw, at 125% headroom.
    assert_eq!(
        link.b_shapes_a(),
        2_500_000,
        "the purchase should cover the surcharge B actually applies"
    );
}

// ---------------------------------------------------------------------------
// Purchases that fail verification
// ---------------------------------------------------------------------------

/// The channel A pays B on, as B recognises it.
fn channel_a_pays_b_on(link: &Link) -> ChannelId {
    link.b
        .sessions
        .peer(&link.a.id)
        .expect("session")
        .grant
        .channels()[0]
        .id
}

/// A TopUp from A ratcheting one channel, as B would receive it.
fn topup_from_a(link: &Link, channel_id: ChannelId, cumulative: u64) -> Event {
    Event::MessageReceived {
        peer: link.a.id,
        msg: Message::TopUp(TopUp {
            updates: vec![ChannelUpdate {
                channel_id,
                cumulative,
                signature: Signature([0; 64]),
            }],
            window_ms: 2_000,
        }),
    }
}

/// Whether B answered with the Reject the design calls for on a TopUp that
/// failed verification.
fn rejected_as_unverified(actions: &[Action]) -> bool {
    actions.iter().any(|x| {
        matches!(
            x,
            Action::Send {
                msg: Message::Reject(Reject {
                    rejected_type,
                    reason: ReasonCode::GrantInvalid,
                    ..
                }),
                ..
            } if *rejected_type == tollgate_protocol::MsgType::TopUp as u8
        )
    })
}

#[test]
fn a_topup_whose_signature_does_not_verify_is_answered_with_reject() {
    // The host checks the signature and hands core only the verdict. The payer
    // is told rather than left to infer it from throughput that never came.
    let mut link = Link::new();
    link.connect();
    let a = link.a.id;
    let channel_id = channel_a_pays_b_on(&link);

    let actions = link.b.sessions.handle(
        Event::TopUpSignatureInvalid {
            peer: a,
            channel_id,
        },
        link.now,
    );

    assert!(rejected_as_unverified(&actions));
    assert!(
        !actions
            .iter()
            .any(|x| matches!(x, Action::SettleChannel { .. })),
        "one failure could be transient, so the channel stays open"
    );
    let grant = &link.b.sessions.peer(&a).expect("session").grant;
    assert_eq!(grant.channel(channel_id).expect("still open").failures, 1);
}

#[test]
fn a_topup_whose_total_does_not_increase_is_answered_with_reject() {
    // A replayed or reordered purchase. The design answers it with the same
    // Reject as a bad signature, not with TopUpReject — there is no rate the
    // payer could re-buy at that would fix it.
    let mut link = Link::new();
    link.connect();
    let channel_id = channel_a_pays_b_on(&link);

    link.deliver(false, topup_from_a(&link, channel_id, 10_000));
    let stale = topup_from_a(&link, channel_id, 10_000);
    let actions = link.b.sessions.handle(stale, link.now);

    assert!(rejected_as_unverified(&actions));
    assert!(
        !actions.iter().any(|x| matches!(
            x,
            Action::Send {
                msg: Message::TopUpReject(_),
                ..
            }
        )),
        "not a grant declined, a grant that failed verification"
    );
    let grant = &link.b.sessions.peer(&link.a.id).expect("session").grant;
    assert_eq!(grant.authorized(), 10_000, "the stale total bought nothing");
}

#[test]
fn a_channel_that_keeps_failing_verification_is_closed() {
    // Each attempt costs a signature verification. Past the threshold the
    // channel is no longer honored, and the last state that did verify is
    // settled so nothing already paid for is lost.
    let mut link = Link::new();
    link.connect();
    let a = link.a.id;
    let channel_id = channel_a_pays_b_on(&link);
    link.deliver(false, topup_from_a(&link, channel_id, 10_000));

    let bad = Event::TopUpSignatureInvalid {
        peer: a,
        channel_id,
    };
    for _ in 1..MAX_VERIFICATION_FAILURES {
        let actions = link.b.sessions.handle(bad.clone(), link.now);
        assert!(
            !actions
                .iter()
                .any(|x| matches!(x, Action::SettleChannel { .. }))
        );
    }
    // A stale total counts against the channel the same as a bad signature.
    let actions = link
        .b
        .sessions
        .handle(topup_from_a(&link, channel_id, 10_000), link.now);

    assert!(rejected_as_unverified(&actions));
    assert!(actions.iter().any(|x| matches!(
        x,
        Action::SettleChannel { channel_id: c, .. } if *c == channel_id
    )));
    let grant = &link.b.sessions.peer(&a).expect("session").grant;
    assert!(grant.channel(channel_id).is_none(), "no longer honored");
    assert_eq!(
        grant.authorized(),
        10_000,
        "what the channel already paid for stays bought"
    );

    // Anything further on it is refused as an unknown channel.
    let actions = link
        .b
        .sessions
        .handle(topup_from_a(&link, channel_id, 20_000), link.now);
    assert!(actions.iter().any(|x| matches!(
        x,
        Action::Send {
            msg: Message::TopUpReject(TopUpReject {
                reason: ReasonCode::FundingInvalid,
                ..
            }),
            ..
        }
    )));
}

#[test]
fn only_failures_in_a_row_count_against_a_channel() {
    // A purchase that verifies clears the count: a payer that stumbles now and
    // then is not the one the threshold is for.
    let mut link = Link::new();
    link.connect();
    let a = link.a.id;
    let channel_id = channel_a_pays_b_on(&link);

    let bad = Event::TopUpSignatureInvalid {
        peer: a,
        channel_id,
    };
    for _ in 1..MAX_VERIFICATION_FAILURES {
        link.b.sessions.handle(bad.clone(), link.now);
    }
    link.deliver(false, topup_from_a(&link, channel_id, 10_000));
    let actions = link.b.sessions.handle(bad, link.now);

    assert!(
        !actions
            .iter()
            .any(|x| matches!(x, Action::SettleChannel { .. }))
    );
    let grant = &link.b.sessions.peer(&a).expect("session").grant;
    assert_eq!(grant.channel(channel_id).expect("still open").failures, 1);
}

#[test]
fn a_channel_named_twice_fails_verification_once() {
    // Both readings increase, but the second would be counted from a stale
    // base. The purchase fails verification, and the channel is counted once.
    let mut link = Link::new();
    link.connect();
    let channel_id = channel_a_pays_b_on(&link);

    let update = |cumulative| ChannelUpdate {
        channel_id,
        cumulative,
        signature: Signature([0; 64]),
    };
    let twice = Event::MessageReceived {
        peer: link.a.id,
        msg: Message::TopUp(TopUp {
            updates: vec![update(10_000), update(20_000)],
            window_ms: 2_000,
        }),
    };
    let actions = link.b.sessions.handle(twice, link.now);

    assert!(rejected_as_unverified(&actions));
    let grant = &link.b.sessions.peer(&link.a.id).expect("session").grant;
    assert_eq!(grant.channel(channel_id).expect("still open").failures, 1);
    assert_eq!(grant.authorized(), 0, "refused as a whole");
}

#[test]
fn a_declined_grant_is_not_counted_against_the_channel() {
    // A window we will not sell is a purchase we decline, not one that failed
    // verification: it keeps TopUpReject and costs the channel nothing.
    let mut link = Link::new();
    link.connect();
    let channel_id = channel_a_pays_b_on(&link);

    let actions = link.b.sessions.handle(
        Event::MessageReceived {
            peer: link.a.id,
            msg: Message::TopUp(TopUp {
                updates: vec![ChannelUpdate {
                    channel_id,
                    cumulative: 10_000,
                    signature: Signature([0; 64]),
                }],
                window_ms: 60_000,
            }),
        },
        link.now,
    );

    assert!(!rejected_as_unverified(&actions));
    assert!(actions.iter().any(|x| matches!(
        x,
        Action::Send {
            msg: Message::TopUpReject(TopUpReject {
                reason: ReasonCode::WindowOutOfRange,
                ..
            }),
            ..
        }
    )));
    let grant = &link.b.sessions.peer(&link.a.id).expect("session").grant;
    assert_eq!(grant.channel(channel_id).expect("still open").failures, 0);
}

// ---------------------------------------------------------------------------
// Channel expiry and sizing
// ---------------------------------------------------------------------------

/// One hour, the design's default TTL.
const TTL_MS: u64 = 3_600_000;

#[test]
fn a_slowly_drawn_channel_rolls_over_before_expiry_and_is_settled_in_time() {
    // The case the safety margin exists for: a channel drawn far too slowly to
    // fill before its expiry. Rolling over on capacity alone, it would outlive
    // its TTL and the funder could reclaim what it had already paid.
    let mut link = Link::new();
    link.a.ttl_ms = Some(TTL_MS);
    link.b.ttl_ms = Some(TTL_MS);
    link.connect();
    let (a, b) = (link.a.id, link.b.id);

    link.deliver(
        true,
        Event::DemandObserved {
            peer: b,
            rate: 1_000,
        },
    );
    let first = link
        .a
        .sessions
        .peer(&b)
        .and_then(|s| s.buyer.active())
        .expect("A pays B on a channel")
        .id;
    assert_eq!(link.a.funded.len(), 1);

    // `max(60 s, 2 × 30 s)` before expiry, and not a moment sooner.
    let margin = link.a.sessions.node_policy().safety_margin_ms(30_000);
    assert_eq!(margin, 60_000);
    link.advance(TTL_MS - margin - 1);
    assert_eq!(link.a.funded.len(), 1, "outside the margin: nothing to do");

    link.advance(1);
    assert_eq!(link.a.funded.len(), 2, "inside it: a replacement is funded");
    assert_eq!(
        link.a.funded[1], link.a.funded[0],
        "a channel that ran out of time was big enough; it does not grow"
    );

    // The replacement is confirmed in the same pump, and A moves onto it at
    // once rather than draining a channel B is about to settle.
    link.deliver(
        true,
        Event::DemandObserved {
            peer: b,
            rate: 1_000,
        },
    );
    let now_using = link
        .a
        .sessions
        .peer(&b)
        .and_then(|s| s.buyer.active())
        .expect("A still pays B")
        .id;
    assert_ne!(now_using, first, "A has moved onto the replacement");

    // B, the receiver, settles the old channel ahead of expiry — half the
    // margin, leaving a window's worth of time to retry a failed settlement.
    let settle_at = TTL_MS - link.b.sessions.node_policy().settle_lead_ms(30_000);
    link.advance(settle_at - 1 - link.now.0);
    assert!(!link.b.settled.contains(&first), "not settled early");
    link.advance(1);
    assert!(
        link.b.settled.contains(&first),
        "settled before the funder could reclaim it"
    );
    assert!(link.now < Millis(TTL_MS));
    assert_eq!(
        link.b.settle_deadlines[&first],
        Some(Millis(TTL_MS)),
        "and asked to retry it no later than the channel's expiry"
    );

    let b_view = link.b.sessions.peer(&a).expect("session");
    assert!(
        b_view.grant.channel(first).is_none(),
        "and no longer honored"
    );
    assert!(b_view.grant.channel(now_using).is_some());
    assert_eq!(
        link.b.access.get(&a),
        Some(&AccessLevel::Active),
        "the peer never lost service over it"
    );
}

#[test]
fn a_channel_that_fills_up_is_replaced_by_a_bigger_one_up_to_the_cap() {
    // Start small, grow with the relationship: every rollover forced by use
    // doubles the next channel, and the operator's ceiling stops it.
    let small = NodePolicy {
        initial_channel_capacity: 1_000_000,
        max_channel_capacity: 3_000_000,
        ..node_policy("https://a.example/mint")
    };
    let mut link = Link::new();
    link.a = Node::new(pubkey(0xA1), small);
    link.connect();
    let b = link.b.id;

    for _ in 0..40 {
        link.deliver(
            true,
            Event::DemandObserved {
                peer: b,
                rate: 320_000,
            },
        );
        link.advance(1_000);
    }

    assert!(link.a.funded.len() >= 4, "funded {:?}", link.a.funded);
    assert_eq!(
        link.a.funded[..4],
        [1_000_000, 2_000_000, 3_000_000, 3_000_000],
        "doubling, clamped to max_channel_capacity"
    );
}

#[test]
fn a_rollover_funds_one_replacement_however_long_the_wallet_takes() {
    // Funding is a mint round trip, many ticks long. Marked as under way only
    // once the channel came back, every tick in between funded another: three
    // or four replacements per rollover, measured.
    let small = NodePolicy {
        initial_channel_capacity: 1_000_000,
        max_channel_capacity: 1_000_000,
        ..node_policy("https://a.example/mint")
    };
    let mut link = Link::new();
    link.a = Node::new(pubkey(0xA1), small);
    link.connect();
    let b = link.b.id;
    assert_eq!(link.a.funded.len(), 1);
    link.a.defer_funding = true;

    // The first purchase takes the channel to 80%, and a replacement is due.
    link.deliver(
        true,
        Event::DemandObserved {
            peer: b,
            rate: 320_000,
        },
    );
    for _ in 0..50 {
        link.advance(100);
    }
    assert_eq!(
        link.a.funded.len(),
        2,
        "five seconds of ticks, one replacement asked for"
    );

    // The wallet fails. The rollover is still due, so it is asked for again,
    // and again only once.
    let (_, request, _) = link.a.held_funding.pop_back().expect("held");
    link.a.held_funding.clear();
    link.deliver(true, Event::OutgoingFundingFailed { peer: b, request });
    link.advance(100);
    assert_eq!(link.a.funded.len(), 3, "a failure frees it to try again");
    let asked = link.now;
    for _ in 0..10 {
        link.advance(100);
    }
    assert_eq!(link.a.funded.len(), 3);

    // An answer that never comes is given up on after the funding timeout.
    link.advance(asked.0 + FUNDING_TIMEOUT_MS - 1 - link.now.0);
    assert_eq!(link.a.funded.len(), 3, "not a moment before");
    link.advance(1);
    assert_eq!(link.a.funded.len(), 4, "then asked for again");

    // The wallet answers, B confirms, and A holds its one replacement. Demand
    // stops first, so nothing fills the replacement and makes the next one due.
    link.deliver(true, Event::DemandObserved { peer: b, rate: 0 });
    let (peer, request, capacity) = link.a.held_funding.pop_back().expect("held");
    let done = link.a.funding_done(peer, request, capacity, link.now);
    let Event::OutgoingChannelFunded { channel_id, .. } = done else {
        unreachable!()
    };
    link.deliver(true, done);
    let session = link.a.sessions.peer(&b).expect("session");
    assert!(!session.buyer.awaiting_confirmation(), "B confirmed it");
    // The first channel filled long ago, so it takes over at once.
    assert_eq!(session.buyer.active().map(|c| c.id), Some(channel_id));
    for _ in 0..50 {
        link.advance(100);
    }
    assert_eq!(link.a.funded.len(), 4, "and asks for no other");
}

#[test]
fn a_late_answer_to_a_request_given_up_on_is_taken_or_reclaimed_but_never_both() {
    // A request given up on after the funding timeout is not cancelled — the
    // wallet may still be working on it — so a slow one and the one asked for
    // in its place can both come back. Taking both opened a second channel on
    // top of the first and stranded one of them; the first back is taken,
    // and the other is handed back to be reclaimed.
    let small = NodePolicy {
        initial_channel_capacity: 1_000_000,
        max_channel_capacity: 1_000_000,
        ..node_policy("https://a.example/mint")
    };
    let mut link = Link::new();
    link.a = Node::new(pubkey(0xA1), small);
    link.connect();
    let b = link.b.id;
    let a = link.a.id;
    let opened = link
        .a
        .sessions
        .peer(&b)
        .expect("session")
        .buyer
        .active()
        .map(|c| c.id);
    link.a.defer_funding = true;

    // A purchase takes the channel to 80%, and a replacement is asked for.
    link.deliver(
        true,
        Event::DemandObserved {
            peer: b,
            rate: 320_000,
        },
    );
    link.deliver(true, Event::DemandObserved { peer: b, rate: 0 });
    link.advance(100);
    assert_eq!(link.a.held_funding.len(), 1);

    // The wallet is slow. The request is given up on and asked again.
    link.advance_to(link.now + FUNDING_TIMEOUT_MS);
    assert_eq!(
        link.a.held_funding.len(),
        2,
        "asked again after the timeout"
    );
    let (_, slow, capacity) = link.a.held_funding.pop_front().expect("held");
    let (_, again, _) = link.a.held_funding.pop_front().expect("held");
    assert!(again > slow, "a later request has the larger id");

    // The slow one lands after all. It is taken: one replacement was wanted,
    // and here it is.
    let done = link.a.funding_done(b, slow, capacity, link.now);
    let Event::OutgoingChannelFunded {
        channel_id: taken, ..
    } = done
    else {
        unreachable!()
    };
    link.deliver(true, done);
    let session = link.a.sessions.peer(&b).expect("session");
    assert_eq!(session.buyer.next_channel().map(|c| c.id), Some(taken));
    assert_eq!(
        session.buyer.active().map(|c| c.id),
        opened,
        "still draining the first"
    );

    // Then the one asked for in its place. Not a second replacement: B hears
    // nothing of it, and A hands it back to be reclaimed.
    let done = link.a.funding_done(b, again, capacity, link.now);
    let Event::OutgoingChannelFunded {
        channel_id: spare, ..
    } = done
    else {
        unreachable!()
    };
    let actions = link.a.sessions.handle(done, link.now);
    assert_eq!(
        actions,
        vec![Action::ReclaimChannel {
            peer: b,
            channel_id: spare,
            capacity,
            expires_at: None,
        }],
        "reclaimed, and nothing sent"
    );
    let session = link.a.sessions.peer(&b).expect("session");
    assert_eq!(session.buyer.next_channel().map(|c| c.id), Some(taken));
    assert_eq!(
        link.b
            .sessions
            .peer(&a)
            .expect("session")
            .grant
            .channels()
            .len(),
        2,
        "B knows the channel in use and its one replacement"
    );

    // Nor does a late answer overwrite a replacement still awaiting the
    // peer's confirmation. Here the peer has not confirmed yet, and the
    // request is answered twice over.
    let mut buyer = crate::buyer::Buyer::new();
    buyer.funding_requested(1, Millis(0));
    buyer.funding_requested(2, Millis(FUNDING_TIMEOUT_MS));
    assert!(buyer.answers(1));
    buyer.funded(ChannelId([1; 32]), 1, None);
    assert!(!buyer.answers(2), "`pending` is never overwritten");
}

#[test]
fn the_default_sizes_start_at_one_proof_and_double_to_the_ceiling() {
    // 1 GiB, 2, 4, 8, 16 — and 16 from then on, however long the peering.
    let policy = NodePolicy::default();
    let mut capacity = policy.first_channel_capacity();
    let mut sizes = vec![capacity];
    for _ in 0..5 {
        capacity = policy.grown_capacity(capacity);
        sizes.push(capacity);
    }
    assert_eq!(
        sizes,
        [1 << 30, 1 << 31, 1 << 32, 1 << 33, 1 << 34, 1 << 34],
        "powers of two, clamped to max_channel_capacity"
    );
}

#[test]
fn channel_sizes_are_clamped_to_the_operators_bounds() {
    let policy = NodePolicy {
        initial_channel_capacity: 10,
        min_channel_capacity: 1_000,
        max_channel_capacity: 5_000,
        capacity_growth_pct: 300,
        ..NodePolicy::default()
    };
    assert_eq!(
        policy.first_channel_capacity(),
        1_000,
        "raised to the floor"
    );
    assert_eq!(policy.grown_capacity(1_000), 3_000);
    assert_eq!(policy.grown_capacity(3_000), 5_000, "held at the ceiling");
    assert_eq!(
        policy.grown_capacity(u64::MAX),
        5_000,
        "and the arithmetic cannot overflow on the way there"
    );

    let flat = NodePolicy {
        capacity_growth_pct: 100,
        ..policy
    };
    assert_eq!(flat.grown_capacity(2_000), 2_000, "100% never grows");
}

#[test]
fn the_safety_margin_is_a_minute_or_two_windows_whichever_is_longer() {
    let policy = NodePolicy::default();
    assert_eq!(policy.safety_margin_ms(30_000), 60_000);
    assert_eq!(policy.safety_margin_ms(10_000), 60_000, "the floor");
    assert_eq!(policy.safety_margin_ms(45_000), 90_000, "two windows");
    assert_eq!(policy.settle_lead_ms(45_000), 45_000, "the receiver's half");
}

// ---------------------------------------------------------------------------
// Dropping and coming back
// ---------------------------------------------------------------------------

/// The channel each side pays the other on.
fn channels_in_use(link: &Link) -> (ChannelId, ChannelId) {
    let (a, b) = (link.a.id, link.b.id);
    let a_pays_on = link.a.sessions.peer(&b).expect("session").buyer.active();
    let b_pays_on = link.b.sessions.peer(&a).expect("session").buyer.active();
    (
        a_pays_on.expect("A pays on a channel").id,
        b_pays_on.expect("B pays on a channel").id,
    )
}

#[test]
fn a_peer_that_blips_and_comes_back_in_time_resumes_its_channels() {
    // A Wi-Fi blip: both transports go, nobody said Disconnect, and both sides
    // still hold everything. Funding two new channels to carry on would be
    // pure waste.
    let mut link = Link::with_grace(60_000);
    link.connect();
    let (a, b) = (link.a.id, link.b.id);

    link.deliver(
        true,
        Event::DemandObserved {
            peer: b,
            rate: 1_000_000,
        },
    );
    assert_eq!(link.b_shapes_a(), 1_250_000);
    let (a_pays_on, b_pays_on) = channels_in_use(&link);
    let signed_before = link.b.sessions.peer(&a).expect("session").grant.channels()[0].signed;
    assert_eq!((link.a.funded.len(), link.b.funded.len()), (1, 1));

    link.blip();
    assert!(link.a.sessions.peer(&b).is_none(), "the link is gone");
    assert!(
        link.a.sessions.parked(&b).is_some(),
        "but its state is held"
    );
    assert!(link.b.sessions.parked(&a).is_some());

    link.advance(5_000);
    link.connect();

    assert_eq!(
        (link.a.funded.len(), link.b.funded.len()),
        (1, 1),
        "neither side funded anything new"
    );
    assert_eq!(channels_in_use(&link), (a_pays_on, b_pays_on));
    assert!(
        link.a.settled.is_empty() && link.b.settled.is_empty(),
        "nothing was given up, so nothing was settled"
    );

    // A picks up buying on the channel it already had, from where it left off.
    link.deliver(
        true,
        Event::DemandObserved {
            peer: b,
            rate: 1_000_000,
        },
    );
    let b_to_a = link.b.sessions.peer(&a).expect("session");
    assert_eq!(b_to_a.grant.channels().len(), 1);
    assert_eq!(b_to_a.grant.channels()[0].id, a_pays_on);
    assert!(
        b_to_a.grant.channels()[0].signed > signed_before,
        "the ratchet carried on rather than starting over"
    );
    assert_eq!(link.b_shapes_a(), 1_250_000);
    assert_eq!(link.b.access.get(&a), Some(&AccessLevel::Active));
    assert_eq!(
        link.a.sessions.peer(&b).expect("session").phase,
        Phase::Established
    );
}

#[test]
fn a_grant_does_not_outlive_the_session_it_was_bought_in() {
    // A new connection is a new session, and a session starts with the grant
    // zeroed. The channels are what is worth keeping; a grant is at most one
    // window, and the payer buys again the moment the Offer is in.
    let mut link = Link::with_grace(60_000);
    link.connect();
    let (a, b) = (link.a.id, link.b.id);

    link.deliver(
        true,
        Event::DemandObserved {
            peer: b,
            rate: 1_000_000,
        },
    );
    link.blip();
    let b_to_a = link.b.sessions.parked(&a).expect("held");
    assert!(b_to_a.grant.started());

    // Demand is a reading of the old link, so the new one starts without it
    // and nothing is bought until the host reports some.
    link.connect();

    let b_to_a = link.b.sessions.peer(&a).expect("session");
    assert!(!b_to_a.grant.started(), "the grant went with the session");
    assert_eq!(link.b_shapes_a(), 4_096, "back to the allowance");
    assert_eq!(
        link.b.access.get(&a),
        Some(&AccessLevel::Active),
        "but the channel is still open"
    );
}

#[test]
fn a_peer_that_comes_back_too_late_starts_over_with_new_channels() {
    let mut link = Link::with_grace(60_000);
    link.connect();
    let (a, b) = (link.a.id, link.b.id);
    let (a_pays_on, b_pays_on) = channels_in_use(&link);

    link.blip();
    link.advance(60_001);

    assert!(link.a.sessions.parked(&b).is_none(), "the grace ran out");
    assert!(link.b.sessions.parked(&a).is_none());
    assert_eq!(link.a.settled, vec![b_pays_on], "A claims what B paid it");
    assert_eq!(link.b.settled, vec![a_pays_on], "B claims what A paid it");

    link.connect();

    assert_eq!(
        (link.a.funded.len(), link.b.funded.len()),
        (2, 2),
        "one new channel each"
    );
    let (a_now, b_now) = channels_in_use(&link);
    assert_ne!(a_now, a_pays_on);
    assert_ne!(b_now, b_pays_on);
    assert_eq!(link.b.access.get(&a), Some(&AccessLevel::Active));
}

#[test]
fn an_orderly_disconnect_ends_the_session_and_holds_nothing() {
    // Disconnect says the peer is going on purpose. There is nothing to come
    // back to, so the session goes as soon as the transport does.
    let mut link = Link::with_grace(60_000);
    link.connect();
    let (a, b) = (link.a.id, link.b.id);

    for action in link.b.sessions.shutdown() {
        if let Action::Send { msg, .. } = action {
            link.deliver(true, Event::MessageReceived { peer: b, msg });
        }
    }
    let (_, b_pays_on) = channels_in_use(&link);
    link.blip();

    assert!(link.a.sessions.peer(&b).is_none());
    assert!(
        link.a.sessions.parked(&b).is_none(),
        "A holds nothing for B"
    );
    assert_eq!(
        link.a.settled,
        vec![b_pays_on],
        "A claims what B paid it as soon as B has gone"
    );
    assert!(
        link.b.sessions.parked(&a).is_none(),
        "B holds nothing for A"
    );

    link.connect();
    assert_eq!(
        (link.a.funded.len(), link.b.funded.len()),
        (2, 2),
        "one new channel each"
    );
}

#[test]
fn a_peer_that_comes_back_without_its_state_gets_fresh_channels() {
    // The other side of the friendly path: A lost everything — a restart
    // inside B's grace period — while B still holds both channels. A cannot
    // resume what it does not remember, so both directions start over, and B
    // settles the channel A paid it on rather than holding it for nobody. The
    // one B paid A on is not B's to settle: only a receiver can.
    let mut link = Link::with_grace(60_000);
    link.connect();
    let (a, b) = (link.a.id, link.b.id);
    let (a_pays_on, b_pays_on) = channels_in_use(&link);

    link.blip();
    link.a = Node::new(a, grace_policy("https://a.example/mint", 60_000));
    // Fresh channel ids, as a real backend's nonce would give.
    link.a.next_channel = 0x50;
    link.connect();

    assert_eq!(link.a.funded.len(), 1, "A funds the one channel it pays on");
    assert_eq!(
        link.b.funded.len(),
        2,
        "B could not resume, so it funds again"
    );
    assert!(
        link.b.settled.contains(&a_pays_on),
        "B claims what A paid it"
    );
    assert_eq!(
        link.b.settled,
        vec![a_pays_on],
        "B lets its own go unsettled"
    );

    let (a_now, b_now) = channels_in_use(&link);
    assert_ne!(a_now, a_pays_on);
    assert_ne!(b_now, b_pays_on);
    let b_to_a = link.b.sessions.peer(&a).expect("session");
    assert_eq!(b_to_a.grant.channels().len(), 1, "only the new one");
    assert_eq!(b_to_a.grant.channels()[0].id, a_now);

    // And both can buy.
    link.deliver(
        true,
        Event::DemandObserved {
            peer: b,
            rate: 1_000_000,
        },
    );
    link.deliver(
        false,
        Event::DemandObserved {
            peer: a,
            rate: 250_000,
        },
    );
    assert_eq!(link.b_shapes_a(), 1_250_000);
    assert_eq!(link.a_shapes_b(), 312_500);
}

#[test]
fn a_second_connection_while_the_first_still_looks_live_resumes_it() {
    // The old transport died but its FIN has not been noticed yet, so the new
    // connection arrives while the session is still up. It is the same peer
    // coming back, and it resumes exactly as a parked one would.
    let mut link = Link::with_grace(60_000);
    link.connect();
    let (a, b) = (link.a.id, link.b.id);
    let (a_pays_on, b_pays_on) = channels_in_use(&link);

    link.connect();

    assert_eq!(
        (link.a.funded.len(), link.b.funded.len()),
        (1, 1),
        "neither side funded anything new"
    );
    assert_eq!(channels_in_use(&link), (a_pays_on, b_pays_on));
    assert!(link.a.settled.is_empty() && link.b.settled.is_empty());
    assert!(link.a.sessions.parked(&b).is_none());
    assert!(link.b.sessions.parked(&a).is_none());
}

#[test]
fn a_peer_that_comes_back_late_before_the_tick_notices_starts_over() {
    // The grace ran out but no tick has expired the session yet. The new
    // connection must not resume it: the channels are settled there and then.
    let mut link = Link::with_grace(60_000);
    link.connect();
    let (a, b) = (link.a.id, link.b.id);
    let (a_pays_on, b_pays_on) = channels_in_use(&link);

    link.blip();
    link.now = link.now + 60_001;
    link.connect();

    assert_eq!(link.a.settled, vec![b_pays_on], "A claims what B paid it");
    assert_eq!(link.b.settled, vec![a_pays_on], "B claims what A paid it");
    assert_eq!(
        (link.a.funded.len(), link.b.funded.len()),
        (2, 2),
        "one new channel each"
    );
    assert!(link.a.sessions.parked(&b).is_none());
    assert!(link.b.sessions.parked(&a).is_none());
}

#[test]
fn without_a_stale_timeout_nothing_is_held_and_the_channels_are_settled() {
    // Zero switches the timeout off, and with it the grace: holding state
    // forever for a peer that never returns would be a leak.
    let mut link = Link::new();
    link.connect();
    let (a, b) = (link.a.id, link.b.id);
    let (a_pays_on, b_pays_on) = channels_in_use(&link);

    link.blip();

    assert!(link.a.sessions.parked(&b).is_none());
    assert!(link.b.sessions.parked(&a).is_none());
    assert_eq!(link.a.settled, vec![b_pays_on]);
    assert_eq!(link.b.settled, vec![a_pays_on]);
}

#[test]
fn a_silent_peer_is_held_like_one_that_dropped() {
    // Silence is an unclean disconnect too: a link that died without a FIN
    // looks exactly like this, so the peer can still come back to its channels.
    let mut link = Link::with_grace(5_000);
    link.connect();
    let (a, b) = (link.a.id, link.b.id);
    let (a_pays_on, b_pays_on) = channels_in_use(&link);

    link.now = Millis(5_001);
    let actions = link.b.sessions.handle(Event::Tick, link.now);
    assert!(actions.iter().any(|x| matches!(x, Action::DropPeer { .. })));
    assert!(link.b.sessions.peer(&a).is_none());
    assert!(link.b.sessions.parked(&a).is_some(), "held, not dropped");

    // A notices the drop too, and both come back inside the grace.
    link.deliver(true, Event::PeerDisconnected { peer: b });
    link.connect();

    assert_eq!((link.a.funded.len(), link.b.funded.len()), (1, 1));
    assert_eq!(channels_in_use(&link), (a_pays_on, b_pays_on));
}

#[test]
fn shutting_down_settles_what_a_held_peer_paid_us() {
    // A peer held after a blip will not find us when it comes back, so what it
    // paid us is claimed on the way out or not at all.
    let mut link = Link::with_grace(60_000);
    link.connect();
    let a = link.a.id;
    let (a_pays_on, _) = channels_in_use(&link);

    link.blip();
    assert!(link.b.sessions.parked(&a).is_some());

    let actions = link.b.sessions.shutdown();
    assert!(
        actions.iter().any(|x| matches!(
            x,
            Action::SettleChannel { peer, channel_id, .. } if *peer == a && *channel_id == a_pays_on
        )),
        "the channel A paid us on still holds value we can claim"
    );
    assert!(link.b.sessions.parked(&a).is_none());
}

#[test]
fn a_held_channel_is_settled_when_its_expiry_comes_round_inside_the_grace() {
    // A grace period may outlast what is left of a channel. Holding it past its
    // settle point would let the payer take back through the refund path what
    // it already paid us on it, so it is settled on the same clock as a live
    // one — and, no longer held, it is not resumed.
    let mut link = Link::with_grace(TTL_MS);
    link.a.ttl_ms = Some(TTL_MS);
    link.b.ttl_ms = Some(TTL_MS);
    link.connect();
    let (a, b) = (link.a.id, link.b.id);
    let (a_pays_on, b_pays_on) = channels_in_use(&link);

    link.blip();
    let settle_at = TTL_MS - link.b.sessions.node_policy().settle_lead_ms(30_000);
    link.advance(settle_at - 1 - link.now.0);
    assert!(link.a.settled.is_empty() && link.b.settled.is_empty());

    link.advance(1);
    assert_eq!(link.b.settled, vec![a_pays_on], "B claims what A paid it");
    assert_eq!(link.a.settled, vec![b_pays_on], "A claims what B paid it");
    assert_eq!(
        link.b.settle_deadlines[&a_pays_on],
        Some(Millis(TTL_MS)),
        "retried no later than the channel's expiry"
    );
    let held = link.b.sessions.parked(&a).expect("still inside the grace");
    assert!(held.grant.channels().is_empty(), "but no longer held");
    assert!(link.a.sessions.parked(&b).is_some());

    // Back inside the grace, but with nothing left to resume on: one new
    // channel each, and nothing settled twice.
    link.connect();
    assert_eq!((link.a.funded.len(), link.b.funded.len()), (2, 2));
    assert_eq!((link.a.settled.len(), link.b.settled.len()), (1, 1));
    let (a_now, b_now) = channels_in_use(&link);
    assert_ne!(a_now, a_pays_on);
    assert_ne!(b_now, b_pays_on);
}

#[test]
fn a_resumed_channel_starts_its_verification_failures_over() {
    // Failures count in a row on one connection. A payer that lost track of
    // its total is exactly the one that reconnects, so the new session starts
    // clean rather than one bad TopUp from losing the channel.
    let mut link = Link::with_grace(60_000);
    link.connect();
    let a = link.a.id;
    let (a_pays_on, _) = channels_in_use(&link);

    let bad = Event::TopUpSignatureInvalid {
        peer: a,
        channel_id: a_pays_on,
    };
    for _ in 1..MAX_VERIFICATION_FAILURES {
        link.b.sessions.handle(bad.clone(), link.now);
    }
    let failures =
        |link: &Link| link.b.sessions.peer(&a).expect("session").grant.channels()[0].failures;
    assert_eq!(failures(&link), MAX_VERIFICATION_FAILURES - 1);

    link.blip();
    link.connect();
    assert_eq!(channels_in_use(&link).0, a_pays_on, "resumed");
    assert_eq!(failures(&link), 0);

    let actions = link.b.sessions.handle(bad, link.now);
    assert!(
        !actions
            .iter()
            .any(|x| matches!(x, Action::SettleChannel { .. })),
        "one failure on a clean slate does not close the channel"
    );
}

#[test]
fn a_peer_that_stops_charging_while_we_were_away_lets_the_resumed_channel_go() {
    // B decided not to charge A while A was away. The Offer on the resumed
    // session says so, and A has nothing to buy: it funds nothing, pays on
    // nothing, and its empty Accept tells B to settle what A had signed.
    let mut link = Link::with_grace(60_000);
    link.connect();
    let (a, b) = (link.a.id, link.b.id);
    let (a_pays_on, b_pays_on) = channels_in_use(&link);

    link.blip();
    link.b.sessions.set_peer_policy(
        a,
        PeerPolicy {
            no_charge: true,
            ..PeerPolicy::default()
        },
        link.now,
    );
    link.connect();

    let a_to_b = link.a.sessions.peer(&b).expect("session");
    assert!(a_to_b.offer.as_ref().expect("B offered").no_charge);
    assert!(a_to_b.buyer.active().is_none(), "nothing to pay on");
    assert_eq!(link.a.funded.len(), 1, "and nothing funded");
    assert_eq!(link.b.settled, vec![a_pays_on], "B claims what A paid it");
    assert_eq!(link.b.access.get(&a), Some(&AccessLevel::Free));

    // The other direction is untouched: B still pays A on the channel it had.
    assert_eq!(link.b.funded.len(), 1);
    assert_eq!(
        link.b
            .sessions
            .peer(&a)
            .expect("session")
            .buyer
            .active()
            .map(|c| c.id),
        Some(b_pays_on)
    );
}
