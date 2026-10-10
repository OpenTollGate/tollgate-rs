//! Fixed-width wire identifiers and the reason-code enum.
//!
//! These are newtypes over byte arrays rather than aliases so that a channel id
//! can never be passed where a signature is expected, and so the decoder has
//! one place to enforce each field's length.

use core::fmt;

/// Compressed secp256k1 public key — a peer's identity on the wire.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PubKey(pub [u8; 33]);

/// Spilman channel identifier.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ChannelId(pub [u8; 32]);

/// Schnorr signature over a channel state update.
#[derive(Clone, Copy)]
pub struct Signature(pub [u8; 64]);

impl PubKey {
    /// Bytes of the compressed key.
    pub fn as_bytes(&self) -> &[u8; 33] {
        &self.0
    }
}

impl ChannelId {
    /// Bytes of the channel id.
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl Signature {
    /// Bytes of the signature.
    pub fn as_bytes(&self) -> &[u8; 64] {
        &self.0
    }
}

impl PartialEq for Signature {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}

impl Eq for Signature {}

/// Render as hex, truncated to the first four bytes — enough to correlate log
/// lines without making them unreadable.
fn fmt_short(bytes: &[u8], f: &mut fmt::Formatter<'_>) -> fmt::Result {
    for b in bytes.iter().take(4) {
        write!(f, "{b:02x}")?;
    }
    write!(f, "..")
}

impl fmt::Debug for PubKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt_short(&self.0, f)
    }
}

impl fmt::Debug for ChannelId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt_short(&self.0, f)
    }
}

impl fmt::Debug for Signature {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt_short(&self.0, f)
    }
}

impl fmt::Display for PubKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt_short(&self.0, f)
    }
}

/// Message type tags, contiguous `0x00..=0x0C` as specified.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum MsgType {
    /// "I am a TollGate node" — protocol version, pubkey.
    Announce = 0x00,
    /// Accepted mints, unit, and the terms a payer may buy on.
    Offer = 0x01,
    /// Accept the offer and fund the outgoing channel.
    Accept = 0x02,
    /// Funding verified; the channel is active.
    ChannelReady = 0x03,
    /// Signed channel update adding to the payer's budget.
    TopUp = 0x04,
    /// Refuse a purchase, with its reason and the reserved rate still free.
    TopUpReject = 0x05,
    /// Open a new channel alongside an exhausting one.
    RolloverInit = 0x06,
    /// The new channel is funded and ready.
    RolloverReady = 0x07,
    /// Request a cooperative close.
    ChannelClose = 0x08,
    /// Acknowledge a close.
    CloseAck = 0x09,
    /// Reject a proposal, with a reason.
    Reject = 0x0A,
    /// Orderly teardown.
    Disconnect = 0x0B,
    /// What is left of the payer's budget.
    Balance = 0x0C,
}

impl MsgType {
    /// Map a wire tag onto a type, or `None` if it is not one we know.
    pub fn from_u8(v: u8) -> Option<Self> {
        Some(match v {
            0x00 => Self::Announce,
            0x01 => Self::Offer,
            0x02 => Self::Accept,
            0x03 => Self::ChannelReady,
            0x04 => Self::TopUp,
            0x05 => Self::TopUpReject,
            0x06 => Self::RolloverInit,
            0x07 => Self::RolloverReady,
            0x08 => Self::ChannelClose,
            0x09 => Self::CloseAck,
            0x0A => Self::Reject,
            0x0B => Self::Disconnect,
            0x0C => Self::Balance,
            _ => return None,
        })
    }
}

/// Machine-readable rejection reasons, shared by [`super::Reject`] and
/// [`super::Disconnect`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ReasonCode {
    /// The from-payer weight is above what the payer will buy at.
    FromPayerWeightUnacceptable = 0x01,
    /// The funding mint is not in our accepted set.
    MintNotAccepted = 0x02,
    /// The unit is not one we sell.
    UnitNotAccepted = 0x03,
    /// The window or reserved rate falls outside the Offer's range.
    OutOfRange = 0x04,
    /// The channel funding did not verify.
    FundingInvalid = 0x05,
    /// The grant signature is invalid, or `cumulative` did not increase.
    GrantInvalid = 0x06,
    /// The reserved rate exceeds the capacity we have left to promise.
    RateExceedsCapacity = 0x07,
    /// The grant exceeds what remains in the channel.
    GrantExceedsChannel = 0x08,
    /// The peer's protocol version is not supported.
    VersionUnsupported = 0x09,
    /// A TopUp came sooner after the last than the Offer's gap allows.
    TooSoon = 0x0A,
    /// Anything else; see the accompanying text.
    Other = 0xFF,
}

impl ReasonCode {
    /// Whether a well-behaved peer could have avoided this by reading the Offer
    /// it was sent.
    ///
    /// Almost every refusal means the peer ignored something we advertised, or
    /// that a revised Offer crossed its message in flight — either way it is
    /// worth an operator's attention.
    ///
    /// Two are exceptions. [`Self::RateExceedsCapacity`] is not a borderline
    /// one: **the Offer carries no rate ceiling**, deliberately. What a node
    /// can promise one payer depends on what it has already promised every
    /// other, and changes continuously, so there is no static number it could
    /// honestly advertise. A payer discovers the limit by being refused and
    /// told what would be taken instead, which is the mechanism working rather
    /// than failing. [`Self::TooSoon`] is the other: a payer that keeps to the
    /// gap can still have two TopUps arrive closer together than it sent them,
    /// and it simply sends again. Logging either as a fault would train an
    /// operator to ignore the ones that are.
    pub fn avoidable_from_offer(self) -> bool {
        !matches!(self, Self::RateExceedsCapacity | Self::TooSoon)
    }

    /// Map a wire code onto a reason. Unknown codes decode as [`Self::Other`]
    /// rather than failing — a peer running a later version may have reasons we
    /// have never heard of, and the message is still actionable without them.
    pub fn from_u8(v: u8) -> Self {
        match v {
            0x01 => Self::FromPayerWeightUnacceptable,
            0x02 => Self::MintNotAccepted,
            0x03 => Self::UnitNotAccepted,
            0x04 => Self::OutOfRange,
            0x05 => Self::FundingInvalid,
            0x06 => Self::GrantInvalid,
            0x07 => Self::RateExceedsCapacity,
            0x08 => Self::GrantExceedsChannel,
            0x09 => Self::VersionUnsupported,
            0x0A => Self::TooSoon,
            _ => Self::Other,
        }
    }
}
