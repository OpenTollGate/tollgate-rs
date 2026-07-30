//! Things core wants done, for the host to carry out.
//!
//! Every effect leaves through here. Core sends nothing, shapes nothing and
//! signs nothing itself — which is what makes the whole crate testable without
//! a socket, a clock or a wallet.

use alloc::string::String;
use alloc::vec::Vec;

use tollgate_protocol::{ChannelId, Message, PubKey};

use crate::access::AccessLevel;

/// An effect for the host to execute.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Put a message on the wire.
    Send {
        /// Who to send it to.
        peer: PubKey,
        /// What to send.
        msg: Message,
    },

    /// Sign a channel update and send it as a TopUp.
    ///
    /// Split out from [`Self::Send`] because core holds no keys: it decides
    /// *what* to sign — the cumulative total and the window — and the host's
    /// wallet produces the signature over `(channel_id, cumulative)`.
    SignAndSendTopUp {
        /// The peer we are paying.
        peer: PubKey,
        /// The channel to ratchet.
        channel_id: ChannelId,
        /// New cumulative total. Strictly greater than the last.
        cumulative: u64,
        /// Window to spend it in, already clamped to the provider's range.
        window_ms: u32,
    },

    /// Change what the resource adapter delivers for a peer.
    ///
    /// In FIPS the adapter also infers reachability advertisement from this —
    /// see [`AccessLevel::advertise`] — so there is no second call to keep in
    /// step with it.
    SetAccess {
        /// The peer.
        peer: PubKey,
        /// Its new level.
        access: AccessLevel,
    },

    /// Shape a peer to this many units per second.
    ///
    /// Already includes the minimum flow allowance as its floor, so the adapter
    /// applies one number and needs to know nothing about grants.
    SetShapingRate {
        /// The peer.
        peer: PubKey,
        /// Units per second.
        rate: u64,
    },

    /// Fund a channel to pay this peer on.
    FundChannel {
        /// The peer to pay.
        peer: PubKey,
        /// Mint to fund against, picked from the ordered list the peer's Offer
        /// carried — the earliest entry we can actually fund in.
        mint_url: String,
        /// Units of capacity to open with.
        capacity: u64,
    },

    /// Verify the funding a peer sent, and tell us the channel it opens.
    VerifyFunding {
        /// The peer that sent it.
        peer: PubKey,
        /// The opaque blob from their Accept or RolloverInit.
        funding: Vec<u8>,
    },

    /// Settle a channel: submit the latest signed state and reclaim the change.
    SettleChannel {
        /// The counterparty.
        peer: PubKey,
        /// The channel to settle.
        channel_id: ChannelId,
    },

    /// Tear down the relationship with a peer.
    ///
    /// The host sends the Disconnect that core already queued, then closes.
    DropPeer {
        /// The peer to drop.
        peer: PubKey,
    },
}
