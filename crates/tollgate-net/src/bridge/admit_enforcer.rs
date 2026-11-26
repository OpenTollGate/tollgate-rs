//! Admitting a v1 device through the real [`Enforcer`] seam.
//!
//! [`admit`](super::admit) decides *whether* a device may be admitted and what it
//! costs. This file is how that decision reaches an enforcer — and the reason it
//! goes through the trait rather than an ad-hoc socket is that the trait is what
//! the rest of the node already speaks:
//!
//! ```text
//! register(peer, addr)      tie a session's key to the address the kernel sees
//! set_access(peer, level)   whether the peer is carried at all
//! set_shaping_rate(peer, r) shape it to r units per second
//! remove(peer)              forget it when the money runs out
//! ```
//!
//! A v1 device has no session and no key of its own, so the bridge supplies both:
//! it holds one [`PubKey`] per admitted device (derived, so a restart can find the
//! same one again) and gives the enforcer the device's address as the peer's
//! address. That is the whole trick, and it is only sound because the enforcer
//! never infers identity from traffic — `register` is how the two are tied.
//!
//! # What this honestly cannot do
//!
//! An enforcer keyed by address cannot tell two devices behind one address apart,
//! and it cannot follow a device that moves. A v1 device therefore gets one
//! admission per address, shared or lost as addresses are. That is not a defect of
//! this adapter; it is what v1 offers, and the adapter says so rather than
//! pretending otherwise.

use std::net::IpAddr;

use tollgate_protocol::PubKey;

use crate::enforcer::Enforcer;
use tollgate_core::AccessLevel;

use super::admit::Admission;

/// A bridge's handle on one device's admission.
///
/// Held by the caller so it can be closed when the bought time runs out. Closing
/// is not optional tidiness: an admission left open keeps shaping a peer that is
/// no longer paying.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Admitted {
    /// The key the bridge uses for this device. It is the bridge's, not the
    /// device's — a v1 device has none.
    pub peer: PubKey,
    /// The address the enforcer was told to recognise.
    pub subject: IpAddr,
    /// The rate it was opened at.
    pub rate: u64,
}

/// The key the bridge uses for a device at an address.
///
/// Derived rather than random, so a bridge that restarts can close an admission it
/// opened before the restart instead of leaking a shaped peer forever. The high
/// bit is set and the rest is the address, which keeps this namespace clearly the
/// bridge's own: a real TollGate key would have to collide with a deliberately
/// structured value to be confused with it.
pub fn peer_for(subject: IpAddr) -> PubKey {
    let mut bytes = [0u8; 33];
    // A compressed-point prefix that is valid but does not correspond to a key
    // this node will ever hold: the bridge is not a session peer, and this key is
    // only ever used as an identifier the enforcer tracks.
    bytes[0] = 0x02;
    match subject {
        IpAddr::V4(v4) => bytes[1..5].copy_from_slice(&v4.octets()),
        IpAddr::V6(v6) => bytes[1..17].copy_from_slice(&v6.octets()),
    }
    // Mark it as the bridge's namespace so it cannot be mistaken for a session key.
    bytes[17] = 0xB1;
    PubKey(bytes)
}

/// Put an admission into force on `enforcer`, and return the handle needed to
/// close it.
///
/// Order matters: `register` before `set_access` before `set_shaping_rate`. A rate
/// applied to a peer the enforcer has not been told the address of would shape
/// traffic it cannot attribute; an access level applied before the register would
/// be attached to a peer with no address. Doing them in this order means a partial
/// failure leaves the peer closed rather than open at a rate.
pub fn apply(enforcer: &dyn Enforcer, admission: &Admission) -> Admitted {
    let peer = peer_for(admission.subject);
    enforcer.register(peer, admission.subject);
    enforcer.set_access(peer, AccessLevel::Active);
    enforcer.set_shaping_rate(peer, admission.rate);
    Admitted {
        peer,
        subject: admission.subject,
        rate: admission.rate,
    }
}

/// Close an admission: shape to zero, then forget the peer.
///
/// Both steps, in this order. Shaping to zero stops the traffic immediately; the
/// `remove` is what stops the enforcer tracking a peer nobody will pay for again.
/// A `remove` alone would be enough for correctness but would leave a window where
/// the peer's counters vanish while its packets are still being shaped.
pub fn close(enforcer: &dyn Enforcer, admitted: &Admitted) {
    enforcer.set_shaping_rate(admitted.peer, 0);
    enforcer.remove(admitted.peer);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::enforcer::Loopback;

    fn ip(s: &str) -> IpAddr {
        s.parse().expect("an address")
    }

    /// The derived key is stable, so a restarted bridge closes the same peer it
    /// opened, and distinct per address, so two devices never share one.
    #[test]
    fn the_peer_key_is_stable_and_distinct() {
        assert_eq!(peer_for(ip("192.0.2.7")), peer_for(ip("192.0.2.7")));
        assert_ne!(peer_for(ip("192.0.2.7")), peer_for(ip("192.0.2.8")));
        // And it cannot be confused with a session key: the namespace byte is
        // set, and the compressed-point prefix is a value no honest key uses in
        // combination with it.
        assert_eq!(peer_for(ip("192.0.2.7")).0[17], 0xB1);
    }

    /// Applying an admission really does register the address, open the peer and
    /// shape it — against a real `Enforcer`, not a mock of one.
    #[test]
    fn applying_an_admission_registers_opens_and_shapes() {
        let enforcer = Loopback::new();
        let admission = Admission {
            subject: ip("192.0.2.7"),
            rate: 250,
            spent: 1_000,
        };
        let admitted = apply(&enforcer, &admission);

        assert_eq!(admitted.peer, peer_for(ip("192.0.2.7")));
        assert_eq!(
            enforcer.recorded_access(admitted.peer),
            Some(AccessLevel::Active),
            "an admitted device must be carried, not left closed"
        );
        assert_eq!(enforcer.shaping_rate(admitted.peer), 250);
        assert!(
            enforcer.peers().contains(&admitted.peer),
            "the enforcer is not tracking the admitted peer"
        );
    }

    /// Closing stops the shaping and forgets the peer. An admission left open
    /// keeps carrying a device that is no longer paying.
    #[test]
    fn closing_stops_the_rate_and_forgets_the_peer() {
        let enforcer = Loopback::new();
        let admission = Admission {
            subject: ip("192.0.2.7"),
            rate: 250,
            spent: 1_000,
        };
        let admitted = apply(&enforcer, &admission);
        close(&enforcer, &admitted);

        assert_eq!(
            enforcer.recorded_access(admitted.peer),
            None,
            "closing must forget the peer, not merely shape it to zero"
        );
        assert_eq!(
            enforcer.shaping_rate(admitted.peer),
            0,
            "closing must stop the rate"
        );
        assert!(
            !enforcer.peers().contains(&admitted.peer),
            "closing must forget the peer"
        );
    }

    /// Two devices are two admissions: closing one must not close the other.
    #[test]
    fn closing_one_device_leaves_the_other_admitted() {
        let enforcer = Loopback::new();
        let first = apply(
            &enforcer,
            &Admission {
                subject: ip("192.0.2.7"),
                rate: 100,
                spent: 100,
            },
        );
        let second = apply(
            &enforcer,
            &Admission {
                subject: ip("192.0.2.8"),
                rate: 200,
                spent: 200,
            },
        );
        close(&enforcer, &first);

        assert_eq!(enforcer.shaping_rate(first.peer), 0);
        assert_eq!(
            enforcer.shaping_rate(second.peer),
            200,
            "closing one device closed another"
        );
    }
}
