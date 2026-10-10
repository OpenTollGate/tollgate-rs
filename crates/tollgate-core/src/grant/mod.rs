//! The provider side of payment: what a payer has bought, and what has been
//! drawn from it.
//!
//! A payment is a **grant**: a number of units, sent with a window and a
//! reserved rate, both chosen by the payer. The grant is **added** to the
//! payer's budget. The window moves the deadline to the later of the old one
//! and now plus the window, so a grant never brings it closer. The reserved
//! rate replaces the one before it.
//!
//! Every second the provider draws `max(units moved, reserved rate × 1 s)` from
//! the budget — the one rule. A reserved rate sells time at a speed, since
//! idle seconds drain too; no reservation sells pay per use. Whatever is left
//! at the deadline expires, and the provider keeps the payment.
//!
//! - `limits.rs` — rate and budget arithmetic, saturating.
//! - `state.rs` — [`GrantState`], the numbers and their transitions, and the
//!   [`Budget`] a payer keeps between sessions.
//! - `core.rs` — [`evaluate_topup`], the pure admission decision.

mod core;
mod limits;
mod state;

#[cfg(test)]
mod tests;

pub use core::{Admission, Verdict, evaluate_topup};
pub use limits::{SECOND_MS, Second, budget_for, per_tick_ceiling, rate_from, units_in};
pub use state::{Budget, GrantState, IncomingChannel, MAX_VERIFICATION_FAILURES};
