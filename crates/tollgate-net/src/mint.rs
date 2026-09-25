//! This node's own mint.
//!
//! Every node issues vouchers against its own capacity and redeems them on
//! delivery. Redemption is a local spent-proof check — the node is the
//! authority on its own paper — which is why payment liveness and service
//! liveness fail together rather than separately.
//!
//! The keyset unit is the **byte**, not the sat. That is one of the three
//! things the voucher model changes, and it is what makes a proof a claim on
//! one unit of capacity rather than on money. A `1024` proof is a 1 KiB claim;
//! two of them make 2 KiB; they split and combine like any other Cashu token.
//!
//! # Minting for the asking
//!
//! Until there is a market, the way to hold this node's vouchers is to mint
//! them here, and the mint gives them away: a NUT-04 mint quote in the node's
//! unit is reported paid the first time the mint checks it, so a buyer asks for
//! what it needs and mints it straight away. There is no Lightning behind it and
//! nothing to pay. Bolt11 is denominated in msat and this keyset in bytes, so a
//! real invoice would have to invent an exchange rate; the quote keeps the
//! standard shape so that an ordinary Cashu wallet can drive it, and nothing
//! else about it is Lightning.
//!
//! That makes service free to anyone who asks, which is the point for now
//! rather than an oversight: what is being exercised is delivery, admission and
//! the channels, not pricing. [`MintConfig::auto_accept`] turns it off, and the
//! mint then issues nothing through NUT-04 at all. Free is not unlimited:
//! [`IssueLimit`] rations how fast the mint gives vouchers away, so the quote
//! endpoint cannot be used to make the node sign and store without end.
//!
//! Everything the mint itself does is real: the keysets, the blind signatures,
//! the DLEQ proofs and the spent-proof set.

use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use axum::Router;
use cdk::mint::{Mint, MintBuilder, MintMeltLimits};
use cdk::nuts::nut00::KnownMethod;
use cdk::nuts::{CurrencyUnit, PaymentMethod};
use cdk_common::Amount;
use cdk_common::common::QuoteTTL;
use cdk_common::payment::{
    self, Bolt11Settings, CreateIncomingPaymentResponse, Event, IncomingPaymentOptions,
    MakePaymentResponse, MintPayment, OutgoingPaymentOptions, PaymentIdentifier,
    PaymentQuoteResponse, SettingsResponse, WaitPaymentResponse,
};
use futures::Stream;

/// How this node's mint is set up.
#[derive(Debug, Clone)]
pub struct MintConfig {
    /// URL peers reach it on, as advertised in our Offer.
    pub url: String,
    /// Quantity unit. `"byte"` for network forwarding.
    pub unit: String,
    /// Seed for the keyset, so a restart keeps issuing against the same keys.
    pub seed: Vec<u8>,
    /// Largest amount a single issuance may create.
    ///
    /// The ceiling on one mint quote, and on one market swap. A buyer that
    /// needs more asks for several quotes, so this bounds the size of a request
    /// rather than how much anybody may hold.
    pub max_amount: u64,
    /// Whether a NUT-04 mint quote is paid the moment it is asked for.
    ///
    /// On, anyone who can reach the mint can mint as much as they like. Off,
    /// the mint serves no mint quotes at all, and its vouchers have to be had
    /// some other way.
    pub auto_accept: bool,
    /// How fast an auto-accepting mint gives its vouchers away.
    pub issue_limit: IssueLimit,
}

/// How fast an auto-accepting mint issues, across everybody who asks.
///
/// Free vouchers are the point while there is no market, but a free quote
/// endpoint is also a way to make the node sign and store without end. This
/// caps the work, not anybody's share: the mint cannot tell one asker from
/// another, so the limit is node-wide, charged when a quote is created, and a
/// quote over it is refused.
///
/// A rate of zero switches that limit off.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IssueLimit {
    /// Bytes of vouchers issued per second, on average.
    pub bytes_per_sec: u64,
    /// Bytes that may be issued at once before the rate applies. Never less
    /// than [`MintConfig::max_amount`], so one quote of the largest size the
    /// mint allows can always be had.
    pub burst_bytes: u64,
    /// Mint quotes created per minute, with a minute's worth as the burst.
    pub quotes_per_minute: u64,
}

