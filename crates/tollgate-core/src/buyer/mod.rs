//! The payer side: when to buy, at what reserved rate, and how much.
//!
//! A payer tops up whenever it wants. Because the signed state is cumulative,
//! a TopUp needs no acknowledgment — a lost one costs nothing since the next
//! carries the correct total, and a reordered one is discarded. So the buyer
//! can raise its reserved rate and start using it in the same breath; the
//! worst case is a single round trip of shaping at the old rate.
//!
//! It keeps its own count of the budget it holds with each provider, from
//! what it signed and what it measured crossing the link, and decides from
//! that. The provider's Balance is information: it never makes the buyer buy
//! more.
//!
//! What it gives up is the ability to pay only for what actually arrived. A
//! budget is drawn whether or not packets land, so transit loss is the buyer's
//! cost. That stays measurable one-sided, so the correction needs no protocol
//! at all: buy less, or buy somewhere else.
//!
//! - `state.rs` — [`Buyer`], [`BuyerPolicy`], and the [`Purchase`] it emits.
//! - `core.rs` — [`poll`], the pure buy/hold decision.

mod core;
mod state;

#[cfg(test)]
mod tests;

pub use core::{Demand, poll};
pub use state::{
    Buyer, BuyerPolicy, ChannelBuyer, FUNDING_TIMEOUT_MS, Leg, Purchase, RolloverReason, Terms,
    Trigger,
};
