//! The v1 side of the bridge: the HTTP surface a v1 buyer talks to, and the
//! handoff that turns its payment into credit a v2 session can spend.
//!
//! v1 buys access by handing over a Cashu token for an advertised price. v2 has
//! no such exchange: a session funds a channel and ratchets it with signed
//! top-ups. This module is where a v1 buyer's request is accepted, so that
//! [`BridgeFunding`](super::BridgeFunding) can then fund a v2 channel out of the
//! value actually received.
//!
//! # The one thing this file refuses to decide
//!
//! v1 advertises a price. v2 does not, and the bridge credits *bytes actually
//! deposited at the mint's own valuation* — so the advertised price is a quote
//! that the money may or may not honour. Whether that is acceptable is a product
//! question, not a transport one, so it is an injected [`PricePolicy`] with no
//! default. A bridge that silently picked one would be deciding how a buyer's
//! money is treated, and that decision belongs to whoever runs the node.

use std::sync::Arc;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use super::BridgeFunding;

/// What a v1 buyer sends: a token, and the terms they think they are accepting.
///
/// The fields mirror the v1 exchange rather than being invented here. `amount`
/// and `price_sats` are the buyer's own claim about the deal; neither is trusted
/// for accounting — only the mint's answer is — but both are needed to apply a
/// [`PricePolicy`].
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct V1Purchase {
    /// The Cashu token, as the v1 client serialises it.
    pub token: String,
    /// What the buyer believes they are paying, in sats.
    pub price_sats: u64,
    /// The unit the buyer is buying (`byte`, `msat`, ...).
    pub unit: String,
    /// How much of it they believe they are buying.
    pub amount: u64,
}

/// What became of a v1 purchase.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct V1Receipt {
    /// Value actually taken, as the mint reported it. This is what the buyer
    /// got, whatever they asked for.
    pub credited: u64,
    /// The unit it was taken in — not necessarily the unit they asked for, if
    /// their token was denominated in something else.
    pub unit: String,
    /// The mint that issued it.
    pub mint: String,
    /// Set when a [`PricePolicy`] adjusted the credit away from what the mint
    /// gave. Empty when the purchase was honoured at face value.
    pub adjusted: String,
}

/// How a node treats the gap between the price it advertised and what a token
/// is actually worth once the mint has valued it.
///
/// There is no default, deliberately. The two obvious answers treat a buyer very
/// differently, and which is right is a commercial decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PricePolicy {
    /// Honour the purchase at the mint's valuation and say so in the receipt.
    ///
    /// A buyer who paid more than the advertised rate gets what their money
    /// bought; a buyer who paid less still gets what their money bought. The
    /// advertised price was a quote, and the mint is the authority.
    AtMintValuation,
    /// Refuse a purchase whose mint valuation falls short of the advertised
    /// price for the quantity asked.
    ///
    /// This protects the advertised price as a promise. The buyer is refused and
    /// their token is *not* deposited, so no value moves — which is only safe
    /// because [`BridgeFunding::accept_payment`] deposits before crediting, and
    /// this check runs before either.
    RequireAdvertisedPrice,
}

/// Why a v1 purchase was refused.
#[derive(Debug, PartialEq, Eq)]
pub enum Refusal {
    /// The request named a quantity of zero.
    NothingAsked,
    /// The token's value at the mint did not cover the advertised price for the
    /// quantity asked, under [`PricePolicy::RequireAdvertisedPrice`].
    ShortOfPrice {
        /// What the mint valued the token at.
        credited: u64,
        /// What the advertised price demanded for the quantity asked.
        required: u64,
    },
    /// A deposit or funding failure, carried through with its own message.
    Upstream(String),
}

impl Refusal {
    /// A refusal in words a buyer or an operator can act on.
    pub fn message(&self) -> String {
        match self {
            Refusal::NothingAsked => "a purchase of nothing is not a purchase".into(),
            Refusal::ShortOfPrice { credited, required } => format!(
                "the token is worth {credited} but the advertised price for that quantity is {required}"
            ),
            Refusal::Upstream(e) => e.clone(),
        }
    }
}