impl Default for IssueLimit {
    /// Sized so that no peer funding channels in earnest ever waits on it.
    ///
    /// - 125 MB/s is 1 Gbit/s. Issuing vouchers faster than the node could
    ///   deliver the bytes they claim buys nobody anything, and no router this
    ///   ships on forwards more than that.
    /// - A 4 GB burst is four channels of the default 1 GB opened at once, as
    ///   when several peers arrive together; the rate refills one in 8 s.
    /// - 60 quotes a minute: a buyer asks for one quote per channel it opens
    ///   (more only for a channel larger than one quote allows), so one a
    ///   second is far above use, and holds a flood of quotes to a trickle of
    ///   stored rows and signatures.
    fn default() -> Self {
        Self {
            bytes_per_sec: 125_000_000,
            burst_bytes: 4_000_000_000,
            quotes_per_minute: 60,
        }
    }
}

/// The unit a keyset denominates in.
///
/// The resource fixes the unit, and every node selling the same resource
/// denominates the same way — that is what makes one issuer's vouchers
/// comparable to another's. `"sat"` is still spelled the Cashu way rather than
/// as a custom unit, so a sat-denominated node stays interoperable with
/// ordinary wallets.
pub fn currency_unit(unit: &str) -> CurrencyUnit {
    match unit {
        "sat" => CurrencyUnit::Sat,
        "msat" => CurrencyUnit::Msat,
        other => CurrencyUnit::Custom(other.into()),
    }
}

/// Build and start this node's mint.
pub async fn build(config: &MintConfig) -> Result<Mint> {
    let unit = currency_unit(&config.unit);
    let db = Arc::new(
        cdk_sqlite::mint::memory::empty()
            .await
            .context("open the mint database")?,
    );

    let mut builder = MintBuilder::new(db.clone())
        .with_name(format!("TollGate node at {}", config.url))
        .with_description("Vouchers: claims on this node's capacity".to_string())
        .with_urls(vec![config.url.clone()]);

    builder
        .configure_unit(unit.clone(), Default::default())
        .map_err(|e| anyhow!("configure the {unit} keyset: {e}"))?;

    // No input fee. A fee would mean a voucher redeemed is worth slightly less
    // than a voucher issued, and delivery is one voucher per unit exactly.
    builder
        .set_unit_fee(&unit, 0)
        .map_err(|e| anyhow!("set the input fee: {e}"))?;

    // A payment processor is how a mint issues paper against something else.
    // Without one there is no mint quote to ask for, which is what switching
    // auto-accept off means.
    if config.auto_accept {
        builder
            .add_payment_processor(
                unit.clone(),
                PaymentMethod::Known(KnownMethod::Bolt11),
                MintMeltLimits::new(1, config.max_amount.max(1)),
                Arc::new(AutoAccept::new(
                    unit.clone(),
                    config.issue_limit,
                    config.max_amount,
                    Instant::now(),
                )),
            )
            .await
            .map_err(|e| anyhow!("accept mint quotes in {unit}: {e}"))?;

        // Adding a Bolt11 processor advertises melting too, and there is
        // nothing to melt into: redeeming a voucher is being served, not being
        // paid out. Advertising it would only send wallets to an endpoint that
        // refuses them.
        let mut info = builder.current_mint_info();
        info.nuts.nut05.methods.clear();
        info.nuts.nut05.disabled = true;
        builder = builder.with_mint_info(info);
    }

    let mint = builder
        .build_with_seed(db.clone(), &config.seed)
        .await
        .context("build the mint")?;

    mint.set_quote_ttl(QuoteTTL::new(10_000, 10_000))
        .await
        .context("set quote TTLs")?;

    if !mint.get_active_keysets().contains_key(&unit) {
        return Err(anyhow!(
            "the mint came up with no active keyset for {unit}; the unit is probably not supported"
        ));
    }

    mint.start().await.context("start the mint")?;
    Ok(mint)
}

/// A payment processor for which every mint quote is already paid.
///
/// The whole of free minting. Nothing is received, so there is nothing to wait
/// for: the mint asks whether a quote is paid when a wallet checks it or mints
/// against it, and the answer is always the full amount. Melting is refused,
/// because nothing this mint issues can be paid out as money.
///
/// Quotes are rationed by [`IssueLimit`], checked when a quote is asked for:
/// that is where the work starts, and a quote refused there never reaches the
/// database.
#[derive(Debug)]
struct AutoAccept {
    unit: CurrencyUnit,
    limits: Mutex<Limits>,
}

impl AutoAccept {
    fn new(unit: CurrencyUnit, limit: IssueLimit, max_amount: u64, now: Instant) -> Self {
        Self {
            unit,
            limits: Mutex::new(Limits::new(limit, max_amount, now)),
        }
    }
}

