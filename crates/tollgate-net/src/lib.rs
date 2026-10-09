//! A TollGate node: the first deployment of the protocol, selling network
//! access.
//!
//! Everything here is the host side of the sans-IO boundary. `tollgate-core`
//! decides what is owed and what to deliver; this crate supplies the sockets,
//! the clock, the signer, the channel backend and the thing that actually
//! moves bytes.
//!
//! - [`identity`] — the keypair, and the signature over a channel update. Core
//!   holds no keys.
//! - [`wire`] — the raw-TCP control plane, length-prefixed CBOR.
//! - [`dataplane`] — the resource itself: real bytes on a real socket, shaped
//!   to what was bought.
//! - [`enforcer`] — the delivery gate and the meters.
//! - [`channel`] — payment channels behind a trait.
//! - [`client`] — one buyer session, for a program that speaks TollGate on a
//!   device's behalf.
//! - [`mint`] — this node's own mint, with a byte-denominated keyset, served
//!   by `mintd` on a public and a private listener.
//! - [`mintd`] — `mint.yaml`, and what `mintd` publishes for `minttop`.
//! - [`market`] — selling those vouchers. A separate protocol on its own path,
//!   served by `merchantd`.
//! - [`merchant`] — `merchantd`: `merchant.yaml`, the funding socket
//!   `tollgated` uses, and the control socket `merchanttop` reads.
//! - [`pricing`] — a price per Mbit in usd, eur or sat, turned into bytes per
//!   token through BTC rates.
//! - [`speedtest`] — a byte source on the mesh, so a client can measure the
//!   path it paid for rather than the path to somebody's CDN.
//! - [`node`] — the driver that connects all of it to core.
//! - [`settle`] — settling channels, and retrying the settlements that fail.
//! - [`config`] — the YAML the operator writes.
//! - [`control`] — a local socket publishing what the node is doing, which is
//!   what `tolltop` reads.

pub mod channel;
pub mod client;
pub mod config;
pub mod control;
pub mod dataplane;
pub mod enforcer;
pub mod fips;
pub mod identity;
pub mod market;
pub mod merchant;
pub mod mint;
pub mod mintd;
pub mod node;
pub mod pricing;
pub mod settle;
pub mod speedtest;
pub mod wallet;
pub mod wire;

#[cfg(test)]
mod tempdir;

pub use identity::Identity;
pub use node::{Node, NodeConfig, PeerConfig};
