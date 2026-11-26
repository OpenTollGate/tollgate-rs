//! The bridge as a [`Funding`] source, so a v2 session can be paid for with v1
//! revenue without the session knowing where the money came from.
//!
//! [`client::run`](crate::client::run) takes an `Arc<dyn Funding>` and asks it for
//! vouchers when a channel needs funding. Everything above this file is
//! protocol: the session, the ratchet, the top-ups. Everything below it is money:
//! `merchantd` holds value and mints what it sells.
//!
//! The bridge sits exactly between them and does one thing — it makes a v1
//! buyer's payment visible to that funding call, and refuses to let the call
//! take more than a v1 buyer actually paid. It does not speak v2, and it does not
//! mint. A bridge that did either would be a second product.
//!
//! # The honest gap this file exposes
//!
//! v2's `Funding::vouchers` asks for a *number of vouchers*, but the money it
//! spends is `merchantd`'s. So "fund from v1 credit" is not a matter of passing
//! the request through: the bridge must first move the v1 buyer's token into
//! `merchantd` (a real swap at the mint), and only then may it ask for vouchers.
//! That is why [`BridgeFunding::vouchers`] deposits before it funds, and why it
//! still refuses to release more than the ledger holds.

use std::sync::Arc;

use anyhow::{Context, Result};

use crate::bridge::Credit;
use crate::channel::Funding;
use crate::merchant::{Held, MerchantClient};

/// A [`Funding`] source backed by `merchantd`, bounded by what v1 buyers paid.
pub struct BridgeFunding {
    merchant: MerchantClient,
    credit: Arc<Credit>,
}

impl std::fmt::Debug for BridgeFunding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BridgeFunding")
            .field("merchant", &self.merchant)
            .finish_non_exhaustive()
    }
}

impl BridgeFunding {
    /// Fund a v2 session out of `merchantd`, releasing at most what the ledger
    /// holds for the mint and unit in question.
    pub fn new(merchant: MerchantClient, credit: Arc<Credit>) -> Self {
        Self { merchant, credit }
    }

    /// Take a v1 buyer's token into `merchantd`, and only then record it.
    ///
    /// Order matters: the deposit is the point of no return — the token's value
    /// leaves the buyer and lands in the operator's wallet. Recording credit
    /// *before* a successful deposit would let the bridge promise money it never
    /// received; recording it after means a failed deposit leaves the ledger
    /// untouched and the token still the buyer's problem to retry.
    ///
    /// The amount credited is what `merchantd` says it actually took, not what
    /// the caller claimed: a token that is smaller than advertised, or of a
    /// different mint, is credited at its real value.
    pub fn accept_payment(&self, token: &str) -> Result<Held> {
        let taken = self
            .merchant
            .deposit_held(token)
            .context("deposit a v1 buyer's token into merchantd")?;
        self.credit
            .deposit(&taken.mint, &taken.unit, taken.amount)
            .map_err(|e| anyhow::anyhow!(e.message()))?;
        Ok(taken)
    }
}

impl Funding for BridgeFunding {
    /// Fund a channel, but only out of credit a v1 buyer created.
    ///
    /// This is the bound the bridge exists for. `merchantd` holds its own wallet
    /// and will print vouchers for whatever it is asked, so calling it directly
    /// would be unbounded: a v2 session could fund itself from the node's mint
    /// without any v1 buyer having paid. So the credit is spent FIRST, and only
    /// what was actually released is asked for.
    ///
    /// If merchantd then fails, the released credit is returned: the money never
    /// arrived, so the ledger must not keep it. A failure here is therefore
    /// neutral on the ledger, which is what lets a session retry.
    fn vouchers(&self, mint: &str, unit: &str, amount: u64) -> Result<String> {
        let released = self.credit.take(mint, unit, amount);
        if released == 0 {
            anyhow::bail!(
                "no credit for {amount} {unit} from {mint}: a v1 buyer has not paid for it"
            );
        }
        match self.merchant.vouchers(mint, unit, released) {
            Ok(token) => Ok(token),
            Err(e) => {
                self.credit.refund(mint, unit, released);
                Err(e).context("fund a channel from credit merchantd had already taken")
            }
        }
    }

    fn deposit(&self, token: &str) -> Result<()> {
        self.accept_payment(token).map(|_| ())
    }
}

