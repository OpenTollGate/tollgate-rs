//! Selling this node's own vouchers, and buying a peer's.
//!
//! **Not part of the TollGate protocol.** No TollGate message buys, sells or
//! swaps a voucher; acquiring them happens before a session and outside it. A
//! node that offers no market at all still works — its peers simply have to
//! arrive holding its vouchers already.
//!
//! # Selling needs no protocol of its own
//!
//! `docs/design/market/market-protocol.md` is explicit about this: buying a
//! node's own vouchers is a standard Cashu mint operation against the
//! `mint_url` it already advertises — a NUT-04 quote, an invoice, a mint. The
//! market endpoints exist only for what Cashu has no answer to, which is
//! pricing and swapping vouchers *across* mints, and this node implements none
//! of that yet. What it serves here is the one thing a buyer cannot get from
//! the mint API: the price signal, at
//! [`INFO_PATH`](crate::market::INFO_PATH).
//!
//! # Where the price lives
//!
//! A mint quote asks the payment processor for an invoice, and cdk converts
//! the quote amount for it — for sats, msats and the fiat units it has rates
//! for. It has no rate for a byte and cannot have one: **bolt11 is denominated
//! in msat, so there is no such thing as an invoice for 1024 bytes.** What a
//! byte of *this* node's capacity is worth is the operator's to say.
//!
//! So the price sits in the payment processor, as [`Price`] — bytes per sat,
//! one number, changeable while the node runs. [`PricedBolt11`] converts a
//! quote's byte amount into an invoice in millisats on the way out, and the
//! payment back into bytes on the way in, so the mint stays purely
//! byte-denominated and every buyer speaks ordinary Cashu.
//!
//! # What is simulated
//!
//! The Lightning backend is cdk's fake wallet: it writes real bolt11 invoices
//! and then pays them itself, immediately. Everything either side of it is
//! real — the quote is priced, the mint will not issue until it has seen the
//! payment, and the proofs are ordinary blind signatures from a real keyset
//! that land in a real spent-proof set. A node that settles in actual sats
//! swaps the backend and changes nothing else.
//!
//! A price of zero closes the market: quotes are refused, so nothing is sold.

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use cdk::nuts::CurrencyUnit;
use cdk::types::FeeReserve;
use cdk_common::Amount;
use cdk_common::payment::{
    CreateIncomingPaymentResponse, Error as PaymentError, Event, IncomingPaymentOptions,
    MakePaymentResponse, MintPayment, OutgoingPaymentOptions, PaymentIdentifier,
    PaymentQuoteResponse, SettingsResponse, WaitPaymentResponse,
};
use cdk_fake_wallet::FakeWallet;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tracing::{debug, warn};

/// The price signal: what this node sells its own capacity for.
pub const INFO_PATH: &str = "/tollgate/market/v1/info";

/// How long to wait for a quote to be paid before giving up on a purchase.
///
/// A Lightning payment is not instant, and nothing downstream can start until
/// it lands. Bounded so that a channel is never blocked forever on one
/// invoice: the caller retries the whole purchase instead.
const PAYMENT_TIMEOUT: Duration = Duration::from_secs(10);

/// What this node charges for its own capacity, in bytes per sat.
///
/// Bytes per sat rather than sats per byte because the honest numbers are
/// enormous in one direction and fractional in the other: a megabyte for a sat
/// is `1000000`, and the same price the other way round is 0.000001 and rounds
/// to nothing. Integers, and no rounding in the price itself.
///
/// Shared and atomic because it is a live setting. The configuration file sets
/// it at startup and the control socket sets it while the node runs, which is
/// the point — an operator raising the price of a scarce uplink should not have
/// to restart the thing selling it.
#[derive(Debug, Clone)]
pub struct Price(Arc<AtomicU64>);

impl Price {
    /// A price in bytes per sat. Zero closes the market.
    pub fn new(bytes_per_sat: u64) -> Self {
        Self(Arc::new(AtomicU64::new(bytes_per_sat)))
    }

    /// What a sat buys right now.
    pub fn bytes_per_sat(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }

    /// Change what a sat buys.
    ///
    /// Takes effect on the next quote. A quote already handed out keeps the
    /// price it was quoted at, which is what makes it a quote.
    pub fn set(&self, bytes_per_sat: u64) {
        self.0.store(bytes_per_sat, Ordering::Relaxed);
    }

