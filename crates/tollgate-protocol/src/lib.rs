//! TollGate wire protocol: CBOR message types, codec, and TCP framing.
//!
//! Specified in `docs/design/core/tollgate-protocol.md`. Every message is a
//! CBOR map whose key `0` carries the message type tag; the remaining keys are
//! small integers, not strings, so the encoding stays compact on a constrained
//! device.
//!
//! - `types.rs` — the fixed-width identifiers the wire carries (pubkey,
//!   channel id, signature) and the reason-code enum.
//! - `message.rs` — one struct per message type plus the [`Message`] union.
//! - `codec.rs` — hand-written encode/decode against `minicbor`'s low-level
//!   `Encoder`/`Decoder`. Hand-written rather than derived because the type tag
//!   lives *inside* the same map as the payload fields, which the derive macro
//!   has no way to express.
//! - `frame.rs` — the 2-byte little-endian length prefix used by the raw-TCP
//!   transport, and an incremental reader for it.
//!
//! The crate is `no_std` + `alloc`: it runs unchanged on an ESP32.

#![no_std]

extern crate alloc;

mod codec;
mod frame;
mod message;
mod types;

pub use codec::{Error, decode, encode};
pub use frame::{FrameReader, MAX_FRAME_LEN, encode_frame};
pub use message::{
    Accept, Announce, ChannelClose, ChannelReady, CloseAck, CloseReason, Disconnect, Message, Offer,
    Reject, RolloverInit, RolloverReady, TopUp, TopUpReject,
};
pub use types::{ChannelId, MsgType, PubKey, ReasonCode, Signature};

/// Protocol version carried in [`Announce`]. Both peers must match.
pub const PROTOCOL_VERSION: u8 = 1;

/// Default TCP port for the raw-TCP transport.
pub const DEFAULT_PORT: u16 = 4747;
