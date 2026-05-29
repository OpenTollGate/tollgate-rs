//! Wire messages and the identifiers that appear in them.

use minicbor::{Decode, Encode};

/// A peer's identity: the raw secp256k1 *compressed* public key (33 bytes).
///
/// Everything in the protocol and core works with these raw bytes. `npub` is
/// only the human-readable Bech32 rendering of the same key and lives at the
/// edges (config, logs, UI) — never on the wire.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct PublicKey([u8; 33]);

impl PublicKey {
    pub const LEN: usize = 33;

    pub const fn from_bytes(bytes: [u8; 33]) -> Self {
        Self(bytes)
    }

    pub const fn as_bytes(&self) -> &[u8; 33] {
        &self.0
    }
}

/// The 15 TollGate message types (see `docs/design/core/tollgate-protocol.md`).
/// The discriminant is the value stored at CBOR field key `0`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum MessageType {
    Announce = 0x00,
    PriceSheet = 0x01,
    Accept = 0x02,
    ChannelReady = 0x03,
    MeteringReport = 0x04,
    BalanceUpdate = 0x05,
    BalanceAck = 0x06,
    BootstrapToken = 0x07,
    BootstrapAck = 0x08,
    RolloverInit = 0x09,
    RolloverReady = 0x0A,
    ChannelClose = 0x0B,
    CloseAck = 0x0C,
    Reject = 0x0D,
    Disconnect = 0x0E,
}

impl MessageType {
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    pub const fn from_u8(value: u8) -> Option<Self> {
        Some(match value {
            0x00 => Self::Announce,
            0x01 => Self::PriceSheet,
            0x02 => Self::Accept,
            0x03 => Self::ChannelReady,
            0x04 => Self::MeteringReport,
            0x05 => Self::BalanceUpdate,
            0x06 => Self::BalanceAck,
            0x07 => Self::BootstrapToken,
            0x08 => Self::BootstrapAck,
            0x09 => Self::RolloverInit,
            0x0A => Self::RolloverReady,
            0x0B => Self::ChannelClose,
            0x0C => Self::CloseAck,
            0x0D => Self::Reject,
            0x0E => Self::Disconnect,
            _ => return None,
        })
    }
}

/// Body of a [`MessageType::MeteringReport`] (0x04): cumulative, **unsigned**
/// resource stats exchanged each interval so both sides compute the same cost.
///
/// Encoded as a CBOR map with integer keys — the convention for every TollGate
/// message. (Key `0`, the message type, is added by the framing layer.) This
/// type doubles as the worked example of the `minicbor` derive pattern the rest
/// of the messages will follow.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Encode, Decode)]
#[cbor(map)]
pub struct MeteringReport {
    /// Cumulative units delivered to the peer since the session baseline.
    #[n(1)]
    pub delivered: u64,
    /// Cumulative units received from the peer since the session baseline.
    #[n(2)]
    pub received: u64,
    /// Monotonic interval sequence number.
    #[n(3)]
    pub seq: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_type_round_trips() {
        for raw in 0x00u8..=0x0E {
            let ty = MessageType::from_u8(raw).expect("known type");
            assert_eq!(ty.as_u8(), raw);
        }
        assert!(MessageType::from_u8(0x0F).is_none());
        assert!(MessageType::from_u8(0xFF).is_none());
    }

    #[test]
    fn metering_report_cbor_round_trips() {
        let report = MeteringReport { delivered: 100, received: 40, seq: 3 };
        let bytes = minicbor::to_vec(&report).expect("encode");
        let back: MeteringReport = minicbor::decode(&bytes).expect("decode");
        assert_eq!(report, back);
    }
}
