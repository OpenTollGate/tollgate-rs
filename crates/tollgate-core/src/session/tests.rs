//! Two nodes talking to each other, entirely in memory.
//!
//! This is what sans-IO buys: the full opening sequence, both payment streams,
//! admission control and the shaper all run here with no socket, no clock and
//! no wallet — the harness below stands in for all three in about eighty lines.
//! `tollgate-net` runs the same state machine against real ones.

use alloc::collections::{BTreeMap, VecDeque};
use alloc::vec;

use tollgate_protocol::{ChannelId, Message, PubKey, Signature, TopUp};

use super::*;
use crate::access::AccessLevel;
use crate::action::Action;
use crate::buyer::BuyerPolicy;
use crate::config::{GrantPolicy, NodePolicy, PeerPolicy};
use crate::event::Event;
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
}

impl Node {
    fn new(id: PubKey, policy: NodePolicy) -> Self {
        Self {
            id,
            sessions: Sessions::new(id, policy, buyer_policy()),
            shaping: BTreeMap::new(),
            access: BTreeMap::new(),
            next_channel: id.0[1],
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
    ///
    /// FIFO, because messages arrive in the order they were sent and a LIFO
    /// drain would reorder a node's own Announce behind its Offer.
    fn pump(&mut self, initial: impl IntoIterator<Item = (bool, Event)>) {
        let mut queue: VecDeque<(bool, Event)> = initial.into_iter().collect();

        while let Some((to_a, event)) = queue.pop_front() {
            let node = if to_a { &mut self.a } else { &mut self.b };
            let from = node.id;

            for action in node.sessions.handle(event, self.now) {
                match action {
                    // The wire: what one node sends, the other receives.
                    Action::Send { msg, .. } => {
                        queue.push_back((!to_a, Event::MessageReceived { peer: from, msg }))
                    }

                    // The wallet: funding always succeeds, instantly.
                    Action::FundChannel { peer, capacity, .. } => {
                        node.next_channel = node.next_channel.wrapping_add(1);
                        let channel_id = ChannelId([node.next_channel; 32]);
                        queue.push_back((
                            to_a,
                            Event::OutgoingChannelFunded {
                                peer,
                                channel_id,
                                capacity,
                                funding: vec![node.next_channel],
                            },
                        ));
                    }
                    Action::VerifyFunding { peer, funding } => queue.push_back((
                        to_a,
                        Event::IncomingFundingVerified {
                            peer,
                            channel_id: ChannelId([funding[0]; 32]),
                            capacity: CHANNEL_CAPACITY,
                        },
                    )),

                    // The signer: core decides what to sign, we produce bytes.
                    Action::SignAndSendTopUp {
                        channel_id,
                        cumulative,
                        window_ms,
                        ..
                    } => queue.push_back((
                        !to_a,
                        Event::MessageReceived {
                            peer: from,
                            msg: Message::TopUp(TopUp {
                                channel_id,
                                cumulative,
                                window_ms,
                                signature: Signature([0; 64]),
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

                    Action::SettleChannel { .. } | Action::DropPeer { .. } => {}
                }
            }
        }
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
    assert!(a_to_b.incoming.is_some(), "B pays A on this one");
    assert!(a_to_b.outgoing.is_some(), "A pays B on this one");
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
    assert_eq!(link.a_shapes_b(), 4_096, "B bought nothing, so B is unchanged");
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
    let bought = link.b.sessions.peer(&a).expect("session").grant.authorized();
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

    let grant = link.b.sessions.peer(&a).expect("session").grant;
    assert_eq!(grant.remaining(), 0);
    assert_eq!(link.b_shapes_a(), 4_096);
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
    let authorized = link.b.sessions.peer(&a).expect("session").grant.authorized();

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

    let grant = link.b.sessions.peer(&a).expect("session").grant;
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
    let authorized = link.b.sessions.peer(&link.a.id).expect("session").grant.authorized();
    let signed = link.a.sessions.peer(&b).expect("session").buyer.cumulative();
    assert_eq!(signed, authorized, "payer and provider agree on the total");
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