/// The two buckets an [`IssueLimit`] fills, either of which may be off.
#[derive(Debug)]
struct Limits {
    bytes: Option<Bucket>,
    quotes: Option<Bucket>,
}

impl Limits {
    fn new(limit: IssueLimit, max_amount: u64, now: Instant) -> Self {
        Self {
            bytes: (limit.bytes_per_sec > 0).then(|| {
                Bucket::new(
                    limit.bytes_per_sec,
                    Duration::from_secs(1),
                    limit.burst_bytes.max(max_amount),
                    now,
                )
            }),
            quotes: (limit.quotes_per_minute > 0).then(|| {
                Bucket::new(
                    limit.quotes_per_minute,
                    Duration::from_secs(60),
                    limit.quotes_per_minute,
                    now,
                )
            }),
        }
    }

    /// Take one quote for `amount`, or take nothing and say which ran out.
    fn admit(&mut self, now: Instant, amount: u64) -> Result<(), &'static str> {
        for bucket in [&mut self.bytes, &mut self.quotes].into_iter().flatten() {
            bucket.refill(now);
        }
        if self.bytes.as_ref().is_some_and(|b| !b.holds(amount)) {
            return Err("bytes issued");
        }
        if self.quotes.as_ref().is_some_and(|b| !b.holds(1)) {
            return Err("quotes asked for");
        }
        if let Some(b) = &mut self.bytes {
            b.take(amount);
        }
        if let Some(b) = &mut self.quotes {
            b.take(1);
        }
        Ok(())
    }
}

/// A token bucket, starting full.
///
/// The level is kept in tokens times nanoseconds of the refill period, so a
/// refill after any interval, however short, adds exactly what it should and
/// nothing is lost to rounding between frequent calls.
#[derive(Debug)]
struct Bucket {
    per_period: u128,
    period_ns: u128,
    capacity: u128,
    level: u128,
    last: Instant,
}

impl Bucket {
    fn new(per_period: u64, period: Duration, burst: u64, now: Instant) -> Self {
        let period_ns = period.as_nanos().max(1);
        let capacity = u128::from(burst).saturating_mul(period_ns);
        Self {
            per_period: u128::from(per_period),
            period_ns,
            capacity,
            level: capacity,
            last: now,
        }
    }

    fn refill(&mut self, now: Instant) {
        let elapsed = now.saturating_duration_since(self.last).as_nanos();
        self.level = self
            .level
            .saturating_add(elapsed.saturating_mul(self.per_period))
            .min(self.capacity);
        self.last = self.last.max(now);
    }

    fn holds(&self, tokens: u64) -> bool {
        self.level >= u128::from(tokens).saturating_mul(self.period_ns)
    }

    fn take(&mut self, tokens: u64) {
        self.level = self
            .level
            .saturating_sub(u128::from(tokens).saturating_mul(self.period_ns));
    }
}

/// Where a quote's amount is kept: in its lookup id.
///
/// The mint stores the id with the quote and hands it back when it asks whether
/// the quote is paid, so carrying the amount in it leaves this processor with
/// no state of its own that grows with every quote asked for.
///
/// The id doubles as the payment's id, which the mint keeps unique across
/// every quote it has recorded. So the nonce is random rather than counted
/// from startup: once the mint's database outlives a restart, a counter would
/// hand a fresh quote an id already on record, and that quote would never be
/// paid.
fn lookup_id(nonce: u128, amount: u64) -> PaymentIdentifier {
    PaymentIdentifier::CustomId(format!("auto-{nonce:032x}-{amount}"))
}

fn amount_of(id: &PaymentIdentifier) -> Option<u64> {
    match id {
        PaymentIdentifier::CustomId(s) => s.strip_prefix("auto-")?.split_once('-')?.1.parse().ok(),
        _ => None,
    }
}

#[async_trait]
impl MintPayment for AutoAccept {
    type Err = payment::Error;

    async fn get_settings(&self) -> Result<SettingsResponse, Self::Err> {
        Ok(SettingsResponse {
            unit: self.unit.to_string(),
            bolt11: Some(Bolt11Settings {
                mpp: false,
                amountless: false,
                invoice_description: false,
            }),
            bolt12: None,
            onchain: None,
            custom: Default::default(),
        })
    }

