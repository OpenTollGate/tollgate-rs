//! One struct per message type, plus the [`Message`] union the codec works on.

use alloc::string::String;
use alloc::vec::Vec;

use crate::types::{ChannelId, MsgType, PubKey, ReasonCode, Signature};

/// `0x00` — first message each peer sends. Peers are already authenticated by
/// the layer underneath, so this identifies rather than authenticates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Announce {
    /// Protocol version. Must match on both sides.
    pub version: u8,
    /// Sender's compressed secp256k1 public key.
    pub pubkey: PubKey,
    /// Quantity unit this node denominates in — `"byte"`, `"wh"`, `"ml"`.
    pub unit: String,
    /// Capability bitfield. All bits reserved in v1.
    pub capabilities: u32,
}

/// `0x01` — what this node will take payment in, what purchases it accepts,
/// and how much a unit from the payer counts. Carries no price: delivery is one
/// voucher per unit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Offer {
    /// Mints whose vouchers this node accepts, most preferred first. Never
    /// empty — a node that will take no payment has nothing to offer.
    pub accepted_mints: Vec<String>,
    /// Quantity unit, matching [`Announce::unit`].
    pub unit: String,
    /// Shortest window this node accepts on a TopUp, in milliseconds. Never
    /// below [`Self::min_topup_gap_ms`], or a budget could expire before its
    /// payer is allowed to renew it.
    pub min_window_ms: u64,
    /// Longest window, in milliseconds: the longest a budget can be kept
    /// without buying again.
    pub max_window_ms: u64,
    /// What one unit from the payer draws from its budget, where one unit to
    /// it draws one: `moved = to_payer + from_payer × weight`. `1` charges both
    /// directions alike, `0` makes what the payer sends free. Unsigned, so a
    /// provider can never pay a customer for sending. Fixed for the session.
    pub from_payer_weight: u16,
    /// This node will not charge the peer, so the peer funds no channel toward
    /// it and sends it no TopUps.
    ///
    /// One-sided: it says only whether *we* charge, never whether the peer
    /// does. Key `5` on the wire, written only when `true`, so an Offer from a
    /// node that charges is byte-for-byte what it was without this field.
    pub no_charge: bool,
    /// Smallest reserved rate accepted on a TopUp, in units per second. `0`
    /// lets a payer reserve nothing and pay only for what it moves.
    pub min_reserved_rate: u64,
    /// Shortest time this node accepts between two TopUps from one payer, in
    /// milliseconds. Each TopUp costs it signature checks and a disk write.
    pub min_topup_gap_ms: u64,
}

/// `0x02` — accept the offer and fund the outgoing channel. Nothing is echoed
/// back: the payer picks a mint from the list the Offer already carried, and
/// the window and reserved rate are chosen per purchase rather than agreed
/// once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Accept {
    /// Opaque channel-funding blob, interpreted by the channel backend. Empty
    /// for free peering, where neither side charges and no channel exists.
    pub funding: Vec<u8>,
}

/// `0x03` — the funding verified and the channel is active. Sent by whichever
/// party verified the funding, which is the party that will be paid on that
/// channel, so the direction needs no field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelReady {
    /// The channel now active.
    pub channel_id: ChannelId,
}

/// One channel's ratchet turn.
///
/// Signed on its own, over `(channel_id, cumulative)` and nothing else — the
/// window and reserved rate are deliberately outside the signature: the window
/// is measured from receipt, so the two sides need no clock agreement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelUpdate {
    /// The payer's channel this update ratchets.
    pub channel_id: ChannelId,
    /// Total units authorized on that channel, ever. Strictly increasing.
    pub cumulative: u64,
    /// Schnorr signature over `(channel_id, cumulative)`.
    pub signature: Signature,
}

/// `0x04` — the balance updates and the purchase in one message, and the only
/// payment message in the protocol.
///
/// **The grant is the combined increase across every update**, and it is added
/// to the payer's budget: nothing already in the budget is lost. One message
/// carries as many channels as the payer wants to draw from, which is what lets
/// a purchase span a channel that is filling up and its replacement, and what
/// lets a payer holding vouchers from several accepted mints spend from more
/// than one at a time.
///
/// **Applied atomically.** If any update fails to increase, the whole message
/// is refused — a partial application would leave the grant size ambiguous.
///
/// Each `cumulative` is monotonic on its own channel, which makes this
/// idempotent: a lost message costs nothing because the next one carries the
/// correct totals, and a reordered one is discarded. So it needs no
/// acknowledgment, and a payer may use what it bought without waiting a round
/// trip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TopUp {
    /// The channels being ratcheted. At least one, at most
    /// [`MAX_CHANNEL_UPDATES`](crate::MAX_CHANNEL_UPDATES).
    pub updates: Vec<ChannelUpdate>,
    /// Keep the budget at least this long, measured from receipt: the deadline
    /// becomes the later of the old one and now plus this.
    pub window_ms: u64,
    /// Units per second to reserve from now on, replacing the rate reserved
    /// before. `0` reserves nothing.
    pub reserved_rate: u64,
}

