//! Per-peer session state.
//!
//! Two payment streams live here, and they are **independent**: the channel the
//! peer pays us on and the channel we pay the peer on. Different mints,
//! different windows, bought at different moments, neither waiting for the
//! other. That is why nearly everything on this struct comes in pairs.

use alloc::string::String;
use alloc::vec::Vec;

use tollgate_protocol::{Balance, Offer, PubKey};

use crate::access::AccessLevel;
use crate::buyer::{Buyer, Terms};
use crate::config::PeerPolicy;
use crate::grant::GrantState;
use crate::meter::Meter;
use crate::time::Millis;

/// How far a session has got through the opening sequence.
///
/// Both peers walk it symmetrically — each sends Announce, each sends Offer,
/// each funds the channel it will pay on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Connected. We have sent our Announce and Offer; nothing has come back.
    Opening,
    /// Their Announce and Offer are in. Channels are being funded and verified.
    Establishing,
    /// At least one direction is ready. Delivery is allowed.
    Established,
    /// A Disconnect has been sent or received; the host is tearing down.
    Closing,
}

/// What the peer advertised in its Offer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerOffer {
    /// Mints it will take payment in, most preferred first.
    pub accepted_mints: Vec<String>,
    /// Its quantity unit.
    pub unit: String,
    /// What it accepts from us: windows, the smallest reserved rate, the gap
    /// between purchases, and the from-payer weight our budget with it is
    /// drawn at. The weight is the one its opening Offer carried; it is fixed
    /// for the session, so a revised Offer does not change it.
    pub terms: Terms,
    /// It will not charge us, so we fund no channel toward it and buy nothing
    /// from it. Its decision alone — it says nothing about whether we charge.
    pub no_charge: bool,
}

/// Everything we know about one peer.
#[derive(Debug, Clone)]
pub struct PeerSession {
    /// Who they are.
    pub peer: PubKey,
    /// Where we are in the opening sequence.
    pub phase: Phase,
    /// What they may have delivered for them.
    pub access: AccessLevel,
    /// Operator overrides for this peer.
    pub policy: PeerPolicy,
    /// Their Offer, once it arrives.
    pub offer: Option<PeerOffer>,
    /// The Offer we last sent them, which is what the keepalive repeats.
    ///
    /// Replayed, not rebuilt: a keepalive is meant to change nothing, and one
    /// rebuilt from the policy in force would carry an override made since as
    /// a side effect of the link being quiet.
    pub offer_sent: Option<Offer>,

    // --- the stream where they pay us -------------------------------------
    /// Their budget with us, and the channels they pay us on. The channels
    /// live here because a purchase may ratchet several at once and the grant
    /// is their combined increase.
    pub grant: GrantState,
    /// The from-payer weight their budget is drawn at: what our Offer carried
    /// when this session started. Fixed for the session, so an override made
    /// since applies from their next one.
    pub weight: u16,
    /// Cumulative counters, raw. The weight is applied when drawing down.
    pub meter: Meter,
    /// When their budget was last drawn while we carried them, so a reserved
    /// rate is drawn for the time between. `None` while we are not carrying
    /// them.
    pub drawn_at: Option<Millis>,
    /// Whether their budget had something left at the last check, so the
    /// moment it runs out or expires is noticed once: they are told with a
    /// Balance, and the host lets the record go.
    pub budget_live: bool,

    // --- the stream where we pay them --------------------------------------
    /// The last Balance they sent us about our budget with them, and when it
    /// came. Information only: what we buy is decided from our own count.
    pub balance: Option<(Balance, Millis)>,
    /// We refused their Offer — its from-payer weight is above what we buy at —
    /// so we fund nothing toward them and buy nothing from them this session.
    pub refused_terms: bool,
    /// Our side of the stream where we pay them.
    ///
    /// The channels for this direction live here rather than beside `incoming`,
    /// because the payer has to track up to three at once — the one in use, a
    /// confirmed replacement, and one awaiting confirmation — and each carries
    /// its own cumulative total.
    pub buyer: Buyer,
    /// Units per second we last observed ourselves wanting over this link.
    ///
    /// Our *download*. What we have to buy is more than that whenever the peer
    /// charges for what we push at it — see [`Self::upload_rate`].
    pub demand: u64,
    /// Units per second we have recently been pushing at this peer.
    ///
    /// The peer's from-payer weight is applied to this, and it draws down the
    /// budget we bought. A node that uploads heavily to a peer with a weight
    /// above `0` and sized its purchases on download alone would run out
    /// early.
    pub upload_rate: u64,
    /// When the meter was last sampled, so a delta can become a rate.
    pub last_meter_at: Millis,

    /// Shaping rate last handed to the enforcer, so we only emit a change when
    /// it actually changes. A shaper call per meter reading would be a lot of
    /// churn for a number that mostly stays put.
    pub applied_rate: Option<u64>,
    /// When we last heard anything at all from them.
    pub last_seen: Millis,
    /// When we last sent them anything at all, so a link we have nothing to
    /// say on is kept alive before the peer's stale timeout drops it.
    pub last_sent: Millis,
    /// Whether the peer, coming back after an unclean disconnect, said it
    /// still holds the channel we pay it on.
    ///
    /// It says so with a ChannelReady for that channel, sent between its
    /// Announce and its Offer. The Offer is where we decide whether to fund, so
    /// by then this is settled one way or the other.
    pub kept_by_peer: bool,
}

impl PeerSession {
    /// A freshly connected peer: nothing funded, nothing delivered.
    pub fn new(peer: PubKey, policy: PeerPolicy, now: Millis) -> Self {
        Self {
            peer,
            phase: Phase::Opening,
            access: AccessLevel::None,
            policy,
            offer: None,
            offer_sent: None,
            grant: GrantState::new(),
            weight: policy.from_payer_weight.unwrap_or(1),
            meter: Meter::new(),
            drawn_at: None,
            budget_live: false,
            balance: None,
            refused_terms: false,
            buyer: Buyer::new(),
            demand: 0,
            upload_rate: 0,
            last_meter_at: now,
            applied_rate: None,
            last_seen: now,
            last_sent: now,
            kept_by_peer: false,
        }
    }

    /// Start a new session over the state kept from the last one.
    ///
    /// Everything about the connection starts again — the opening sequence,
    /// what the enforcer was told, the meter and the demand, which were
    /// readings of the old link. The channels in both directions are kept, and
    /// so are the budgets, as they are into any session; the reservations are
    /// not.
    pub fn resume(&mut self, policy: PeerPolicy, now: Millis) {
        let grant = core::mem::take(&mut self.grant);
        let buyer = self.buyer;
        *self = Self {
            grant,
            buyer,
            ..Self::new(self.peer, policy, now)
        };
        self.grant.restart(now);
        self.buyer.restart();
    }

    /// Whether the peer has budget with us right now.
    pub fn paying(&self, now: Millis) -> bool {
        self.grant.is_live(now)
    }
}
