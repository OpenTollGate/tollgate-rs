//! Sans-IO TollGate protocol logic.
//!
//! This crate is **pure**: it never does I/O, never reads the clock, never
//! verifies a signature, and depends on no async runtime. The host
//! (`tollgate-net`) turns real events into [`Event`] values, feeds them in, and
//! executes the [`Action`]s that come back. Every decision that needs the time
//! takes it as a `now_ms` argument.
//!
//! That boundary is what lets the same logic run on a Linux router and on an
//! ESP32: the crate is `no_std` + `alloc`, and the parts that would need a
//! runtime live entirely on the host's side of the seam.
//!
//! Module layout follows the same split throughout — `core.rs` holds the pure
//! decisions, `state.rs` the data they read, `limits.rs` the arithmetic:
//!
//! - [`grant`] — each payer's budget, deadline and reserved rate, and the
//!   one rule that draws it. The provider side of payment.
//! - [`buyer`] — when to buy, at what reserved rate, and how much. The payer
//!   side: it adds back what has drained and follows demand.
//! - [`session`] — the per-peer message lifecycle that ties the two together.
//! - [`meter`] — cumulative counters to and from the payer, link-local.
//! - [`access`] — the delivery gate.
//!
//! # What the host must do before calling in
//!
//! Core trusts what it is handed. Specifically, the host must have
//! **verified any signature** on a message before wrapping it in
//! [`Event::MessageReceived`], exactly as FIPS terminates Noise before the
//! protocol layer sees a peer. Core decides *what* is owed and *when*; it never
//! decides whether a peer is who it claims to be.
//!
//! Two more things are the host's, because they touch a signature check or a
//! disk:
//!
//! - **The gap between purchases**, before any signature on a TopUp is
//!   checked: [`Sessions::too_soon`](session::Sessions::too_soon).
//! - **Each payer's budget, on disk**: written when core asks with
//!   [`Action::SaveBudget`], and handed back in
//!   [`Event::PeerConnected`] when the payer returns.

#![no_std]

extern crate alloc;

pub mod access;
pub mod buyer;
pub mod config;
pub mod grant;
pub mod meter;
pub mod session;

mod action;
mod event;
mod time;

pub use access::AccessLevel;
pub use action::Action;
pub use config::{BurstPolicy, GrantPolicy, NodePolicy, PeerPolicy};
pub use event::Event;
pub use time::Millis;

// Re-exported so a host can name the wire types without also depending on
// `tollgate-protocol` directly.
pub use tollgate_protocol::{ChannelId, Message, PubKey, ReasonCode, Signature};