    /// Whether this node is selling at all.
    pub fn is_selling(&self) -> bool {
        self.bytes_per_sat() > 0
    }

    /// What `bytes` cost, in millisats, at the price in force.
    ///
    /// Rounded up, and never free: a buyer asking for one byte at a megabyte
    /// per sat owes a millisat rather than nothing. Rounding down would let a
    /// buyer take capacity a byte at a time and pay for none of it.
    pub fn msat_for(&self, bytes: u64) -> Option<u64> {
        let per_sat = self.bytes_per_sat();
        if per_sat == 0 || bytes == 0 {
            return None;
        }
        let msat = (bytes as u128 * 1_000).div_ceil(per_sat as u128);
        Some(msat.clamp(1, u64::MAX as u128) as u64)
    }
}

/// What this node is selling, and at what price.
///
/// The signal the design cares about is this number against the quantity
/// printed on the voucher: what an issuer's paper sells for is a public
/// measure of how much the network expects that issuer to deliver.
#[derive(Debug, Serialize, Deserialize)]
pub struct Info {
    /// The unit its vouchers denominate in.
    pub unit: String,
    /// What one sat buys. Zero means this node is not selling here.
    pub bytes_per_sat: u64,
}

/// The market router, mounted alongside the mint.
pub fn router(unit: String, price: Price) -> Router {
    Router::new()
        .route(INFO_PATH, get(info))
        .with_state((unit, price))
}

async fn info(State((unit, price)): State<(String, Price)>) -> Json<Info> {
    Json(Info {
        unit,
        bytes_per_sat: price.bytes_per_sat(),
    })
}

/// A bolt11 backend that prices a byte-denominated mint in millisats.
///
/// The mint quotes in the unit it issues and asks for an invoice in that unit;
/// bolt11 only knows about money. This sits between them: bytes become
/// millisats on the way out, at the operator's price, and the payment that
/// comes back becomes bytes again — the quantity the quote was for, not a
/// figure reconverted at whatever the price has since become. A buyer who has
/// been quoted has been quoted.
#[derive(Debug)]
pub struct PricedBolt11 {
    price: Price,
    /// Settles the actual invoices. Denominated in millisats, because that is
    /// what an invoice is denominated in.
    lightning: FakeWallet,
    /// What each invoice was for, in the mint's unit, keyed by the identifier
    /// the backend knows the payment by.
    quoted: Arc<RwLock<HashMap<PaymentIdentifier, u64>>>,
    /// The unit the mint issues, so payments are reported back in it.
    unit: CurrencyUnit,
}

impl PricedBolt11 {
    /// A backend that sells `unit` at `price`.
    pub fn new(unit: CurrencyUnit, price: Price) -> Self {
        Self {
            price,
            lightning: FakeWallet::new(
                FeeReserve {
                    min_fee_reserve: 0.into(),
                    percent_fee_reserve: 0.0,
                },
                HashMap::default(),
                Default::default(),
                // No delay: the simulated hop exists to prove the mint waits
                // for payment, not to make it wait longer.
                0,
                CurrencyUnit::Msat,
            ),
            quoted: Arc::new(RwLock::new(HashMap::new())),
            unit,
        }
    }

    /// Report a payment in the unit the quote was denominated in.
    ///
    /// The backend reports millisats, because that is what was paid. The mint
    /// credits the quote in its own unit, so a payment for an invoice this
    /// backend wrote is worth exactly the quantity that invoice was written
    /// for — and one it did not write is worth nothing, rather than being
    /// silently mistaken for a quantity of bytes.
    async fn in_mint_units(&self, mut payment: WaitPaymentResponse) -> Option<WaitPaymentResponse> {
        let bytes = *self.quoted.read().await.get(&payment.payment_identifier)?;
        payment.payment_amount = Amount::new(bytes, self.unit.clone());
        Some(payment)
    }
}

#[async_trait]
impl MintPayment for PricedBolt11 {
    type Err = PaymentError;

    async fn start(&self) -> Result<(), Self::Err> {
        self.lightning.start().await
    }

    async fn stop(&self) -> Result<(), Self::Err> {
        self.lightning.stop().await
    }

    async fn get_settings(&self) -> Result<SettingsResponse, Self::Err> {
        self.lightning.get_settings().await
    }

