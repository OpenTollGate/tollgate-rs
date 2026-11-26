//! Which price policy a node should run, argued from what the v1 advert actually
//! promises.
//!
//! `PricePolicy` has no default on purpose: it decides how a buyer's money is treated
//! when the token they hand over is worth less than the quantity they asked for. That
//! is a commercial choice, and this file does not make it. What it does is make the
//! difference concrete, so the choice is made with the numbers in view rather than
//! from a paragraph of prose.
//!
//! The facts that constrain it, from the code rather than from opinion:
//!
//! - v1's advertisement is a **specific offer**: `price_per_step` says that
//!   `price_sats` buys `step_size` units. It is not a rate, and it is not "whatever
//!   your token turns out to be worth".
//! - `V1Purchase.amount` is the quantity the buyer believes they are buying, so a
//!   buyer who asks for 500 has been told what 500 costs.
//! - The bridge credits **bytes actually deposited at the mint's valuation**, because
//!   that is the only number that agrees with the wallet holding the money.
//!
//! Put together: a token worth less than the advertised price for the quantity asked
//! means the offer was not met. `AtMintValuation` honours it anyway and reports the
//! adjustment; `RequireAdvertisedPrice` refuses the purchase. The tests below show
//! both, and what the buyer is left holding in each case.

use std::sync::Arc;

use tollgate_net::bridge::exchange::{PricePolicy, Refusal, V1Purchase, accept};
use tollgate_net::bridge::{BridgeFunding, Credit};
use tollgate_net::control::Response;
use tollgate_net::merchant::{MerchantClient, Request};

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::sync::Mutex;

/// A stand-in `merchantd` that reports a fixed deposit answer. It holds no money; the
/// point is to control what the "mint" says a token was worth.
fn fake_merchantd(
    answer: serde_json::Value,
    seen: Arc<Mutex<Vec<Request>>>,
) -> (PathBuf, std::thread::JoinHandle<()>) {
    let dir = std::env::temp_dir().join(format!(
        "price-policy-{}-{:?}",
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
            let body = serde_json::to_vec(&Response::Ok { data: answer }).expect("encode");
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

/// The advertisement's promise is a specific quantity for a specific price, so a token
/// worth less than that is a short offer. This asserts what each policy does with one,
/// side by side, so the difference is a number rather than a description.
///
/// The buyer asked for 500 bytes; the mint says their token is worth 120.
#[test]
fn the_two_policies_on_one_short_offer() {
    // --- AtMintValuation: honour it, credit 120, and say so.
    let seen_a = Arc::new(Mutex::new(Vec::new()));
    let (sock_a, h_a) = fake_merchantd(
        serde_json::json!({ "mint": "https://m", "unit": "byte", "amount": 120 }),
        Arc::clone(&seen_a),
    );
    let credit_a = Arc::new(Credit::new());
    let funding_a = BridgeFunding::new(MerchantClient::new(&sock_a), Arc::clone(&credit_a));
    let receipt = accept(&funding_a, PricePolicy::AtMintValuation, &purchase(500))
        .expect("honoured at valuation");
    h_a.join().expect("thread");

    // The buyer is admitted with 120 bytes of access, not the 500 they asked for, and
    // the receipt says both numbers.
    assert_eq!(receipt.credited, 120);
    assert_eq!(credit_a.available("https://m", "byte"), 120);
    assert!(
        receipt.adjusted.contains("120") && receipt.adjusted.contains("500"),
        "the receipt must state what was taken and what was asked: {}",
        receipt.adjusted
    );

    // --- RequireAdvertisedPrice: refuse the purchase, keep the value.
    let seen_b = Arc::new(Mutex::new(Vec::new()));
    let (sock_b, h_b) = fake_merchantd(
        serde_json::json!({ "mint": "https://m", "unit": "byte", "amount": 120 }),
        Arc::clone(&seen_b),
    );
    let credit_b = Arc::new(Credit::new());
    let funding_b = BridgeFunding::new(MerchantClient::new(&sock_b), Arc::clone(&credit_b));
    let refused = accept(
        &funding_b,
        PricePolicy::RequireAdvertisedPrice,
        &purchase(500),
    )
    .expect_err("refused as a short offer");
    h_b.join().expect("thread");

    match refused {
        Refusal::ShortOfPrice { credited, required } => {
            assert_eq!(credited, 120);
            assert_eq!(required, 500);
        }
        other => panic!("expected a shortfall, got {other:?}"),
    }
    // Both policies deposited the token — the money moved either way, because the
    // deposit is the point of no return and happens before the policy is applied. What
    // differs is whether the buyer is TOLD they got access or told the offer was short.
    assert_eq!(
        credit_b.available("https://m", "byte"),
        120,
        "a refused purchase must not lose the buyer's value"
    );

    // The two outcomes differ in exactly one way: whether an entitlement was recorded
    // as a completed purchase. Both leave 120 bytes of value held for that buyer.
    assert_eq!(
        credit_a.available("https://m", "byte"),
        credit_b.available("https://m", "byte"),
        "the value held is the same; only the answer differs"
    );
}

/// A token worth at least what was asked is honoured under either policy. Without this,
/// the strict policy would look like it refuses everything.
#[test]
fn a_full_offer_is_honoured_under_both_policies() {
    for policy in [
        PricePolicy::AtMintValuation,
        PricePolicy::RequireAdvertisedPrice,
    ] {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let (sock, handle) = fake_merchantd(
            serde_json::json!({ "mint": "https://m", "unit": "byte", "amount": 500 }),
            Arc::clone(&seen),
        );
        let credit = Arc::new(Credit::new());
        let funding = BridgeFunding::new(MerchantClient::new(&sock), Arc::clone(&credit));
        let receipt = accept(&funding, policy, &purchase(500))
            .unwrap_or_else(|e| panic!("{policy:?} refused a full offer: {}", e.message()));
        handle.join().expect("thread");
        assert_eq!(receipt.credited, 500);
        assert!(
            receipt.adjusted.is_empty(),
            "{policy:?} adjusted a full offer: {}",
            receipt.adjusted
        );
    }
}

/// An overpayment is honoured at its real value under both policies, and neither
/// pretends the buyer only paid for what they asked.
#[test]
fn an_overpayment_is_credited_at_its_real_value() {
    for policy in [
        PricePolicy::AtMintValuation,
        PricePolicy::RequireAdvertisedPrice,
    ] {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let (sock, handle) = fake_merchantd(
            serde_json::json!({ "mint": "https://m", "unit": "byte", "amount": 900 }),
            Arc::clone(&seen),
        );
        let credit = Arc::new(Credit::new());
        let funding = BridgeFunding::new(MerchantClient::new(&sock), Arc::clone(&credit));
        let receipt = accept(&funding, policy, &purchase(500))
            .unwrap_or_else(|e| panic!("{policy:?} refused an overpayment: {}", e.message()));
        handle.join().expect("thread");
        assert_eq!(
            receipt.credited, 900,
            "{policy:?} discarded the buyer's overpayment"
        );
        assert_eq!(credit.available("https://m", "byte"), 900);
    }
}
