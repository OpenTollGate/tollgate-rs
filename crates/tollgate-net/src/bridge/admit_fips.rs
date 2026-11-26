//! Admitting a v1 device when the node enforces over FIPS.
//!
//! [`admit_enforcer`](super::admit_enforcer) admits a v1 device by telling the
//! enforcer to recognise its **address**. That is correct for the loopback and
//! `ip` enforcers and it is **wrong for FIPS**, and the FIPS enforcer says why in
//! as many words:
//!
//! > `Ip` binds a peer's public key to whatever address it announced itself from,
//! > on the peer's own say-so. On an unwrapped IP network that is a real hole:
//! > claim a paying peer's key and its budget gates your address.
//! >
//! > FIPS names peers by npub and has already authenticated the link with a Noise
//! > IK handshake before this node hears about it… There is no address to spoof and
//! > no binding step to subvert — which is why the `register` address argument is
//! > ignored rather than merely unused.
//!
//! So the whole premise of address admission — *"tell the enforcer to trust this
//! address"* — is the vulnerability FIPS is designed not to have. This module
//! therefore does **not** reuse [`binding_for`](super::binding_for). It admits by
//! key and nothing else.
//!
//! # What follows for a v1 device
//!
//! A v1 device has no key. Under FIPS that is fatal rather than inconvenient: there
//! is no address to register and no subject to bind, because the node cannot invent
//! an identity and have FIPS honour it. A device reaches the mesh by completing a
//! Noise handshake, and a device that has done that already has a key — so a device
//! without one is not on the mesh to be admitted.
//!
//! The honest consequence: **this bridge cannot put a keyless v1 device on a FIPS
//! mesh.** What it can do is the other thing that is actually useful — admit a peer
//! whose key it was given (by the mesh, not by the device) and charge a v1 buyer's
//! credit for it. That is the shape below, and the caller must supply the key from a
//! source that authenticated it. This module never derives one.

use std::net::IpAddr;

use tollgate_protocol::PubKey;

use crate::bridge::Credit;
use crate::enforcer::Enforcer;
use tollgate_core::AccessLevel;

/// Why a peer could not be admitted over FIPS.
#[derive(Debug, PartialEq, Eq)]
pub enum FipsRefused {
    /// No credit at the rate and duration asked for.
    Unpaid {
        /// What the ledger holds for that mint and unit.
        available: u64,
        /// What the request needed.
        needed: u64,
    },
    /// The key is all zero, which is not a peer.
    NoKey,
    /// A rate of zero would admit nothing while charging for it.
    NothingAsked,
}

impl FipsRefused {
    /// The refusal in words.
    pub fn message(&self) -> String {
        match self {
            FipsRefused::Unpaid { available, needed } => {
                format!("this peer has {available} left and admittance at that rate needs {needed}")
            }
            FipsRefused::NoKey => "a zero key is not a peer".into(),
            FipsRefused::NothingAsked => "a rate of zero admits nothing".into(),
        }
    }
}

/// Admit an already-authenticated peer, paid for out of a v1 buyer's credit.
///
/// `peer` must come from a source that authenticated it — the mesh. This function
/// cannot tell whether it did, so the requirement is stated here and left to the
/// caller rather than silently assumed: deriving a key from an address would
/// reintroduce exactly the spoofable binding FIPS removes.
///
/// Billing is `rate × seconds`, the same as the address path, so the two agree on
/// what access costs even though they disagree on what identifies a peer.
pub fn admit_peer(
    credit: &Credit,
    enforcer: &dyn Enforcer,
    mint: &str,
    unit: &str,
    peer: PubKey,
    rate: u64,
    seconds: u64,
) -> Result<u64, FipsRefused> {
    if peer.0 == [0u8; 33] {
        return Err(FipsRefused::NoKey);
    }
    if rate == 0 {
        return Err(FipsRefused::NothingAsked);
    }
    let needed = rate.saturating_mul(seconds);
    let available = credit.available(mint, unit);
    if available < needed {
        return Err(FipsRefused::Unpaid { available, needed });
    }
    let spent = credit.take(mint, unit, needed);
    if spent < needed {
        credit.refund(mint, unit, spent);
        return Err(FipsRefused::Unpaid {
            available: spent,
            needed,
        });
    }

    // The address argument is ignored by the FIPS enforcer; passing the derived
    // mesh address rather than a claim keeps this honest under the other kinds too.
    enforcer.register(peer, IpAddr::V6(crate::fips::address(peer)));
    enforcer.set_access(peer, AccessLevel::Active);
    enforcer.set_shaping_rate(peer, rate);
    Ok(spent)
}

