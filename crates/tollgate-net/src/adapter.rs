//! The resource adapter: what actually delivers, and what counts it.
//!
//! Core decides *what* to enforce; this enforces it. For network forwarding
//! that means two numbers per peer — an access level and a shaping rate — and
//! two counters. Nothing here knows about grants, windows or vouchers.
//!
//! The shaper is a token bucket. A grant buys a rate for a window, and the rate
//! is fixed for the grant's life, so a bucket that refills at that rate and
//! caps at a short burst is the natural enforcement: capacity left unused early
//! is not banked, which is exactly what the bucket's cap expresses.

use std::collections::HashMap;
use std::sync::Mutex;

use tollgate_core::access::AccessLevel;
use tollgate_core::grant::units_in;
use tollgate_core::meter::Counters;
use tollgate_protocol::PubKey;

/// How much of an unused second the bucket will hold onto.
///
/// Some slack absorbs scheduling jitter — a writer that wakes 30 ms late should
/// still be able to send what it was owed. Too much and a buyer could idle and
/// then burst, which is the thing grants exist to prevent, so this stays well
/// under one window.
const BURST_MS: u64 = 250;

/// One peer's link, as the adapter sees it.
#[derive(Debug, Clone, Copy)]
struct Link {
    access: AccessLevel,
    /// Units per second we may deliver to this peer.
    rate: u64,
    /// Tokens available to spend right now.
    tokens: u64,
    /// Cumulative units delivered to and received from this peer, since the
    /// session started. Raw — the received multiplier is applied by core when
    /// it draws the grant down, not here.
    counters: Counters,
    /// Units per second *we* want to pull from this peer. Drives our buying.
    demand: u64,
}

impl Default for Link {
    fn default() -> Self {
        Self {
            access: AccessLevel::None,
            rate: 0,
            tokens: 0,
            counters: Counters::ZERO,
            demand: 0,
        }
    }
}

/// A resource adapter that forwards nothing but meters and shapes for real.
///
/// The bytes it governs are produced by the data plane in
/// [`crate::dataplane`], which is a real socket carrying real traffic at
/// whatever rate this permits. What is simulated is only *where the traffic
/// comes from* — a generator rather than a user — not the shaping or the
/// accounting.
#[derive(Debug, Default)]
pub struct Adapter {
    links: Mutex<HashMap<PubKey, Link>>,
}

impl Adapter {
    /// An adapter with no peers.
    pub fn new() -> Self {
        Self::default()
    }

    /// Apply an access level decided by core.
    pub fn set_access(&self, peer: PubKey, access: AccessLevel) {
        self.links
            .lock()
            .expect("not poisoned")
            .entry(peer)
            .or_default()
            .access = access;
    }

    /// Apply a shaping rate decided by core.
    ///
    /// Already includes the minimum flow allowance as its floor, so there is no
    /// second rule here about what an unpaid peer may do.
    pub fn set_shaping_rate(&self, peer: PubKey, rate: u64) {
        let mut links = self.links.lock().expect("not poisoned");
        let link = links.entry(peer).or_default();
        link.rate = rate;
        // Do not carry a fat bucket across a rate change: a peer that just
        // bought a much higher rate should not also get to spend tokens that
        // accrued while it was idle at the old one.
        link.tokens = link.tokens.min(units_in(rate, BURST_MS));
    }

    /// Set what we want to pull from this peer, in units per second.
    pub fn set_demand(&self, peer: PubKey, rate: u64) {
        self.links
            .lock()
            .expect("not poisoned")
            .entry(peer)
            .or_default()
            .demand = rate;
    }

    /// What we want to pull from this peer.
    pub fn demand(&self, peer: PubKey) -> u64 {
        self.links
            .lock()
            .expect("not poisoned")
            .get(&peer)
            .map(|l| l.demand)
            .unwrap_or(0)
    }

    /// Cumulative counters for a peer.
    pub fn counters(&self, peer: PubKey) -> Counters {
        self.links
            .lock()
            .expect("not poisoned")
            .get(&peer)
            .map(|l| l.counters)
            .unwrap_or(Counters::ZERO)
    }

    /// The rate a peer is currently shaped to.
    pub fn shaping_rate(&self, peer: PubKey) -> u64 {
        self.links
            .lock()
            .expect("not poisoned")
            .get(&peer)
            .map(|l| l.rate)
            .unwrap_or(0)
    }

