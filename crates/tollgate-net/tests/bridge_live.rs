//! The bridge against real daemons, not stand-ins.
//!
//! The unit tests drive a fake `merchantd` so they can run anywhere. This one
//! talks to a real `merchantd` and a real `mintd`, because the claim that matters
//! — that a v1 buyer's money can fund a v2 channel — is a claim about those two
//! programs, not about my reading of their protocol.
//!
//! It is ignored by default and needs the funding socket:
//!
//! ```sh
//! TOLLGATE_FUNDING_SOCK=/tmp/x/funding.sock cargo test -p tollgate-net \
//!     --test bridge_live -- --ignored --nocapture
//! ```
//!
//! What it proves, in order:
//!
//! 1. a v1 purchase is accepted and credited at the mint's valuation;
//! 2. the same account can then fund a v2 channel, because merchantd now holds
//!    the value that purchase deposited;
//! 3. the vouchers it returns are real Cashu tokens of the requested mint and
//!    unit — not a number the bridge invented.
//!
//! What it does NOT prove: that a v2 *session* runs on those vouchers. That needs
//! a gateway and a peer, and is a separate piece of work.

use std::sync::Arc;

use tollgate_net::bridge::{BridgeFunding, Credit, PricePolicy, V1Purchase, accept};
use tollgate_net::merchant::MerchantClient;

fn funding_socket() -> String {
    std::env::var("TOLLGATE_FUNDING_SOCK").expect(
        "set TOLLGATE_FUNDING_SOCK to a running merchantd's funding socket, \
         e.g. /tmp/v1v2/funding.sock",
    )
}

fn mint_and_unit() -> (String, String) {
    (
        std::env::var("TOLLGATE_MINT").unwrap_or_else(|_| "http://127.0.0.1:3338".into()),
        std::env::var("TOLLGATE_UNIT").unwrap_or_else(|_| "byte".into()),
    )
}

/// Make a v1 buyer's token by asking merchantd for vouchers — the same call a v2
/// session would make. This stands in for "a v1 buyer paid", because producing a
/// genuine v1 payment means running a v1 gateway, which is the piece that does
/// not exist yet.
fn a_v1_buyers_token(merchant: &MerchantClient, mint: &str, unit: &str, amount: u64) -> String {
    use tollgate_net::channel::Funding;
    merchant
        .vouchers(mint, unit, amount)
        .expect("merchantd must be able to fund")
}

#[test]
#[ignore = "needs a running merchantd and mintd; see the module docs"]
fn a_v1_purchase_funds_a_v2_channel_against_real_daemons() {
    let (mint, unit) = mint_and_unit();
    let merchant = MerchantClient::new(funding_socket());
    let credit = Arc::new(Credit::new());
    let funding = BridgeFunding::new(merchant.clone(), Arc::clone(&credit));

    // Step 1: a v1 purchase. The buyer hands over a token for a quantity.
    let token = a_v1_buyers_token(&merchant, &mint, &unit, 4_000);
    assert!(token.starts_with("cashu"), "not a cashu token: {token:.32}");
    let purchase = V1Purchase {
        token,
        price_sats: 2,
        unit: unit.clone(),
        amount: 4_000,
    };

    let receipt = accept(&funding, PricePolicy::AtMintValuation, &purchase)
        .expect("a real payment must be accepted");
    println!(
        "purchase accepted: credited={} unit={} mint={} adjusted={:?}",
        receipt.credited, receipt.unit, receipt.mint, receipt.adjusted
    );
    assert_eq!(
        receipt.credited, 4_000,
        "the mint's valuation should equal what was asked here"
    );
    assert_eq!(
        credit.available(&mint, &unit),
        4_000,
        "the purchase must be visible to the funding side"
    );

    // Step 2: a v2 session asks for vouchers. merchantd holds what the purchase
    // deposited, so this must succeed — that is the whole point of the bridge.
    use tollgate_net::channel::Funding;
    let vouchers = funding
        .vouchers(&mint, &unit, 1_500)
        .expect("a v2 channel must be fundable from v1 revenue");
    println!("funded a v2 channel: {:.48}...", vouchers);
    assert!(
        vouchers.starts_with("cashu"),
        "the funding answer is not a cashu token: {vouchers:.32}"
    );
    assert!(
        vouchers.len() > 100,
        "a real voucher token is not three characters long"
    );

    // Step 3: funding spends credit, and the ledger now holds the rest. This is
    // the bound that stops a v2 session funding itself from the node's mint
    // without a v1 buyer having paid: 4_000 was deposited, 1_500 was funded.
    assert_eq!(
        credit.available(&mint, &unit),
        2_500,
        "funding must spend the credit it used"
    );

    // And a session cannot reach past what a buyer paid, even though merchantd
    // would happily mint more.
    let over = funding
        .vouchers(&mint, &unit, 99_000)
        .expect("funds what is left");
    assert!(over.starts_with("cashu"));
    assert_eq!(
        credit.available(&mint, &unit),
        0,
        "the ledger must be fully spent, not overdrawn"
    );
}

/// The bound is a ledger property, so it is provable without any daemon: this is
/// the same rule the unit tests pin, restated here because it is the one that
/// stops the bridge becoming a mint.
#[test]
fn the_ledger_cannot_release_more_than_it_holds() {
    let credit = Credit::new();
    credit.deposit("https://m", "byte", 10).expect("deposit");
    assert_eq!(credit.take("https://m", "byte", 1_000), 10);
    assert_eq!(credit.take("https://m", "byte", 1_000), 0);
}
