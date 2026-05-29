//! TollGate wire protocol — CBOR message types and canonical encoding.
//!
//! Resource-agnostic and `no_std` + `alloc`, so the same types compile for a
//! tokio host and for esp32. The wire format (see
//! `docs/design/core/tollgate-protocol.md`) is CBOR maps with **integer field
//! keys**; field key `0` always carries the [`MessageType`].
#![no_std]

extern crate alloc;

mod message;
mod product;

pub use message::{MessageType, MeteringReport, PublicKey};
pub use product::{DEFAULT_PRICING_SCALE, MintPrice, ProductId, product_id};