    async fn create_incoming_payment_request(
        &self,
        options: IncomingPaymentOptions,
    ) -> Result<CreateIncomingPaymentResponse, Self::Err> {
        let IncomingPaymentOptions::Bolt11(options) = options else {
            return Err(payment::Error::UnsupportedPaymentOption);
        };
        if options.amount.unit() != &self.unit {
            return Err(payment::Error::UnsupportedUnit);
        }

        let amount = options.amount.value();
        if let Err(exhausted) = self
            .limits
            .lock()
            .expect("not poisoned")
            .admit(Instant::now(), amount)
        {
            tracing::debug!(
                amount,
                exhausted,
                "refused a mint quote over the issue limit"
            );
            return Err(payment::Error::Custom(format!(
                "this mint is issuing too fast: the limit on {exhausted} is reached, try again shortly"
            )));
        }

        let id = lookup_id(secp256k1::rand::random(), amount);
        Ok(CreateIncomingPaymentResponse {
            // Not an invoice, and not pretending to be one: there is nothing
            // to pay. It says what happened for anyone who reads it.
            request: format!("auto-accepted:{id}"),
            request_lookup_id: id,
            expiry: options.unix_expiry,
            extra_json: None,
        })
    }

    async fn check_incoming_payment_status(
        &self,
        payment_identifier: &PaymentIdentifier,
    ) -> Result<Vec<WaitPaymentResponse>, Self::Err> {
        // Paid in full, once. The mint records a payment by its id and ignores
        // one it has already seen, so answering the same way on every check
        // never pays a quote twice.
        let Some(amount) = amount_of(payment_identifier) else {
            return Ok(Vec::new());
        };
        Ok(vec![WaitPaymentResponse {
            payment_identifier: payment_identifier.clone(),
            payment_amount: Amount::new(amount, self.unit.clone()),
            payment_id: payment_identifier.to_string(),
        }])
    }

    async fn wait_payment_event(
        &self,
    ) -> Result<Pin<Box<dyn Stream<Item = Event> + Send>>, Self::Err> {
        // Nothing ever arrives: a quote is paid by being asked about.
        Ok(Box::pin(futures::stream::pending()))
    }

    fn is_payment_event_stream_active(&self) -> bool {
        false
    }

    fn cancel_payment_event_stream(&self) {}

    async fn get_payment_quote(
        &self,
        _unit: &CurrencyUnit,
        _options: OutgoingPaymentOptions,
    ) -> Result<PaymentQuoteResponse, Self::Err> {
        Err(payment::Error::UnsupportedPaymentOption)
    }

    async fn make_payment(
        &self,
        _unit: &CurrencyUnit,
        _options: OutgoingPaymentOptions,
    ) -> Result<MakePaymentResponse, Self::Err> {
        Err(payment::Error::UnsupportedPaymentOption)
    }

    async fn check_outgoing_payment(
        &self,
        _payment_identifier: &PaymentIdentifier,
    ) -> Result<MakePaymentResponse, Self::Err> {
        Err(payment::Error::UnsupportedPaymentOption)
    }
}

