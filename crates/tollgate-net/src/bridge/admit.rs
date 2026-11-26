//! Admitting a v1 device to a v2 node: the inbound half of the bridge.
//!
//! [`BridgeFunding`](super::BridgeFunding) covers the outbound case — this node
//! paying a v2 upstream with v1 revenue. This module covers the other direction:
//! a device that only speaks the v1 request/response exchange wants service from
//! a node that only speaks v2.
//!
//! # Why it can work at all
//!
//! The v2 enforcer never learns a device's key from traffic. `tollgate-protocol`'s
//! enforcer module says so explicitly: *"`peer` is always the payer: the TollGate
//! key a session runs under. What the enforcer recognizes in traffic is a
//! [`Subject`]: plain bytes, whose form the instance's identity fixes. Every
//! subject a payer holds arrives in a `Bind`; nothing is ever inferred from the
//! payer's key."*
//!
//! So a device need not hold a v2 key to be admitted. It needs a **subject** the
//! enforcer recognises — and an IPv4/IPv6 address is one. The bridge therefore:
//!
//! 1. takes a v1 buyer's payment at its own v1 endpoint;
//! 2. picks a **payer key** it controls for that device (the v2 session's identity
//!    is the bridge's, not the phone's — the phone has no v2 key and never will);
//! 3. opens that payer at a rate with [`Set`], and binds the device's address to it
//!    with [`Bind`];
//! 4. closes it with `Set { rate: Some(0) }` when the money runs out.
//!
//! # What this is not
//!
//! It is **not** a v2 session: no channel is funded, no `TopUp` is signed, no
//! Spilman ratchet runs. The bridge tells the v2 node to admit and shape one
//! address, and pays for that from a v1 buyer's deposit by the ordinary
//! [`BridgeFunding`] path. That keeps one money path instead of two.
//!
//! It is also **not** identity**: two devices behind one address share an
//! admission, and a device that changes address loses it. v1 has no better
//! identity to offer — its clients are identified by what the network sees, which
//! is exactly the MAC model this whole effort exists to escape. A v1 device gets
//! the identity v1 has: its address.

use std::net::IpAddr;

use tollgate_protocol::enforcer::{Binding, Subject};

use super::Credit;

/// One v1 device admitted through a v2 node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Admission {
    /// The address the enforcer was told to recognise.
    pub subject: IpAddr,
    /// The rate it was opened at, in units per second.
    pub rate: u64,
    /// What the bridge had to spend of a v1 buyer's credit.
    pub spent: u64,
}

