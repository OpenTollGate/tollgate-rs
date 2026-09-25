//! Things that happened, as the host reports them.
//!
//! Core never observes anything itself. The host turns sockets, timers, wallet
//! callbacks and meter readings into these, feeds them in, and executes the
//! [`Action`](crate::Action)s that come back.

use alloc::string::String;
use alloc::vec::Vec;

use tollgate_protocol::{ChannelId, Message, PubKey, ReasonCode};

use crate::meter::Counters;

/// An input to the session state machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// A peer's transport came up and the layer underneath authenticated it.
    /// Nothing is delivered for it yet.
    PeerConnected {
        /// The peer.
        peer: PubKey,
    },

    /// A peer's transport went away, cleanly or otherwise.
    PeerDisconnected {
        /// The peer.
        peer: PubKey,
    },

    /// A TollGate message arrived.
    ///
    /// **The host has already verified any signature it carries.** Core decides
    /// what is owed, never whether the peer is who it claims to be — exactly as
    /// FIPS terminates Noise before the protocol layer sees anything.
    MessageReceived {
        /// Who sent it.
        peer: PubKey,
        /// What they sent.
        msg: Message,
    },

    /// A TopUp arrived carrying a signature that did not verify.
    ///
    /// The host checked it — core never does — and the message itself goes no
    /// further, since nothing in an unauthentic purchase can be acted on. What
    /// comes through is only enough to answer it: the payer is told, and a
    /// channel that keeps failing stops being honored.
    ///
    /// Verification is only the signature (and, for Spilman, that the channel
    /// is open and the total fits its capacity). An update whose total does not
    /// increase verifies, reaches core in the TopUp, and is refused there as
    /// `GrantInvalid`, which core answers and counts the same way.
    TopUpSignatureInvalid {
        /// Who sent it.
        peer: PubKey,
        /// The channel whose update failed. A purchase is refused as a whole,
        /// so the host may stop at the first bad signature.
        channel_id: ChannelId,
    },

    /// Our wallet finished funding the channel we pay this peer on, in response
    /// to [`Action::FundChannel`](crate::Action::FundChannel).
    OutgoingChannelFunded {
        /// The peer we will pay on it.
        peer: PubKey,
        /// The channel.
        channel_id: ChannelId,
        /// Units it can carry before it must roll over.
        capacity: u64,
        /// Opaque funding blob to put in the Accept, interpreted by whatever
        /// channel backend produced it.
        funding: Vec<u8>,
    },

    /// Our wallet verified the funding a peer sent, in response to
    /// [`Action::VerifyFunding`](crate::Action::VerifyFunding).
    IncomingFundingVerified {
        /// The peer that funded it.
        peer: PubKey,
        /// The channel they will pay us on.
        channel_id: ChannelId,
        /// Units it can carry.
        capacity: u64,
        /// The mint the channel is funded in. Core refuses the channel unless
        /// it is one of our accepted mints, whatever the backend checked.
        mint_url: String,
    },

    /// Our wallet could not verify a peer's funding.
    IncomingFundingRejected {
        /// The peer whose funding failed.
        peer: PubKey,
        /// Why, as the peer will be told: [`ReasonCode::MintNotAccepted`] when
        /// the backend refused the mint, [`ReasonCode::FundingInvalid`] for
        /// anything else.
        reason: ReasonCode,
    },

    /// Fresh cumulative meter readings for a peer.
    Metered {
        /// The peer.
        peer: PubKey,
        /// Cumulative units since session start, both directions.
        counters: Counters,
    },

    /// What this node currently wants to move over a link, in units per second.
    ///
    /// This is what drives [`buyer`](crate::buyer). How it is measured is the
    /// host's business — offered load, recent throughput, queue depth.
    DemandObserved {
        /// The peer we would be buying from.
        peer: PubKey,
        /// Units per second wanted.
        rate: u64,
    },

    /// Time passed. Drives deadline expiry, renewals and rollover.
    Tick,
}