/// `0x05` — the provider will not honor a purchase that verified: it came too
/// soon, asks for terms outside the Offer, would oversubscribe what the
/// provider has promised, or exceeds what is left in a channel.
///
/// Declining to ratchet already leaves the payer's money untouched; this exists
/// so the payer learns in one round trip, and acts on the reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TopUpReject {
    /// The states we are declining to ratchet to, echoed back so the payer can
    /// tell which purchase was refused. A single channel no longer identifies
    /// one, since a purchase may span several. Signatures are not echoed —
    /// the payer already holds them and this message is rare.
    pub refused: Vec<RefusedUpdate>,
    /// The highest reserved rate we would accept from this payer now, in units
    /// per second: our capacity less the other payers' reserved rates.
    pub max_reserved_rate: u64,
    /// Why it was refused.
    pub reason: ReasonCode,
}

/// One entry of a refused purchase.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RefusedUpdate {
    /// The channel.
    pub channel_id: ChannelId,
    /// The cumulative total we did not ratchet to.
    pub cumulative: u64,
}

/// `0x06` — open a new channel alongside one approaching exhaustion. Initiated
/// by the funder alone: only the party putting up new funds decides when.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RolloverInit {
    /// The channel being replaced, which keeps draining to 100%.
    pub old_channel_id: ChannelId,
    /// Funding blob for the replacement channel.
    pub funding: Vec<u8>,
}

/// `0x07` — the replacement channel is funded and ready.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RolloverReady {
    /// The channel being replaced.
    pub old_channel_id: ChannelId,
    /// The replacement, which grants continue on once the old one exhausts.
    pub new_channel_id: ChannelId,
}

/// Why a channel is being closed. A distinct set from [`ReasonCode`] — these
/// describe an orderly wind-down rather than a refusal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum CloseReason {
    /// Ordinary cooperative close.
    Normal = 0,
    /// The peer's from-payer weight was not acceptable.
    PriceRejected = 1,
    /// The peer is going away.
    PeerLeaving = 2,
}

impl CloseReason {
    /// Map a wire code onto a reason, defaulting unknown codes to
    /// [`Self::Normal`] — the close proceeds either way.
    pub fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::PriceRejected,
            2 => Self::PeerLeaving,
            _ => Self::Normal,
        }
    }
}

/// `0x08` — request a cooperative close.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelClose {
    /// The channel to close.
    pub channel_id: ChannelId,
    /// Proposed final balance.
    pub final_balance: u64,
    /// Signature over the final balance.
    pub final_signature: Signature,
    /// Why.
    pub reason: CloseReason,
}

/// `0x09` — acknowledge a close at the agreed balance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloseAck {
    /// The channel being closed.
    pub channel_id: ChannelId,
    /// The final balance both sides settle on.
    pub accepted_balance: u64,
}

/// `0x0A` — general-purpose rejection for any proposal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reject {
    /// The wire tag of the message being rejected.
    pub rejected_type: u8,
    /// Machine-readable reason.
    pub reason: ReasonCode,
    /// Optional human-readable detail.
    pub text: Option<String>,
}

/// `0x0B` — orderly teardown of the whole relationship. A bare FIN instead is
/// treated as an unclean disconnect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Disconnect {
    /// Why we are leaving.
    pub reason: ReasonCode,
}

/// `0x0C` — what is left of the payer's budget, until when, and the rate it
/// has reserved. Sent by the provider.
///
/// Information, not an instruction: the payer keeps its own count and decides
/// what to buy from that, never from this. The deadline is sent as time left,
/// so the two sides need no clock agreement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Balance {
    /// Units left in the budget: `authorized − consumed`.
    pub remaining: u64,
    /// Milliseconds until the deadline; `0` when there is no budget.
    pub expires_in_ms: u64,
    /// Units per second reserved now; `0` for none.
    pub reserved_rate: u64,
}

/// Any TollGate message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    /// See [`Announce`].
    Announce(Announce),
    /// See [`Offer`].
    Offer(Offer),
    /// See [`Accept`].
    Accept(Accept),
    /// See [`ChannelReady`].
    ChannelReady(ChannelReady),
    /// See [`TopUp`].
    TopUp(TopUp),
    /// See [`TopUpReject`].
    TopUpReject(TopUpReject),
    /// See [`RolloverInit`].
    RolloverInit(RolloverInit),
    /// See [`RolloverReady`].
    RolloverReady(RolloverReady),
    /// See [`ChannelClose`].
    ChannelClose(ChannelClose),
    /// See [`CloseAck`].
    CloseAck(CloseAck),
    /// See [`Reject`].
    Reject(Reject),
    /// See [`Disconnect`].
    Disconnect(Disconnect),
    /// See [`Balance`].
    Balance(Balance),
}

impl Message {
    /// The wire tag this message encodes under.
    pub fn msg_type(&self) -> MsgType {
        match self {
            Self::Announce(_) => MsgType::Announce,
            Self::Offer(_) => MsgType::Offer,
            Self::Accept(_) => MsgType::Accept,
            Self::ChannelReady(_) => MsgType::ChannelReady,
            Self::TopUp(_) => MsgType::TopUp,
            Self::TopUpReject(_) => MsgType::TopUpReject,
            Self::RolloverInit(_) => MsgType::RolloverInit,
            Self::RolloverReady(_) => MsgType::RolloverReady,
            Self::ChannelClose(_) => MsgType::ChannelClose,
            Self::CloseAck(_) => MsgType::CloseAck,
            Self::Reject(_) => MsgType::Reject,
            Self::Disconnect(_) => MsgType::Disconnect,
            Self::Balance(_) => MsgType::Balance,
        }
    }
}