// `Request` and `Response` are re-exported so this file's tests can build the
// same messages the client sends, rather than a private imitation of them.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::Response;
    use crate::merchant::Request;
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixListener;
    use std::path::PathBuf;

    /// A stand-in `merchantd`: answers one request per connection with a fixed
    /// reply, and records what it was asked. This is a test double for the socket
    /// protocol only — it holds no money and mints nothing.
    fn fake_merchantd(
        reply: Response,
        seen: Arc<std::sync::Mutex<Vec<Request>>>,
    ) -> (PathBuf, std::thread::JoinHandle<()>) {
        let dir = std::env::temp_dir().join(format!(
            "bridge-merchant-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("m.sock");
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).expect("bind");
        let handle = std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut line = String::new();
                let mut reader = BufReader::new(stream.try_clone().expect("clone"));
                let _ = reader.read_line(&mut line);
                if let Ok(request) = serde_json::from_str::<Request>(&line) {
                    seen.lock().expect("not poisoned").push(request);
                }
                let body = serde_json::to_vec(&reply).expect("encode");
                let _ = stream.write_all(&body);
            }
        });
        (path, handle)
    }

    /// A v1 buyer's payment must be deposited before it is credited, and the
    /// credit must be what merchantd said it took — not what the buyer claimed.
    #[test]
    fn a_payment_is_credited_at_what_merchantd_took() {
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (sock, handle) = fake_merchantd(
            Response::Ok {
                data: serde_json::json!({ "mint": "https://m", "unit": "sat", "amount": 42 }),
            },
            Arc::clone(&seen),
        );
        let credit = Arc::new(Credit::new());
        let funding = BridgeFunding::new(MerchantClient::new(&sock), Arc::clone(&credit));

        let held = funding.accept_payment("cashuBtoken").expect("accept");
        handle.join().expect("thread");

        assert_eq!(held.amount, 42);
        assert_eq!(
            credit.available("https://m", "sat"),
            42,
            "credit must be what merchantd took"
        );
        // And the buyer's token really was handed to merchantd, not merely
        // counted: the ledger is not a substitute for moving the money.
        let seen = seen.lock().expect("not poisoned");
        match seen.first() {
            Some(Request::Deposit { token }) => assert_eq!(token, "cashuBtoken"),
            other => panic!("expected a deposit, saw {other:?}"),
        }
    }

    /// A deposit merchantd refused must credit nothing. Promising money the
    /// operator never received is the failure this ordering exists to prevent.
    #[test]
    fn a_refused_deposit_credits_nothing() {
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (sock, handle) = fake_merchantd(
            Response::Error {
                message: "mint does not know that token".into(),
            },
            Arc::clone(&seen),
        );
        let credit = Arc::new(Credit::new());
        let funding = BridgeFunding::new(MerchantClient::new(&sock), Arc::clone(&credit));

        let err = funding
            .accept_payment("cashuBstolen")
            .expect_err("must refuse");
        handle.join().expect("thread");
        assert!(
            format!("{err:#}").contains("merchantd"),
            "the refusal must name where it came from: {err:#}"
        );
        assert_eq!(
            credit.available("https://m", "sat"),
            0,
            "a refused deposit credited money anyway"
        );
    }

    /// The bound this bridge exists for: a session must not be able to fund more
    /// than a v1 buyer paid, even though merchantd would happily mint it.
    ///
    /// merchantd holds its own wallet and can print vouchers on demand, so asking
    /// it directly is unbounded. The ledger is what makes the bridge a bridge
    /// rather than a free pass to the node's mint: funding must spend credit.
    #[test]
    fn funding_cannot_exceed_what_a_buyer_paid() {
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (sock, handle) = fake_merchantd(
            Response::Ok {
                data: serde_json::json!({ "token": "cashuBvouchers" }),
            },
            Arc::clone(&seen),
        );
        let credit = Arc::new(Credit::new());
        credit
            .deposit("https://m", "sat", 100)
            .expect("a buyer paid");
        let funding = BridgeFunding::new(MerchantClient::new(&sock), Arc::clone(&credit));

        // A session asks for far more than was paid. The bridge funds what the
        // ledger holds and no more - it does not error, because a session asking
        // for a capacity the deposits do not yet cover is normal, and the honest
        // answer is the smaller number.
        let got = funding
            .vouchers("https://m", "sat", 10_000)
            .expect("funds what it has");
        assert_eq!(got, "cashuBvouchers");

        // What matters is that merchantd was asked for 100, not 10_000: the bound
        // is in what was requested, not in what was returned.
        match seen.lock().expect("not poisoned").first() {
            Some(Request::Fund { amount, .. }) => assert_eq!(
                *amount, 100,
                "asked merchantd for more vouchers than the buyer paid for"
            ),
            other => panic!("expected a fund request, saw {other:?}"),
        }
        drop(handle);
        // And the whole payment was spent, exactly once.
        assert_eq!(credit.available("https://m", "sat"), 0);
    }

    /// What the ledger can pay for, it does pay for — and it spends the credit,
    /// so a second channel cannot spend the same money.
    #[test]
    fn funding_spends_the_credit_it_used() {
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (sock, handle) = fake_merchantd(
            Response::Ok {
                data: serde_json::json!({ "token": "cashuBvouchers" }),
            },
            Arc::clone(&seen),
        );
        let credit = Arc::new(Credit::new());
        credit
            .deposit("https://m", "sat", 100)
            .expect("a buyer paid");
        let funding = BridgeFunding::new(MerchantClient::new(&sock), Arc::clone(&credit));

        funding.vouchers("https://m", "sat", 60).expect("fund");
        handle.join().expect("thread");
        assert_eq!(
            credit.available("https://m", "sat"),
            40,
            "funding must spend the credit it used"
        );
    }

    /// Funding a channel asks merchantd for exactly the vouchers the session
    /// asked for, in the mint and unit it named.
    #[test]
    fn funding_asks_merchantd_for_the_vouchers_the_session_wanted() {
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (sock, handle) = fake_merchantd(
            Response::Ok {
                data: serde_json::json!({ "token": "cashuBvouchers" }),
            },
            Arc::clone(&seen),
        );
        let credit = Arc::new(Credit::new());
        credit
            .deposit("https://m", "sat", 500)
            .expect("a buyer paid");
        let funding = BridgeFunding::new(MerchantClient::new(&sock), Arc::clone(&credit));

        let token = funding.vouchers("https://m", "sat", 500).expect("fund");
        handle.join().expect("thread");
        assert_eq!(token, "cashuBvouchers");
        match seen.lock().expect("not poisoned").first() {
            Some(Request::Fund { mint, unit, amount }) => {
                assert_eq!(mint, "https://m");
                assert_eq!(unit, "sat");
                assert_eq!(*amount, 500);
            }
            other => panic!("expected a fund request, saw {other:?}"),
        }
    }
}