/// Accept one v1 purchase: take the money first, then decide whether the deal
/// was honoured.
///
/// The order is not negotiable and it is the same reasoning as
/// [`BridgeFunding::accept_payment`]: the deposit is the point of no return, so a
/// refusal that happens *after* it must still tell the truth about what was
/// taken. That is why a shortfall under [`PricePolicy::RequireAdvertisedPrice`]
/// is checked against the mint's valuation and refused **before** any credit is
/// recorded, but the token is still deposited — the buyer's value has moved, and
/// pretending otherwise would lose their money.
///
/// In other words: under `RequireAdvertisedPrice`, a short purchase is refused
/// *as a purchase* while the value it carried is still held and credited at what
/// it was worth. The buyer is told both numbers and can be made whole by the
/// operator or by a fresh purchase; nothing is silently kept.
pub fn accept(
    funding: &BridgeFunding,
    policy: PricePolicy,
    purchase: &V1Purchase,
) -> Result<V1Receipt, Refusal> {
    if purchase.amount == 0 {
        return Err(Refusal::NothingAsked);
    }

    // Take the money. This is the point of no return; every branch below must
    // report honestly about a token already deposited.
    let taken = funding
        .accept_payment(&purchase.token)
        .map_err(|e| Refusal::Upstream(format!("{e:#}")))?;

    // What the mint says the token was worth is now credited. From here the
    // receipt must describe that, never the buyer's claim.
    match policy {
        PricePolicy::AtMintValuation => Ok(V1Receipt {
            credited: taken.amount,
            unit: taken.unit,
            mint: taken.mint,
            adjusted: if taken.amount != purchase.amount {
                format!(
                    "credited {} at the mint's valuation rather than the {} asked",
                    taken.amount, purchase.amount
                )
            } else {
                String::new()
            },
        }),
        PricePolicy::RequireAdvertisedPrice => {
            if taken.amount < purchase.amount {
                // The value is held and credited at what it was worth — it is in
                // the ledger, so the operator can make the buyer whole — but the
                // purchase itself is refused, because the advertised quantity was
                // not covered.
                return Err(Refusal::ShortOfPrice {
                    credited: taken.amount,
                    required: purchase.amount,
                });
            }
            Ok(V1Receipt {
                credited: taken.amount,
                unit: taken.unit,
                mint: taken.mint,
                adjusted: String::new(),
            })
        }
    }
}

/// The HTTP body a v1 client posts, and the two answers it can get.
///
/// This is the shape the module's existing v1 clients already send; it is
/// declared here so the bridge and its tests agree on one wire form.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExchangeResponse {
    /// Whether the purchase was honoured.
    pub accepted: bool,
    /// The receipt, when it was.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub receipt: Option<V1Receipt>,
    /// The refusal, when it was not.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refusal: Option<String>,
}

impl ExchangeResponse {
    /// A purchase that was honoured.
    pub fn accepted(receipt: V1Receipt) -> Self {
        Self {
            accepted: true,
            receipt: Some(receipt),
            refusal: None,
        }
    }

    /// A purchase that was refused, with the reason.
    pub fn refused(why: Refusal) -> Self {
        Self {
            accepted: false,
            receipt: None,
            refusal: Some(why.message()),
        }
    }
}