/// Close an admission opened by [`admit_peer`].
///
/// The same two steps as the address path and for the same reason: shaping to zero
/// stops the traffic now, and `remove` stops the enforcer tracking a peer nobody
/// will pay for again.
pub fn close_peer(enforcer: &dyn Enforcer, peer: PubKey) {
    enforcer.set_shaping_rate(peer, 0);
    enforcer.remove(peer);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::enforcer::Loopback;

    fn a_key() -> PubKey {
        let mut bytes = [0u8; 33];
        bytes[0] = 0x02;
        bytes[1] = 0x11;
        PubKey(bytes)
    }

    fn ip_mint_and_unit() -> (&'static str, &'static str) {
        ("https://m", "byte")
    }

    /// A real peer, paid for, is admitted and shaped — and the credit is spent.
    #[test]
    fn a_paid_peer_is_admitted_and_shaped() {
        let (mint, unit) = ip_mint_and_unit();
        let credit = Credit::new();
        credit.deposit(mint, unit, 1_000).expect("a buyer paid");
        let enforcer = Loopback::new();

        let spent = admit_peer(&credit, &enforcer, mint, unit, a_key(), 100, 4).expect("admitted");

        assert_eq!(spent, 400);
        assert_eq!(credit.available(mint, unit), 600);
        assert_eq!(enforcer.shaping_rate(a_key()), 100);
        assert_eq!(enforcer.recorded_access(a_key()), Some(AccessLevel::Active));
    }

    /// A peer with no credit behind it is refused and costs nothing.
    #[test]
    fn an_unpaid_peer_is_refused_and_charged_nothing() {
        let (mint, unit) = ip_mint_and_unit();
        let credit = Credit::new();
        credit
            .deposit(mint, unit, 50)
            .expect("a buyer paid a little");
        let enforcer = Loopback::new();

        let refused = admit_peer(&credit, &enforcer, mint, unit, a_key(), 100, 4)
            .expect_err("400 is more than 50");
        assert_eq!(
            refused,
            FipsRefused::Unpaid {
                available: 50,
                needed: 400
            }
        );
        assert_eq!(credit.available(mint, unit), 50);
        assert_eq!(enforcer.shaping_rate(a_key()), 0, "refused, yet shaped");
    }

    /// A zero key is refused rather than registered: FIPS would have no peer to
    /// name, and the node would be shaping nothing while charging for it.
    #[test]
    fn a_zero_key_is_refused() {
        let (mint, unit) = ip_mint_and_unit();
        let credit = Credit::new();
        credit.deposit(mint, unit, 1_000).expect("a buyer paid");
        let enforcer = Loopback::new();
        assert_eq!(
            admit_peer(&credit, &enforcer, mint, unit, PubKey([0u8; 33]), 10, 1),
            Err(FipsRefused::NoKey)
        );
        assert_eq!(credit.available(mint, unit), 1_000);
    }

    /// The key is what is admitted, so the address the enforcer is told about is
    /// derived from the key rather than supplied. This is the property the FIPS
    /// enforcer relies on: a peer cannot be admitted under an address it chose.
    #[test]
    fn the_address_comes_from_the_key_not_from_the_caller() {
        let a = crate::fips::address(a_key());
        let b = crate::fips::address(a_key());
        assert_eq!(a, b, "the same key must always name the same address");
        assert_ne!(a, crate::fips::address(PubKey([0x02; 33])));
    }

    /// Closing stops the rate and forgets the peer.
    #[test]
    fn closing_stops_the_rate_and_forgets_the_peer() {
        let (mint, unit) = ip_mint_and_unit();
        let credit = Credit::new();
        credit.deposit(mint, unit, 1_000).expect("a buyer paid");
        let enforcer = Loopback::new();
        admit_peer(&credit, &enforcer, mint, unit, a_key(), 100, 1).expect("admitted");

        close_peer(&enforcer, a_key());
        assert_eq!(enforcer.shaping_rate(a_key()), 0);
        assert_eq!(enforcer.recorded_access(a_key()), None);
    }
}