/// Serve the mint over HTTP until `shutdown` resolves.
///
/// A peer funds its channel against **our** mint, so this has to be reachable
/// by peers — which it always is, because it is the node they are already
/// talking to.
pub async fn serve(
    mint: Arc<Mint>,
    market: Router,
    listen: std::net::SocketAddr,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> Result<()> {
    // The market rides on the same listener as the mint under its own path.
    // It is a separate protocol that happens to be served by the same process;
    // a node may point its peers at somebody else's market entirely. All that
    // is served here is the price signal — selling this node's own vouchers is
    // the mint API beside it, and needs nothing of its own.
    //
    // cdk serves the NUT-04 routes only for the methods it is told about, so
    // they are read back from what the mint advertises: a mint that
    // auto-accepts gets its quote endpoints, and one that does not has none.
    let info = mint.mint_info().await.context("read the mint's info")?;
    let mut methods: Vec<String> = info
        .nuts
        .nut04
        .methods
        .iter()
        .map(|m| m.method.to_string())
        .collect();
    methods.sort();
    methods.dedup();
    let router = cdk_axum::create_mint_router(Arc::clone(&mint), methods)
        .await
        .context("build the mint router")?
        .merge(market);

    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .with_context(|| format!("bind the mint on {listen}"))?;

    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown)
        .await
        .context("serve the mint")?;

    mint.stop().await.context("stop the mint")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_network_unit_is_a_custom_keyset_unit() {
        // Cashu has no byte unit of its own; it is the resource that fixes it.
        assert_eq!(currency_unit("byte"), CurrencyUnit::Custom("byte".into()));
    }

    #[test]
    fn sat_stays_the_cashu_sat() {
        // Otherwise a sat-denominated node would be issuing something ordinary
        // wallets could not read.
        assert_eq!(currency_unit("sat"), CurrencyUnit::Sat);
    }

    #[test]
    fn a_quote_carries_its_amount_in_its_lookup_id() {
        // What makes the processor stateless: whatever the mint hands back is
        // enough to say how much was paid.
        assert_eq!(amount_of(&lookup_id(3, 1_048_576)), Some(1_048_576));
        assert_eq!(amount_of(&lookup_id(u128::MAX, u64::MAX)), Some(u64::MAX));
        assert_ne!(lookup_id(0, 10), lookup_id(1, 10), "ids are unique");

        // Anything it did not make is not paid.
        assert_eq!(amount_of(&PaymentIdentifier::CustomId("x-1".into())), None);
        assert_eq!(amount_of(&PaymentIdentifier::PaymentHash([0; 32])), None);
    }

    fn limit(bytes_per_sec: u64, burst_bytes: u64, quotes_per_minute: u64) -> IssueLimit {
        IssueLimit {
            bytes_per_sec,
            burst_bytes,
            quotes_per_minute,
        }
    }

    #[test]
    fn a_burst_is_issued_at_once_and_the_rest_is_refused() {
        let t0 = Instant::now();
        let mut limits = Limits::new(limit(1_000, 10_000, 0), 1, t0);

        assert_eq!(limits.admit(t0, 6_000), Ok(()));
        assert_eq!(limits.admit(t0, 4_000), Ok(()), "the whole burst");
        assert_eq!(
            limits.admit(t0, 1),
            Err("bytes issued"),
            "and not a byte more"
        );
    }

    #[test]
    fn the_bucket_refills_at_the_rate_and_no_further_than_the_burst() {
        let t0 = Instant::now();
        let mut limits = Limits::new(limit(1_000, 10_000, 0), 1, t0);
        limits.admit(t0, 10_000).expect("empty it");

        // Half a second is 500 bytes, arriving however finely it is sliced.
        let mut now = t0;
        for _ in 0..500 {
            now += Duration::from_millis(1);
            limits.bytes.as_mut().expect("on").refill(now);
        }
        assert_eq!(limits.admit(now, 501), Err("bytes issued"));
        assert_eq!(limits.admit(now, 500), Ok(()));

        // An hour idle fills it to the burst, not beyond.
        let later = now + Duration::from_secs(3_600);
        assert_eq!(limits.admit(later, 10_001), Err("bytes issued"));
        assert_eq!(limits.admit(later, 10_000), Ok(()));
    }

    #[test]
    fn a_refused_quote_takes_nothing() {
        let t0 = Instant::now();
        let mut limits = Limits::new(limit(1_000, 10_000, 2), 1, t0);

        // Too large for the bytes: the quote allowance is left alone.
        assert_eq!(limits.admit(t0, 20_000), Err("bytes issued"));
        assert_eq!(limits.admit(t0, 1), Ok(()));
        assert_eq!(limits.admit(t0, 1), Ok(()));

        // Out of quotes: the bytes are left alone.
        assert_eq!(limits.admit(t0, 1), Err("quotes asked for"));
        let bytes = limits.bytes.as_ref().expect("on");
        assert!(bytes.holds(9_998) && !bytes.holds(9_999));
    }

    #[test]
    fn the_largest_quote_the_mint_allows_always_fits_the_burst() {
        // A burst smaller than one full-size quote would refuse that quote
        // forever, however long the buyer waited.
        let t0 = Instant::now();
        let mut limits = Limits::new(limit(1, 10, 0), 1_000_000, t0);
        assert_eq!(limits.admit(t0, 1_000_000), Ok(()));
    }

    #[test]
    fn a_zero_rate_switches_that_limit_off() {
        let t0 = Instant::now();
        let mut limits = Limits::new(limit(0, 0, 0), 1, t0);
        for _ in 0..1_000 {
            assert_eq!(limits.admit(t0, u64::MAX), Ok(()));
        }
    }

    #[tokio::test]
    async fn a_byte_denominated_mint_comes_up_with_an_active_keyset() {
        let mint = build(&MintConfig {
            url: "http://127.0.0.1:0".into(),
            unit: "byte".into(),
            seed: vec![7; 32],
            max_amount: 1_000_000_000,
            auto_accept: false,
            issue_limit: IssueLimit::default(),
        })
        .await
        .expect("build a byte mint");

        assert!(
            mint.get_active_keysets()
                .contains_key(&CurrencyUnit::Custom("byte".into())),
            "the byte keyset should be active"
        );
    }
}