/// Why a v1 device could not be admitted.
#[derive(Debug, PartialEq, Eq)]
pub enum AdmissionRefused {
    /// No credit covers this rate for this device. A v1 buyer has not paid, or
    /// has already spent what they paid.
    Unpaid {
        /// What the ledger holds.
        available: u64,
        /// What the requested rate would cost.
        needed: u64,
    },
    /// The address is not one an enforcer can key on.
    UnusableSubject(&'static str),
    /// A rate of zero would admit nothing while pretending to.
    NothingAsked,
}

impl AdmissionRefused {
    /// The refusal in words, for a log or a buyer.
    pub fn message(&self) -> String {
        match self {
            AdmissionRefused::Unpaid { available, needed } => format!(
                "this device has {available} left and admittance at that rate needs {needed}"
            ),
            AdmissionRefused::UnusableSubject(why) => format!("cannot admit {why}"),
            AdmissionRefused::NothingAsked => "a rate of zero admits nothing".into(),
        }
    }
}

/// Turn a v1 buyer's address and a rate into the `Bind` a v2 enforcer needs.
///
/// The `Binding` carries `delegated: false` deliberately. That flag means "a
/// local trusted client's word rather than what `tollgated` itself saw", and the
/// protocol calls it "the only trust distinction on the wire". The bridge *is*
/// asserting this on the buyer's behalf rather than from the node's own traffic
/// observation, so this is the one place where the honest value is arguable — and
/// the safe direction is to leave it false and let the enforcer apply its own
/// checks, rather than claim a delegation this code cannot justify.
pub fn binding_for(device: IpAddr) -> Result<Binding, AdmissionRefused> {
    if device.is_unspecified() {
        return Err(AdmissionRefused::UnusableSubject("an unspecified address"));
    }
    if device.is_multicast() {
        return Err(AdmissionRefused::UnusableSubject("a multicast address"));
    }
    // `Subject::address` is infallible and always sixteen bytes: an IPv4 address
    // is carried IPv4-mapped, so each address has exactly one encoding and a
    // binding cannot be ambiguous about which form it meant.
    Ok(Binding {
        subject: Subject::address(device),
        delegated: false,
    })
}

/// Charge a v1 buyer's credit for admitting `device` at `rate`, and return what
/// the enforcer must be told.
///
/// Billing is per second of admittance, so a caller passes how many seconds it is
/// buying and this spends `rate × seconds`. That keeps the two decisions — how
/// fast, and for how long — separate, which is what makes expiry expressible:
/// closing is `Set { rate: Some(0) }`, and the caller does it when the bought
/// seconds run out.
pub fn admit(
    credit: &Credit,
    mint: &str,
    unit: &str,
    device: IpAddr,
    rate: u64,
    seconds: u64,
) -> Result<Admission, AdmissionRefused> {
    if rate == 0 {
        return Err(AdmissionRefused::NothingAsked);
    }
    let binding = binding_for(device)?;
    debug_assert!(!binding.subject.as_bytes().is_empty());

    let needed = rate.saturating_mul(seconds);
    let available = credit.available(mint, unit);
    if available < needed {
        return Err(AdmissionRefused::Unpaid { available, needed });
    }
    let spent = credit.take(mint, unit, needed);
    if spent < needed {
        // take() releases what it can, so this is the same refusal as above,
        // restated for the case where the balance moved between the two reads.
        credit.refund(mint, unit, spent);
        return Err(AdmissionRefused::Unpaid {
            available: spent,
            needed,
        });
    }
    Ok(Admission {
        subject: device,
        rate,
        spent,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().expect("an address")
    }

    /// An address is a subject the enforcer can key on, and the binding says it
    /// was not delegated — because the bridge cannot claim the node saw it.
    #[test]
    fn an_address_becomes_a_binding_that_is_not_delegated() {
        let binding = binding_for(ip("192.0.2.7")).expect("a usable address");
        assert!(
            !binding.delegated,
            "the bridge cannot claim the node saw this"
        );
        assert_eq!(
            binding.subject.as_bytes().len(),
            16,
            "an address subject is sixteen bytes"
        );
        assert_eq!(
            binding.subject.as_bytes()[10..12],
            [0xff, 0xff],
            "an IPv4 address must be carried IPv4-mapped, so one address has one encoding"
        );
    }

    /// An unusable address is refused rather than bound, because a binding for
    /// `::` would admit half the internet or nothing at all.
    #[test]
    fn unusable_addresses_are_refused() {
        assert_eq!(
            binding_for(ip("::")),
            Err(AdmissionRefused::UnusableSubject("an unspecified address"))
        );
        assert_eq!(
            binding_for(ip("ff02::1")),
            Err(AdmissionRefused::UnusableSubject("a multicast address"))
        );
    }

    /// Admission costs what the rate says it costs, and the ledger drops by
    /// exactly that. This is the inbound twin of the funding bound: a device
    /// cannot be admitted for longer than a v1 buyer paid for.
    #[test]
    fn admission_is_paid_for_out_of_a_buyers_credit() {
        let credit = Credit::new();
        credit
            .deposit("https://m", "byte", 1_000)
            .expect("a buyer paid");

        let admission = admit(&credit, "https://m", "byte", ip("192.0.2.7"), 100, 4)
            .expect("four seconds at 100 a second is 400");
        assert_eq!(admission.spent, 400);
        assert_eq!(admission.rate, 100);
        assert_eq!(credit.available("https://m", "byte"), 600);
    }

    /// A device cannot be admitted past what was paid: the refusal names both
    /// numbers, and nothing is spent.
    #[test]
    fn admission_beyond_the_balance_is_refused_and_costs_nothing() {
        let credit = Credit::new();
        credit
            .deposit("https://m", "byte", 50)
            .expect("a buyer paid");

        let refused = admit(&credit, "https://m", "byte", ip("192.0.2.7"), 100, 4)
            .expect_err("400 is more than 50");
        assert_eq!(
            refused,
            AdmissionRefused::Unpaid {
                available: 50,
                needed: 400
            }
        );
        assert_eq!(
            credit.available("https://m", "byte"),
            50,
            "a refused admission spent the buyer's credit anyway"
        );
    }

    /// A zero rate is refused: `Set { rate: Some(0) }` means *closed*, so asking
    /// to admit at zero is asking to admit nothing while taking the money.
    #[test]
    fn a_zero_rate_is_refused() {
        let credit = Credit::new();
        credit
            .deposit("https://m", "byte", 1_000)
            .expect("a buyer paid");
        assert_eq!(
            admit(&credit, "https://m", "byte", ip("192.0.2.7"), 0, 10),
            Err(AdmissionRefused::NothingAsked)
        );
        assert_eq!(credit.available("https://m", "byte"), 1_000);
    }

    /// Admission for a different mint than the buyer paid into is refused, for
    /// the same reason funding is: vouchers of one mint are not another's.
    #[test]
    fn admission_cannot_be_paid_for_from_another_mint() {
        let credit = Credit::new();
        credit
            .deposit("https://a", "byte", 1_000)
            .expect("a buyer paid");
        let refused = admit(&credit, "https://b", "byte", ip("192.0.2.7"), 10, 1)
            .expect_err("the wrong mint's credit");
        assert_eq!(
            refused,
            AdmissionRefused::Unpaid {
                available: 0,
                needed: 10
            }
        );
    }
}
