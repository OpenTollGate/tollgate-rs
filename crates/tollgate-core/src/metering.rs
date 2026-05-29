//! Metering counters and transit-loss reconciliation.

/// Cumulative metering counters since the session baseline. Monotonic — they
/// only ever grow within a session.
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub struct Counters {
    /// Units delivered to the peer.
    pub delivered: u64,
    /// Units received from the peer.
    pub received: u64,
}

/// Reconcile a counter when the two sides disagree (transit loss makes the
/// sender's and receiver's tallies differ): bill the **higher** of the two.
///
/// This is honest-provider-optimistic — a dishonest provider could inflate the
/// count. Stronger guarantees (proof-of-delivery, reputation) are future work;
/// see `docs/design/core/tollgate-metering.md`.
pub const fn reconcile(local: u64, remote: u64) -> u64 {
    if local > remote { local } else { remote }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reconcile_takes_the_higher_value() {
        assert_eq!(reconcile(10, 7), 10);
        assert_eq!(reconcile(3, 9), 9);
        assert_eq!(reconcile(5, 5), 5);
    }
}
