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
use crate::buyer::BuyerPolicy;
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
    /// Shaping rate last applied per peer — what the resource adapter would be
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
        let from = node.id;

        for action in node.sessions.handle(event, now) {
            match action {
                // The wire: what one node sends, the other receives.
                Action::Send { peer, msg } => {
                    queue.push_back((peer, Event::MessageReceived { peer: from, msg }))
                }

                // The wallet: funding always succeeds, instantly. The blob
                // carries what the other side's wallet would read out of real
                // funding: the channel, its capacity and its expiry.
                Action::FundChannel { peer, capacity, .. } => {
                    node.next_channel = node.next_channel.wrapping_add(1);
                    node.funded.push(capacity);
                    let channel_id = ChannelId([node.next_channel; 32]);
                    let expires_at = node.ttl_ms.map(|ttl| now + ttl);
                    let mut funding = vec![node.next_channel];
                    funding.extend_from_slice(&capacity.to_be_bytes());
                    funding.extend_from_slice(&expires_at.map_or(0, |e| e.0).to_be_bytes());
                    queue.push_back((
                        from,
                        Event::OutgoingChannelFunded {
                            peer,
                            channel_id,
                            capacity,
                            expires_at,
                            funding,
                        },
                    ));
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

                // The resource adapter.
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
                // The channel backend's record, which only settlement reads.
                Action::RecordUpdates { .. } | Action::DropPeer { .. } => {}
            }
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
    // so with none there is nothing left: every adapter closes the gate.
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
    link.a.sessions.set_peer_policy(b, free);
    link.b.sessions.set_peer_policy(a, free);
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
        "rate did not change, so the adapter should not have been told"
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
