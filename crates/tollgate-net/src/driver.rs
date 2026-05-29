//! Bridges transport/adapter/wallet <-> the tollgate-core Session.
//!
//! The driver is the only place that holds both tokio handles and the pure
//! core state machine. It translates network events into core Events, calls
//! Session::handle, and dispatches the resulting Actions.

pub struct Driver;

impl Driver {
    pub fn new() -> Self {
        Self
    }
}