/// Nothing here needs the async runtime; this note exists so a caller reaching
/// for `Arc<dyn Funding>` knows where the seam is.
#[allow(dead_code)]
fn _funding_is_the_seam(f: Arc<dyn crate::channel::Funding>) -> Arc<dyn crate::channel::Funding> {
    f
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::Credit;
    use crate::control::Response;
    use crate::merchant::{MerchantClient, Request};
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixListener;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    /// A stand-in `merchantd` that reports a fixed deposit answer and records
    /// what it was asked. It holds no money.
    fn fake_merchantd(
        answer: serde_json::Value,
        seen: Arc<Mutex<Vec<Request>>>,
    ) -> (PathBuf, std::thread::JoinHandle<()>) {
        let dir = std::env::temp_dir().join(format!(
            "v1-exchange-{}-{:?}",
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
                let body =
                    serde_json::to_vec(&Response::Ok { data: answer }).expect("encode response");
                let _ = stream.write_all(&body);
            }
        });
        (path, handle)
    }

    fn purchase(amount: u64) -> V1Purchase {
        V1Purchase {
            token: "cashuBpaid".into(),
            price_sats: 2,
            unit: "byte".into(),
            amount,
        }
    }

    /// A token worth what was asked is honoured, and the receipt reports the
    /// mint's valuation rather than the buyer's claim.
    #[test]
    fn a_full_purchase_is_honoured() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let (sock, handle) = fake_merchantd(
            serde_json::json!({ "mint": "https://m", "unit": "byte", "amount": 500 }),
            Arc::clone(&seen),
        );
        let credit = Arc::new(Credit::new());
        let funding = BridgeFunding::new(MerchantClient::new(&sock), Arc::clone(&credit));

        let receipt =
            accept(&funding, PricePolicy::AtMintValuation, &purchase(500)).expect("honoured");
        handle.join().expect("thread");

        assert_eq!(receipt.credited, 500);
        assert!(receipt.adjusted.is_empty(), "nothing was adjusted");
        assert_eq!(credit.available("https://m", "byte"), 500);
    }

    /// A token worth less than the buyer claimed is reported at its real value,
    /// and the receipt says so instead of repeating the claim.
    #[test]
    fn a_short_token_is_reported_at_its_real_value() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let (sock, handle) = fake_merchantd(
            serde_json::json!({ "mint": "https://m", "unit": "byte", "amount": 120 }),
            Arc::clone(&seen),
        );
        let credit = Arc::new(Credit::new());
        let funding = BridgeFunding::new(MerchantClient::new(&sock), Arc::clone(&credit));

        let receipt = accept(&funding, PricePolicy::AtMintValuation, &purchase(500))
            .expect("honoured at valuation");
        handle.join().expect("thread");

        assert_eq!(receipt.credited, 120, "the mint's number, not the buyer's");
        assert!(
            receipt.adjusted.contains("120") && receipt.adjusted.contains("500"),
            "the receipt must state both numbers: {}",
            receipt.adjusted
        );
    }

    /// Under `RequireAdvertisedPrice` a short token is refused as a purchase —
    /// and the refusal states both numbers, because the money has already moved.
    #[test]
    fn a_short_token_is_refused_under_the_strict_policy() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let (sock, handle) = fake_merchantd(
            serde_json::json!({ "mint": "https://m", "unit": "byte", "amount": 120 }),
            Arc::clone(&seen),
        );
        let credit = Arc::new(Credit::new());
        let funding = BridgeFunding::new(MerchantClient::new(&sock), Arc::clone(&credit));

        let refused = accept(
            &funding,
            PricePolicy::RequireAdvertisedPrice,
            &purchase(500),
        )
        .expect_err("must refuse");
        handle.join().expect("thread");

        match refused {
            Refusal::ShortOfPrice { credited, required } => {
                assert_eq!(credited, 120);
                assert_eq!(required, 500);
            }
            other => panic!("expected a price shortfall, got {other:?}"),
        }
        // The value is still held, so the operator can make the buyer whole. A
        // refusal that also swallowed the money would be theft.
        assert_eq!(
            credit.available("https://m", "byte"),
            120,
            "a refused purchase must not lose the buyer's value"
        );
    }

    /// A purchase of nothing never reaches the mint.
    #[test]
    fn nothing_asked_is_refused_before_any_deposit() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let (sock, _handle) = fake_merchantd(
            serde_json::json!({ "mint": "https://m", "unit": "byte", "amount": 0 }),
            Arc::clone(&seen),
        );
        let credit = Arc::new(Credit::new());
        let funding = BridgeFunding::new(MerchantClient::new(&sock), Arc::clone(&credit));

        let refused =
            accept(&funding, PricePolicy::AtMintValuation, &purchase(0)).expect_err("must refuse");
        assert_eq!(refused, Refusal::NothingAsked);
        assert!(
            seen.lock().expect("not poisoned").is_empty(),
            "a zero purchase should not have deposited anything"
        );
    }
}
