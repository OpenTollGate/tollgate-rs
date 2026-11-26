//! The v1-to-v2 bridge: what lets a device that only speaks the v1 request /
//! response exchange reach a v2 gateway.
//!
//! v2 is the native protocol and stays the native protocol. This bridge exists
//! only for devices that cannot speak v1 — it is a compatibility wrapper, not a
//! second product.
//!
//! # Why the two sides cannot simply be connected
//!
//! The two protocols disagree about where money lives. A v1 purchase is a
//! Cashu token handed over for access; v2 has no such message at all. A v2
//! session instead funds a Spilman channel and then ratchets it with signed
//! top-ups, and the vouchers it deals in come from a mint the gateway accepts.
//!
//! So there is nothing to "translate" message for message. What the bridge does
//! is narrower, and it is worth stating plainly because the temptation to do
//! more is exactly how a compatibility shim turns into a second money path:
//!
//! 1. a v1 buyer hands over a Cashu token — the same token a v1 gateway takes;
//! 2. the bridge deposits that token into `merchantd`, which holds everything of
//!    value the node owns (see `Request::Deposit`);
//! 3. the v2 session's channel is funded from there by the ordinary `Request::Fund`
//!    path, which is already what every v2 buyer uses.
//!
//! The bridge **mints nothing and prices nothing**. It cannot conjure vouchers:
//! it can only deposit what a v1 buyer actually paid, and the amount a channel
//! gets is bounded by what was deposited. That bound is the whole safety
//! argument, and it is what the tests below pin.
//!
//! # What this does not do
//!
//! It does not speak the v2 wire itself — `tollgate-net`'s client already does,
//! and this bridge sits beside it rather than reimplementing it. It does not
//! decide a price: v1 states a price in its advertisement and v2 does not state
//! one at all, so the bridge must be told what a deposit is worth and will
//! refuse to guess.

pub mod admit;
pub mod admit_enforcer;
pub mod admit_fips;
pub mod enforcer_daemon;
pub mod exchange;
pub mod funding;
pub mod http;

pub use exchange::{PricePolicy, Refusal, V1Purchase, V1Receipt, accept};
pub use funding::BridgeFunding;
pub use http::{BridgeConfig, BridgeState, router};

use std::collections::BTreeMap;
use std::sync::Mutex;

/// What the bridge knows about money it has taken in and not yet spent.
///
/// Keyed by `(mint, unit)` because vouchers of one mint are not vouchers of
/// another, and a gateway that accepts several mints must not let a deposit at
/// one fund a channel at another.
#[derive(Debug, Default)]
pub struct Credit {
    held: Mutex<BTreeMap<(String, String), u64>>,
}

/// Why a deposit was refused.
#[derive(Debug, PartialEq, Eq)]
pub enum DepositError {
    /// Nothing of value arrived. A zero-amount deposit is not a payment.
    Nothing,
    /// The token did not parse as a Cashu token, or the depositor did not say
    /// which mint and unit it is in. The bridge will not guess either.
    Unreadable(&'static str),
}

impl DepositError {
    /// A refusal in words a caller can log or show. The bridge never reports a
    /// refusal as success, so this is always paired with an `Err`.
    pub fn message(&self) -> String {
        match self {
            DepositError::Nothing => "a deposit of nothing is not a payment".into(),
            DepositError::Unreadable(why) => format!("unreadable deposit: {why}"),
        }
    }
}

impl Credit {
    /// An empty ledger: nothing taken in, nothing to fund.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record `amount` of `mint`'s paper in `unit`, as taken from a v1 buyer.
    ///
    /// This is the bridge's only intake. It is deliberately an accounting step
    /// and not a network one: the token itself goes to `merchantd` through
    /// `Request::Deposit`, which is where it is actually swapped and held. This
    /// ledger exists so the bridge can refuse to fund more than it took.
    pub fn deposit(&self, mint: &str, unit: &str, amount: u64) -> Result<u64, DepositError> {
        if amount == 0 {
            return Err(DepositError::Nothing);
        }
        if mint.is_empty() {
            return Err(DepositError::Unreadable("no mint"));
        }
        if unit.is_empty() {
            return Err(DepositError::Unreadable("no unit"));
        }
        let mut held = self.held.lock().expect("not poisoned");
        let slot = held
            .entry((mint.to_owned(), unit.to_owned()))
            .or_insert(0u64);
        *slot = slot.saturating_add(amount);
        Ok(*slot)
    }

    /// How much of `mint`'s paper in `unit` is available to fund a channel.
    pub fn available(&self, mint: &str, unit: &str) -> u64 {
        let held = self.held.lock().expect("not poisoned");
        held.get(&(mint.to_owned(), unit.to_owned()))
            .copied()
            .unwrap_or(0)
    }