    async fn create_incoming_payment_request(
        &self,
        options: IncomingPaymentOptions,
    ) -> Result<CreateIncomingPaymentResponse, Self::Err> {
        let IncomingPaymentOptions::Bolt11(mut bolt11) = options else {
            // Bolt12 offers and custom methods would each need their own answer
            // to the same pricing question, and nothing asks for one yet.
            return Err(PaymentError::UnsupportedPaymentOption);
        };

        let bytes = bolt11.amount.value();
        let Some(msat) = self.price.msat_for(bytes) else {
            warn!(
                bytes,
                bytes_per_sat = self.price.bytes_per_sat(),
                "refusing to quote"
            );
            return Err(PaymentError::Custom(if self.price.is_selling() {
                "a quote has to be for at least one unit".into()
            } else {
                "this node is not selling its vouchers".into()
            }));
        };

        bolt11.amount = Amount::new(msat, CurrencyUnit::Msat);
        let invoice = self
            .lightning
            .create_incoming_payment_request(IncomingPaymentOptions::Bolt11(bolt11))
            .await?;

        self.quoted
            .write()
            .await
            .insert(invoice.request_lookup_id.clone(), bytes);
        debug!(
            bytes,
            msat,
            bytes_per_sat = self.price.bytes_per_sat(),
            "quoted capacity"
        );
        Ok(invoice)
    }

    async fn get_payment_quote(
        &self,
        _unit: &CurrencyUnit,
        _options: OutgoingPaymentOptions,
    ) -> Result<PaymentQuoteResponse, Self::Err> {
        // Melting is the other direction — vouchers back into money — and it
        // needs an answer to a question this node has not been given: what it
        // will pay to buy its own paper back. Refusing is honest; guessing a
        // price would not be.
        Err(PaymentError::Custom(
            "this node does not redeem its vouchers for money".into(),
        ))
    }

    async fn make_payment(
        &self,
        _unit: &CurrencyUnit,
        _options: OutgoingPaymentOptions,
    ) -> Result<MakePaymentResponse, Self::Err> {
        Err(PaymentError::Custom(
            "this node does not redeem its vouchers for money".into(),
        ))
    }

    async fn wait_payment_event(
        &self,
    ) -> Result<Pin<Box<dyn futures::Stream<Item = Event> + Send>>, Self::Err> {
        let stream = self.lightning.wait_payment_event().await?;
        let quoted = Arc::clone(&self.quoted);
        let unit = self.unit.clone();

        // Same rewrite as `in_mint_units`, on the streaming path. Payments for
        // invoices this backend never wrote are dropped rather than passed
        // through with an amount in the wrong unit.
        Ok(Box::pin(stream.filter_map(move |event| {
            let quoted = Arc::clone(&quoted);
            let unit = unit.clone();
            async move {
                let Event::PaymentReceived(mut payment) = event;
                let bytes = *quoted.read().await.get(&payment.payment_identifier)?;
                payment.payment_amount = Amount::new(bytes, unit);
                Some(Event::PaymentReceived(payment))
            }
        })))
    }

    fn is_wait_invoice_active(&self) -> bool {
        self.lightning.is_wait_invoice_active()
    }

    fn cancel_wait_invoice(&self) {
        self.lightning.cancel_wait_invoice();
    }

    async fn check_incoming_payment_status(
        &self,
        payment_identifier: &PaymentIdentifier,
    ) -> Result<Vec<WaitPaymentResponse>, Self::Err> {
        let paid = self
            .lightning
            .check_incoming_payment_status(payment_identifier)
            .await?;

        let mut converted = Vec::with_capacity(paid.len());
        for payment in paid {
            if let Some(payment) = self.in_mint_units(payment).await {
                converted.push(payment);
            }
        }
        Ok(converted)
    }

    async fn check_outgoing_payment(
        &self,
        payment_identifier: &PaymentIdentifier,
    ) -> Result<MakePaymentResponse, Self::Err> {
        self.lightning
            .check_outgoing_payment(payment_identifier)
            .await
    }
}

// ---------------------------------------------------------------------------
// The buying side
// ---------------------------------------------------------------------------

