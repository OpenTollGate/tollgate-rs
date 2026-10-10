//! Things core wants done, for the host to carry out.
//!
//! Every effect leaves through here. Core sends nothing, shapes nothing and
//! signs nothing itself — which is what makes the whole crate testable without
//! a socket, a clock or a wallet.

use alloc::string::String;
use alloc::vec::Vec;

use tollgate_protocol::{ChannelId, ChannelUpdate, Message, PubKey};

use crate::access::AccessLevel;
use crate::grant::Budget;
use crate::time::Millis;

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

    /// Sign each channel update and send them as one TopUp.
    ///
    /// Split out from [`Self::Send`] because core holds no keys: it decides
    /// *what* to sign, and the host's wallet produces a signature over each
    /// `(channel_id, cumulative)`.
    ///
    /// One action, one message: the grant is the combined increase across every
    /// ratchet here, and splitting it over several messages would leave the
    /// provider unable to tell one purchase from two.
    SignAndSendTopUp {
        /// The peer we are paying.
        peer: PubKey,
        /// Each channel to ratchet, and its new cumulative total. Strictly
        /// greater than that channel's last.
        ratchets: Vec<(ChannelId, u64)>,
        /// How long the provider is to keep the budget, already clamped to its
        /// range.
        window_ms: u64,
        /// The rate to reserve from now on, units per second; `0` for none.
        reserved_rate: u64,
    },

    /// Keep a purchase's channel updates as each channel's latest signed state.
    ///
    /// The host checked every signature before core saw the TopUp, but checking
    /// is not keeping: a purchase is honored or refused as a whole, and the
    /// channel backend's record is what settlement submits. So the updates are
    /// recorded only once core has accepted the whole purchase, and the backend
    /// never holds a state the grant did not pay for.
    ///
    /// Comes before any [`Self::SettleChannel`] the same purchase sets off, so a
    /// channel this purchase filled settles at the state recorded here.
    RecordUpdates {
        /// The peer that paid.
        peer: PubKey,
        /// The updates, exactly as the TopUp carried them.
        updates: Vec<ChannelUpdate>,
    },

    /// Change what the enforcer delivers for a peer.
    ///
    /// In FIPS the enforcer also infers reachability advertisement from this —
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
    /// Already includes the burst policy, the clip near the end of a budget and
    /// the minimum flow allowance as its floor, so the enforcer applies one
    /// number and needs to know nothing about budgets.
    SetShapingRate {
        /// The peer.
        peer: PubKey,
        /// Units per second.
        rate: u64,
    },

    /// Fund a channel to pay this peer on.
    ///
    /// Answer with [`Event::OutgoingChannelFunded`](crate::Event::OutgoingChannelFunded)
    /// or, if it cannot be done,
    /// [`Event::OutgoingFundingFailed`](crate::Event::OutgoingFundingFailed),
    /// carrying `request` back: no other channel is asked for while one of
    /// them is still owed, unless the request times out.
    FundChannel {
        /// The peer to pay.
        peer: PubKey,
        /// Which request this is, unique for the life of the node. Core can
        /// only tell a late answer to a request it has given up on from the
        /// answer to the one it asked since by this.
        request: u64,
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
        /// When the funder can reclaim it through the refund path, on the
        /// same clock as `now`, or `None` if it never expires or we do not
        /// know. Past it, a settlement is no longer worth retrying.
        expires_at: Option<Millis>,
    },

    /// Take back the funds of a channel we funded and will never use.
    ///
    /// The answer to a [`Self::FundChannel`] core no longer wanted: another
    /// request for the same peer came back first, or the peer is gone. Core
    /// has told the peer nothing about it and forgets it here, so nothing will
    /// ever be signed on it and no receiver will close it — the funds come
    /// back only through the refund path, once `expires_at` has passed.
    ReclaimChannel {
        /// The peer it was funded toward.
        peer: PubKey,
        /// The channel.
        channel_id: ChannelId,
        /// Units locked in it.
        capacity: u64,
        /// When the refund path opens, on the same clock as `now`, or `None`
        /// for a channel that never expires.
        expires_at: Option<Millis>,
    },

    /// Keep a payer's budget, beside the channel backups, so it survives a
    /// reconnect and a restart.
    ///
    /// Asked for at every TopUp core accepts, when a session ends, and when
    /// the budget reaches zero or expires — with `remaining` zero, which means
    /// the record can go. The host stores the deadline as a clock time, and
    /// hands the budget back in [`Event::PeerConnected`](crate::Event::PeerConnected).
    /// Under `enforcer.identity: address` it keys the record by the address
    /// the peer is at as well as its key.
    SaveBudget {
        /// The payer.
        peer: PubKey,
        /// What is left, and until when.
        budget: Budget,
    },

    /// Tear down the relationship with a peer.
    ///
    /// The host sends the Disconnect that core already queued, then closes.
    DropPeer {
        /// The peer to drop.
        peer: PubKey,
    },
}