    /// Every peer the adapter is tracking.
    pub fn peers(&self) -> Vec<PubKey> {
        self.links
            .lock()
            .expect("not poisoned")
            .keys()
            .copied()
            .collect()
    }

    /// Refill a peer's bucket for `elapsed_ms` and take out what may be sent now.
    ///
    /// Returns the number of units the data plane is allowed to write. A peer
    /// whose delivery is blocked gets nothing regardless of its bucket.
    pub fn take_allowance(&self, peer: PubKey, elapsed_ms: u64) -> u64 {
        let mut links = self.links.lock().expect("not poisoned");
        let link = links.entry(peer).or_default();

        if !link.access.delivery_allowed() && link.rate == 0 {
            return 0;
        }

        let cap = units_in(link.rate, BURST_MS);
        link.tokens = link
            .tokens
            .saturating_add(units_in(link.rate, elapsed_ms))
            .min(cap);

        let take = link.tokens;
        link.tokens -= take;
        take
    }

    /// Record units actually written to a peer — its download.
    pub fn record_delivered(&self, peer: PubKey, units: u64) {
        let mut links = self.links.lock().expect("not poisoned");
        let link = links.entry(peer).or_default();
        link.counters.delivered = link.counters.delivered.saturating_add(units);
    }

    /// Record units actually read from a peer — its upload.
    pub fn record_received(&self, peer: PubKey, units: u64) {
        let mut links = self.links.lock().expect("not poisoned");
        let link = links.entry(peer).or_default();
        link.counters.received = link.counters.received.saturating_add(units);
    }

    /// Forget a peer that has gone away.
    pub fn remove(&self, peer: PubKey) {
        self.links.lock().expect("not poisoned").remove(&peer);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer() -> PubKey {
        PubKey([0x02; 33])
    }

    #[test]
    fn the_bucket_refills_at_the_shaped_rate() {
        let adapter = Adapter::new();
        adapter.set_access(peer(), AccessLevel::Active);
        adapter.set_shaping_rate(peer(), 1_000_000);

        assert_eq!(
            adapter.take_allowance(peer(), 100),
            100_000,
            "100 ms at 1 M/s"
        );
        assert_eq!(adapter.take_allowance(peer(), 50), 50_000);
    }

    #[test]
    fn an_idle_peer_cannot_bank_capacity_and_then_burst() {
        // This is the token bucket standing in for "capacity left unused early
        // is not banked for later".
        let adapter = Adapter::new();
        adapter.set_access(peer(), AccessLevel::Active);
        adapter.set_shaping_rate(peer(), 1_000_000);

        let after_idling = adapter.take_allowance(peer(), 60_000);
        assert_eq!(
            after_idling,
            units_in(1_000_000, BURST_MS),
            "a minute of idling buys only the burst allowance"
        );
    }

    #[test]
    fn a_rate_rise_does_not_release_tokens_banked_at_the_old_rate() {
        let adapter = Adapter::new();
        adapter.set_access(peer(), AccessLevel::Active);
        adapter.set_shaping_rate(peer(), 1_000);
        adapter.take_allowance(peer(), 10_000);

        adapter.set_shaping_rate(peer(), 100_000_000);
        assert_eq!(
            adapter.take_allowance(peer(), 0),
            0,
            "the bucket was trimmed to the new rate's burst, and nothing has accrued yet"
        );
    }

    #[test]
    fn a_blocked_peer_with_no_allowance_gets_nothing() {
        let adapter = Adapter::new();
        adapter.set_access(peer(), AccessLevel::None);
        adapter.set_shaping_rate(peer(), 0);
        assert_eq!(adapter.take_allowance(peer(), 1_000), 0);
    }

    #[test]
    fn a_blocked_peer_still_gets_the_minimum_flow_allowance() {
        // The allowance is what breaks the bootstrap circle: a peer holding no
        // vouchers has to be able to reach a mint to acquire some.
        let adapter = Adapter::new();
        adapter.set_access(peer(), AccessLevel::None);
        adapter.set_shaping_rate(peer(), 4_096);
        assert_eq!(
            adapter.take_allowance(peer(), 1_000),
            1_024,
            "capped by the burst"
        );
    }

    #[test]
    fn counters_accumulate_in_both_directions() {
        let adapter = Adapter::new();
        adapter.record_delivered(peer(), 1_000);
        adapter.record_delivered(peer(), 500);
        adapter.record_received(peer(), 200);

        assert_eq!(
            adapter.counters(peer()),
            Counters {
                delivered: 1_500,
                received: 200
            }
        );
    }
}