/// Buy `amount` units of a mint's vouchers, and return them as a token.
///
/// Ordinary NUT-04, because selling a node's own vouchers needs nothing else:
/// ask for a quote, pay the invoice, hand over blinded outputs, unblind the
/// signatures. The seller prices the quote; this side only decides how much to
/// buy.
///
/// Paying is the one step that is stubbed. The node being bought from settles
/// its own invoices, so what happens here is a wait rather than a payment —
/// and it is where a wallet goes.
pub fn acquire(mint_url: &str, amount: u64, unit: &str, keyset_info: &str) -> Result<String> {
    let quote = quote(mint_url, amount, unit)?;
    debug!(amount, unit, quote = %quote.quote, "quoted for vouchers");
    wait_until_paid(mint_url, &quote)?;

    // Blinded after the quote is paid, so a purchase that never completes
    // leaves no secrets waiting for signatures that will not come.
    let blinded = cdk_spilman::create_plain_blinded_messages(amount, keyset_info)
        .map_err(|e| anyhow!("blind {amount} {unit}: {e}"))?;
    let blinded: serde_json::Value = serde_json::from_str(&blinded).context("blinded messages")?;
    let secrets = blinded["secrets_with_blinding"].to_string();

    let issued = http(
        "POST",
        &format!("{mint_url}/v1/mint/bolt11"),
        &serde_json::json!({
            "quote": quote.quote,
            "outputs": blinded["blinded_messages"],
        })
        .to_string(),
    )
    .map_err(|e| anyhow!("mint {amount} {unit} at {mint_url}: {e}"))?;

    let issued: serde_json::Value = serde_json::from_str(&issued).context("mint response")?;
    let signatures = issued["signatures"]
        .as_array()
        .ok_or_else(|| anyhow!("{mint_url} issued nothing for {amount} {unit}: {issued}"))?;

    let proofs =
        cdk_spilman::construct_proofs(&serde_json::to_string(signatures)?, &secrets, keyset_info)
            .map_err(|e| anyhow!("unblind {amount} {unit}: {e}"))?;

    cdk_spilman::build_cashu_b_token(mint_url, unit, &proofs)
        .map_err(|e| anyhow!("build a token: {e}"))
}

/// A mint quote, as much of it as the buyer needs.
#[derive(Debug, Deserialize)]
struct MintQuote {
    quote: String,
    #[allow(dead_code)]
    request: String,
}

/// Ask a mint what a quantity costs, and get an invoice for it.
fn quote(mint_url: &str, amount: u64, unit: &str) -> Result<MintQuote> {
    let body = http(
        "POST",
        &format!("{mint_url}/v1/mint/quote/bolt11"),
        &serde_json::json!({ "amount": amount, "unit": unit }).to_string(),
    )
    .map_err(|e| anyhow!("ask {mint_url} what {amount} {unit} cost: {e}"))?;

    serde_json::from_str(&body)
        .with_context(|| format!("{mint_url} would not quote {amount} {unit}: {body}"))
}