    /// Take up to `amount` for a channel, returning what was actually released.
    ///
    /// This is where the safety bound lives: a channel can never be funded with
    /// more than a v1 buyer actually paid. Asking for more is not an error —
    /// a session may want a capacity the deposits do not yet cover, and the
    /// answer is simply the smaller number, which the caller funds.
    pub fn take(&self, mint: &str, unit: &str, amount: u64) -> u64 {
        let mut held = self.held.lock().expect("not poisoned");
        let slot = held
            .entry((mint.to_owned(), unit.to_owned()))
            .or_insert(0u64);
        let released = (*slot).min(amount);
        *slot -= released;
        released
    }

    /// Return unspent credit, for a channel that was funded with less than it
    /// took or settled without using everything.
    pub fn refund(&self, mint: &str, unit: &str, amount: u64) {
        if amount == 0 {
            return;
        }
        let mut held = self.held.lock().expect("not poisoned");
        let slot = held
            .entry((mint.to_owned(), unit.to_owned()))
            .or_insert(0u64);
        *slot = slot.saturating_add(amount);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bridge's whole safety argument: it can never fund more than a v1
    /// buyer paid, no matter what a session asks for.
    #[test]
    fn a_channel_cannot_be_funded_beyond_what_a_buyer_paid() {
        let credit = Credit::new();
        assert_eq!(credit.deposit("https://mint.example", "sat", 100), Ok(100));

        // A session asks for far more than was paid.
        let released = credit.take("https://mint.example", "sat", 10_000);
        assert_eq!(released, 100, "the bridge released more than it took in");
        assert_eq!(credit.available("https://mint.example", "sat"), 0);

        // And there is nothing left for a second channel.
        assert_eq!(credit.take("https://mint.example", "sat", 1), 0);
    }

    /// Vouchers of one mint are not vouchers of another: a deposit at one must
    /// not fund a channel at another, or a bridge becomes a mint of its own.
    #[test]
    fn a_deposit_does_not_fund_a_different_mint() {
        let credit = Credit::new();
        credit
            .deposit("https://a.example", "sat", 50)
            .expect("deposit");
        assert_eq!(credit.take("https://b.example", "sat", 50), 0);
        assert_eq!(credit.available("https://a.example", "sat"), 50);
    }

    /// The same holds across units.
    #[test]
    fn a_deposit_does_not_fund_a_different_unit() {
        let credit = Credit::new();
        credit.deposit("https://m", "sat", 50).expect("deposit");
        assert_eq!(credit.take("https://m", "msat", 50), 0);
        assert_eq!(credit.available("https://m", "sat"), 50);
    }

    /// Nothing in is nothing out, and a deposit that cannot be attributed to a
    /// mint and unit is refused rather than guessed at.
    #[test]
    fn an_unattributable_deposit_is_refused() {
        let credit = Credit::new();
        assert_eq!(
            credit.deposit("https://m", "sat", 0),
            Err(DepositError::Nothing)
        );
        assert_eq!(
            credit.deposit("", "sat", 10),
            Err(DepositError::Unreadable("no mint"))
        );
        assert_eq!(
            credit.deposit("https://m", "", 10),
            Err(DepositError::Unreadable("no unit"))
        );
        assert_eq!(credit.available("https://m", "sat"), 0);
    }

    /// Deposits accumulate, which is what a buyer making several purchases
    /// across a session produces.
    #[test]
    fn deposits_accumulate() {
        let credit = Credit::new();
        credit.deposit("https://m", "sat", 10).expect("first");
        credit.deposit("https://m", "sat", 15).expect("second");
        assert_eq!(credit.available("https://m", "sat"), 25);
    }

    /// A channel that took less than it asked, or settled with change, returns
    /// the difference — money must not disappear into the bridge.
    #[test]
    fn unspent_credit_can_be_returned() {
        let credit = Credit::new();
        credit.deposit("https://m", "sat", 100).expect("deposit");
        assert_eq!(credit.take("https://m", "sat", 40), 40);
        assert_eq!(credit.available("https://m", "sat"), 60);
        credit.refund("https://m", "sat", 40);
        assert_eq!(credit.available("https://m", "sat"), 100);
    }

    /// Concurrent deposits and takes must not lose money. The ledger is
    /// shared mutable state on a money path, so the arithmetic has to hold
    /// under contention rather than merely when it is serialised by luck.
    #[test]
    fn concurrent_takes_never_exceed_the_balance() {
        use std::sync::Arc;

        let credit = Arc::new(Credit::new());
        credit.deposit("https://m", "sat", 100).expect("deposit");

        let mut handles = Vec::new();
        for _ in 0..8 {
            let credit = Arc::clone(&credit);
            handles.push(std::thread::spawn(move || {
                let mut total = 0u64;
                for _ in 0..50 {
                    total += credit.take("https://m", "sat", 1);
                }
                total
            }));
        }
        let taken: u64 = handles.into_iter().map(|h| h.join().expect("thread")).sum();
        assert_eq!(
            taken, 100,
            "the ledger released a different amount than it held"
        );
        assert_eq!(credit.available("https://m", "sat"), 0);
    }
}
