//! `tollgate-core` — resource-agnostic TollGate logic.
//!
//! # Architecture: sans-IO
//!
//! This crate is a pure, synchronous state machine. It performs **no I/O** and
//! depends on **no async runtime**, so it builds for esp32 (`no_std` + `alloc`)
//! exactly as it does for a tokio host. That is the whole reason for the shape:
//! tokio cannot run on esp32, so anything that touched it could not be shared.
//!
//! The host drives core in a loop:
//!   1. translate real-world I/O (peer connected, bytes arrived, a meter
//!      reading, a token verified by the mint) into an [`Event`];
//!   2. call [`Session::handle`], which returns a list of [`Action`]s;
//!   3. execute those actions — send bytes, install firewall rules, call the
//!      mint — feeding any results back in as new [`Event`]s.
//!
//! Consequently the `Wallet` and resource-adapter work lives in the **host**
//! (`tollgate-net`), not here: core only emits the *intent*
//! (e.g. [`Action::VerifyBootstrapToken`]) and reacts to the result. This keeps
//! core deterministic and trivially testable — no executor, no mocks, no clock.
#![no_std]

extern crate alloc;

pub mod access;
pub mod action;
pub mod event;
pub mod metering;
pub mod peer;
pub mod pricing;
pub mod session;
pub mod time;

pub use access::AccessLevel;
pub use action::Action;
pub use event::Event;
pub use peer::PeerId;
pub use pricing::Product;
pub use session::Session;
pub use time::Millis;
