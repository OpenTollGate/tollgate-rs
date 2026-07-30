//! Per-peer session state.
//!
//! Two payment streams live here, and they are **independent**: the channel the
//! peer pays us on and the channel we pay the peer on. Different mints,
//! different windows, bought at different moments, neither waiting for the
//! other. That is why nearly everything on this struct comes in pairs.

use alloc::string::String;
use alloc::vec::Vec;

use tollgate_protocol::{ChannelId, PubKey};

use crate::access::AccessLevel;
use crate::buyer::{Buyer, WindowBounds};
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

/// One direction's channel, once it exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChannelSlot {
    /// The channel.
    pub id: ChannelId,
    /// Units it can carry before it must roll over.
    pub capacity: u64,
    /// Whether a rollover has already been started for it.
    pub rolling_over: bool,
}

/// What the peer advertised in its Offer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerOffer {
    /// Mints it will take payment in, most preferred first.
    pub accepted_mints: Vec<String>,
    /// Its quantity unit.
    pub unit: String,
    /// Window bounds it will accept from us.
    pub bounds: WindowBounds,
    /// Its surcharge on what we push at it. We do not enforce this — the peer
    /// does, against our grant — but it is worth holding for the operator to
    /// see why a link is costing what it costs.
    pub received_multiplier: u16,
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

    // --- the stream where they pay us -------------------------------------
    /// The channel they pay us on.
    pub incoming: Option<ChannelSlot>,
    /// What they have bought and drawn.
    pub grant: GrantState,
    /// Cumulative counters, raw. The multiplier is applied when drawing down.
    pub meter: Meter,

    // --- the stream where we pay them --------------------------------------
    /// The channel we pay them on.
    pub outgoing: Option<ChannelSlot>,
    /// Our side of it.
    pub buyer: Buyer,
    /// Units per second we last observed ourselves wanting over this link.
    pub demand: u64,

    /// Shaping rate last handed to the adapter, so we only emit a change when
    /// it actually changes. A shaper call per meter reading would be a lot of
    /// churn for a number that mostly stays put.
    pub applied_rate: Option<u64>,
    /// When we last heard anything at all from them.
    pub last_seen: Millis,
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
            incoming: None,
            grant: GrantState::new(),
            meter: Meter::new(),
            outgoing: None,
            buyer: Buyer::new(),
            demand: 0,
            applied_rate: None,
            last_seen: now,
        }
    }

    /// Whether the peer has a live grant with us right now.
    pub fn paying(&self, now: Millis) -> bool {
        self.grant.is_live(now)
    }

    /// Whether the channel they pay us on is close enough to exhaustion to
    /// warrant rolling over.
    ///
    /// Rollover is initiated by the funder alone — only the party putting up
    /// new funds decides when — so this is only ever consulted for the
    /// direction we fund.
    pub fn needs_rollover(
        &self,
        slot: Option<ChannelSlot>,
        cumulative: u64,
        threshold_pct: u8,
    ) -> bool {
        let Some(slot) = slot else { return false };
        if slot.rolling_over || slot.capacity == 0 {
            return false;
        }
        // Widened to u128 rather than saturated: saturating either side would
        // make a large channel look permanently past its threshold.
        (cumulative as u128) * 100 >= (slot.capacity as u128) * (threshold_pct as u128)
    }
}