/// Wait until the seller agrees the quote is paid.
///
/// The mint will not sign anything until it has seen the money, so a buyer
/// that does not wait gets a refusal rather than vouchers.
fn wait_until_paid(mint_url: &str, quote: &MintQuote) -> Result<()> {
    let deadline = Instant::now() + PAYMENT_TIMEOUT;
    let url = format!("{mint_url}/v1/mint/quote/bolt11/{}", quote.quote);

    while Instant::now() < deadline {
        let body = http("GET", &url, "").map_err(|e| anyhow!("check a mint quote: {e}"))?;
        let status: serde_json::Value =
            serde_json::from_str(&body).with_context(|| format!("quote status: {body}"))?;

        // `state` is NUT-04's; `paid` is what older mints answered with.
        let paid = status["state"].as_str() == Some("PAID")
            || status["state"].as_str() == Some("ISSUED")
            || status["paid"].as_bool() == Some(true)
            || status["amount_paid"].as_u64().is_some_and(|a| a > 0);
        if paid {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    bail!(
        "{mint_url} did not see payment for quote {} within {}s",
        quote.quote,
        PAYMENT_TIMEOUT.as_secs()
    )
}

/// The id of a mint's active keyset for `unit`.
///
/// A purchase is signed by one keyset, and it has to be the active one: a mint
/// keeps older keysets readable so outstanding vouchers stay redeemable, but it
/// will not sign new ones against them.
pub fn active_keyset(mint_url: &str, unit: &str) -> Result<String> {
    let body = http("GET", &format!("{mint_url}/v1/keysets"), "")
        .map_err(|e| anyhow!("list keysets at {mint_url}: {e}"))?;
    let listing: serde_json::Value =
        serde_json::from_str(&body).with_context(|| format!("keyset listing from {mint_url}"))?;

    listing["keysets"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|k| k["unit"].as_str() == Some(unit) && k["active"].as_bool().unwrap_or(false))
        .and_then(|k| k["id"].as_str())
        .map(str::to_owned)
        .ok_or_else(|| anyhow!("{mint_url} has no active {unit} keyset"))
}

/// A blocking HTTP call, for the paths the synchronous channel backend drives.
///
/// The node runs every backend call on a blocking thread, so a blocking client
/// here does not stall the runtime.
fn http(method: &str, url: &str, body: &str) -> Result<String, String> {
    let client = reqwest::blocking::Client::new();
    let request = match method {
        "GET" => client.get(url),
        _ => client
            .post(url)
            .header("content-type", "application/json")
            .body(body.to_owned()),
    };
    let response: reqwest::blocking::Response = request.send().map_err(|e| e.to_string())?;
    response.text().map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_price_of_zero_sells_nothing() {
        let price = Price::new(0);
        assert!(!price.is_selling());
        assert_eq!(price.msat_for(1_000_000), None);
    }

    #[test]
    fn a_megabyte_a_sat_prices_a_megabyte_at_a_sat() {
        let price = Price::new(1_000_000);
        assert_eq!(price.msat_for(1_000_000), Some(1_000));
        assert_eq!(price.msat_for(1_000_000_000), Some(1_000_000));
    }

    #[test]
    fn a_fraction_of_a_millisat_still_costs_one() {
        // Rounding down would let a buyer take capacity a byte at a time and
        // pay for none of it.
        let price = Price::new(1_000_000);
        assert_eq!(price.msat_for(1), Some(1));
        assert_eq!(price.msat_for(1_001), Some(2), "rounded up, not to nearest");
        assert_eq!(price.msat_for(0), None, "nothing is not a purchase");
    }

    #[test]
    fn the_price_can_change_while_the_node_runs() {
        let price = Price::new(1_000_000);
        let elsewhere = price.clone();
        elsewhere.set(2_000_000);
        assert_eq!(price.bytes_per_sat(), 2_000_000);
        assert_eq!(price.msat_for(1_000_000), Some(500), "half the price");
    }

    #[tokio::test]
    async fn a_quote_is_invoiced_in_money_and_credited_in_capacity() {
        let backend = PricedBolt11::new(CurrencyUnit::Custom("byte".into()), Price::new(1_000_000));

        // The mint asks for an invoice denominated in what it issues.
        let invoice = backend
            .create_incoming_payment_request(IncomingPaymentOptions::Bolt11(
                cdk_common::payment::Bolt11IncomingPaymentOptions {
                    description: None,
                    amount: Amount::new(2_000_000, CurrencyUnit::Custom("byte".into())),
                    unix_expiry: None,
                },
            ))
            .await
            .expect("quote two megabytes");

        // The simulated backend pays its own invoices immediately.
        let mut paid = Vec::new();
        for _ in 0..50 {
            paid = backend
                .check_incoming_payment_status(&invoice.request_lookup_id)
                .await
                .expect("check payment");
            if !paid.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        let payment = paid.first().expect("the invoice should have been paid");
        assert_eq!(
            payment.payment_amount.value(),
            2_000_000,
            "the mint credits the quote in bytes, not in the millisats that paid for it"
        );
        assert_eq!(
            payment.payment_amount.unit(),
            &CurrencyUnit::Custom("byte".into())
        );
    }

    #[tokio::test]
    async fn a_closed_market_will_not_quote() {
        let backend = PricedBolt11::new(CurrencyUnit::Custom("byte".into()), Price::new(0));
        let refused = backend
            .create_incoming_payment_request(IncomingPaymentOptions::Bolt11(
                cdk_common::payment::Bolt11IncomingPaymentOptions {
                    description: None,
                    amount: Amount::new(1_000, CurrencyUnit::Custom("byte".into())),
                    unix_expiry: None,
                },
            ))
            .await;
        assert!(refused.is_err(), "a price of zero is not a price");
    }
}
