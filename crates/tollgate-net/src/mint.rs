//! This node's own mint, as `mintd` runs it.
//!
//! A node that sells its capacity issues vouchers against it and redeems them
//! on delivery. Redemption is a spent-proof check at the node's own mint — the
//! node is the authority on its own paper — which is why payment liveness and
//! service liveness fail together rather than separately. The mint runs as its
//! own daemon, `mintd`, so `tollgated` holds none of its keys and settles at it
//! like any other Cashu client (`docs/design/core/tollgate-daemons.md`).
//!
//! The keyset unit is the **byte**, not the sat. That is one of the three
//! things the voucher model changes, and it is what makes a proof a claim on
//! one unit of capacity rather than on money. A `1024` proof is a 1 KiB claim;
//! two of them make 2 KiB; they split and combine like any other Cashu token.
//!
//! # One API, two listeners
//!
//! The mint serves the standard Cashu API and nothing else, twice ([`serve`]):
//!
//! - **Private**, for `merchantd` alone: a NUT-04 mint quote in the node's
//!   unit is reported paid the first time the mint checks it. That is how the
//!   node sells — `merchantd` takes the money, then mints what it sold here.
//! - **Public**, for everyone: the same API without mint quotes, unless
//!   [`Listeners::auto_accept`] is on, in which case the public listener hands
//!   them out too and service is free to anyone who asks. Auto-accept exists
//!   for testing and for giving free vouchers to third parties; it is free
//!   without a cap on how much, and [`IssueLimit`] rations only how many quotes
//!   the public listener serves, so it cannot be used to make the mint sign and
//!   store without end.
//!
//! What is privileged is the address, not the call. There is no Lightning
//! behind a quote and nothing to pay. Bolt11 is denominated in msat and this
//! keyset in bytes, so a real invoice would have to invent an exchange rate;
//! the quote keeps the standard shape so that an ordinary Cashu wallet can
//! drive it, and nothing else about it is Lightning.
//!
//! Everything the mint itself does is real: the keysets, the blind signatures,
//! the DLEQ proofs and the spent-proof set.
//!
//! # The spent-proof set is kept on disk
//!
//! The keyset is derived from the mint's own seed, so it comes back unchanged
//! after a restart and every voucher a peer holds stays redeemable. The record
//! of which vouchers have already been redeemed has to survive the same
//! restart, or the paper outlives the memory of having been paid out and a
//! peer can spend the same voucher twice. So the database is a file.
//!
//! The mint quotes it issues live in the same file, so their ids have to stay
//! unique across restarts too; see `lookup_id`.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use axum::Router;
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
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
    /// Where the mint database lives: the keysets and the spent-proof set.
    ///
    /// It has to outlive the process for the same reason the seed has to be
    /// deterministic — see the module documentation.
    pub file: PathBuf,
    /// Largest amount a single issuance may create.
    ///
    /// The ceiling on one mint quote. A buyer that needs more asks for several
    /// quotes, so this bounds the size of a request rather than how much
    /// anybody may hold.
    pub max_amount: u64,
}

/// Where the mint is served, and who may have a quote paid.
#[derive(Debug, Clone)]
pub struct Listeners {
    /// The mint as peers and wallets see it.
    pub public: SocketAddr,
    /// Mint quotes here are paid on creation. Whoever reaches it can print
    /// this node's vouchers, so it belongs on loopback, for `merchantd` alone.
    pub private: SocketAddr,
    /// Whether the public listener pays quotes on creation too.
    ///
    /// On, anyone who can reach the mint can mint as much as they like. Off,
    /// the public listener serves no mint quotes at all.
    pub auto_accept: bool,
    /// How many quotes the public listener serves.
    pub issue_limit: IssueLimit,
}

/// What the mint has been asked for since it started, for `minttop`.
#[derive(Debug, Default)]
pub struct Stats {
    /// Mint quotes served on the public listener.
    pub public_quotes: AtomicU64,
    /// Mint quotes served on the private listener.
    pub private_quotes: AtomicU64,
    /// Public quotes refused for being over the [`IssueLimit`].
    pub refused_quotes: AtomicU64,
}

/// How many quotes the public listener serves, across everybody who asks.
///
/// Auto-accept is free or it is off: what a quote may be for is not capped,
/// only how often one may be asked for. A free quote endpoint is otherwise a
/// way to make the node sign and store without end. This caps the work, not
/// anybody's share: the mint cannot tell one asker from another, so the limit
/// is node-wide, charged when a quote is created, and a quote over it is
/// refused.
///
/// A rate of zero switches the limit off.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IssueLimit {
    /// Mint quotes created per minute, with a minute's worth as the burst.
    pub quotes_per_minute: u64,
}

impl Default for IssueLimit {
    /// 60 quotes a minute: a buyer asks for one quote per channel it opens
    /// (more only for a channel larger than one quote allows), so one a second
    /// is far above use, and holds a flood of quotes to a trickle of stored
    /// rows and signatures.
    fn default() -> Self {
        Self {
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

/// Where a node keeps its mint database unless told otherwise.
///
/// In the state directory, so the packages keep it across an upgrade.
pub fn default_path() -> PathBuf {
    crate::config::state_file("mint.sqlite")
}

/// Where the mint keeps its seed unless told otherwise.
pub fn default_seed_path() -> PathBuf {
    crate::config::state_file("mint.seed")
}

/// Read the mint's seed, creating it on first start.
///
/// The mint's own secret, not derived from the node's identity: `tollgated`
/// never needs it, and either key can be rotated or stored apart from the
/// other. Losing it retires every voucher outstanding, since the keyset cannot
/// be rebuilt without it.
pub fn load_or_create_seed(path: &std::path::Path) -> Result<Vec<u8>> {
    if let Ok(text) = std::fs::read_to_string(path) {
        return hex::decode(text.trim())
            .with_context(|| format!("the mint seed at {} is not hex", path.display()));
    }
    if let Some(dir) = path.parent()
        && !dir.as_os_str().is_empty()
    {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    }
    let seed: [u8; 32] = secp256k1::rand::random();
    write_secret(path, &hex::encode(seed))
        .with_context(|| format!("write a new mint seed to {}", path.display()))?;
    tracing::info!(path = %path.display(), "created a new mint seed");
    Ok(seed.to_vec())
}

#[cfg(unix)]
fn write_secret(path: &std::path::Path, contents: &str) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(contents.as_bytes())
}

#[cfg(not(unix))]
fn write_secret(path: &std::path::Path, contents: &str) -> std::io::Result<()> {
    std::fs::write(path, contents)
}

/// Build and start this node's mint.
pub async fn build(config: &MintConfig) -> Result<Mint> {
    let unit = currency_unit(&config.unit);
    let path = &config.file;
    if let Some(dir) = path.parent()
        && !dir.as_os_str().is_empty()
    {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    }
    let db = Arc::new(
        cdk_sqlite::mint::MintSqliteDatabase::new(path.clone())
            .await
            .with_context(|| format!("open the mint database at {}", path.display()))?,
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
    // This one reports every quote paid; which listener serves quotes at all
    // is what decides who may have one (see `serve`).
    builder
        .add_payment_processor(
            unit.clone(),
            PaymentMethod::Known(KnownMethod::Bolt11),
            MintMeltLimits::new(1, config.max_amount.max(1)),
            Arc::new(PaidOnCreation { unit: unit.clone() }),
        )
        .await
        .map_err(|e| anyhow!("accept mint quotes in {unit}: {e}"))?;

    // Adding a Bolt11 processor advertises melting too, and there is nothing
    // to melt into: redeeming a voucher is being served, not being paid out.
    // Advertising it would only send wallets to an endpoint that refuses them.
    let mut info = builder.current_mint_info();
    info.nuts.nut05.methods.clear();
    info.nuts.nut05.disabled = true;
    builder = builder.with_mint_info(info);

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
/// Nothing is received, so there is nothing to wait for: the mint asks whether
/// a quote is paid when a wallet checks it or mints against it, and the answer
/// is always the full amount. Melting is refused, because nothing this mint
/// issues can be paid out as money. Who may ask for a quote at all is the
/// listener's business, not this one's.
#[derive(Debug)]
struct PaidOnCreation {
    unit: CurrencyUnit,
}

/// The bucket an [`IssueLimit`] fills, if it is on.
#[derive(Debug)]
struct Limits {
    quotes: Option<Bucket>,
}

impl Limits {
    fn new(limit: IssueLimit, now: Instant) -> Self {
        Self {
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

    /// Take one quote, or take nothing and refuse.
    fn admit(&mut self, now: Instant) -> Result<(), &'static str> {
        let Some(quotes) = &mut self.quotes else {
            return Ok(());
        };
        quotes.refill(now);
        if !quotes.holds(1) {
            return Err("quotes asked for");
        }
        quotes.take(1);
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
/// from startup: the mint's database outlives a restart, so a counter would
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
impl MintPayment for PaidOnCreation {
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

/// Serve the mint on both listeners until `shutdown` resolves.
///
/// cdk serves the NUT-04 routes only for the methods it is told about. The
/// private listener is told about the quote method, so `merchantd` can have
/// quotes paid there; the public one only when auto-accept is on, and then
/// behind the [`IssueLimit`]. Everything else — keys, swap, state check — is
/// the same on both.
pub async fn serve(
    mint: Arc<Mint>,
    listeners: Listeners,
    stats: Arc<Stats>,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> Result<()> {
    let (public, private) = routers(
        Arc::clone(&mint),
        listeners.auto_accept,
        listeners.issue_limit,
        stats,
    )
    .await?;

    let bind = |addr: SocketAddr| async move {
        tokio::net::TcpListener::bind(addr)
            .await
            .with_context(|| format!("bind the mint on {addr}"))
    };
    let private_listener = bind(listeners.private).await?;
    let public_listener = bind(listeners.public).await?;

    // One shutdown for both: the private listener stops with the public one.
    let (stop, stopped) = tokio::sync::watch::channel(false);
    let mut private_stopped = stopped.clone();
    let private = tokio::spawn(async move {
        axum::serve(private_listener, private)
            .with_graceful_shutdown(async move {
                let _ = private_stopped.wait_for(|s| *s).await;
            })
            .await
    });
    axum::serve(public_listener, public)
        .with_graceful_shutdown(async move {
            shutdown.await;
            let _ = stop.send(true);
        })
        .await
        .context("serve the public mint")?;
    private
        .await
        .context("join the private mint")?
        .context("serve the private mint")?;

    mint.stop().await.context("stop the mint")?;
    Ok(())
}

/// The public and private routers, in that order.
async fn routers(
    mint: Arc<Mint>,
    auto_accept: bool,
    issue_limit: IssueLimit,
    stats: Arc<Stats>,
) -> Result<(Router, Router)> {
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

    let private = cdk_axum::create_mint_router(Arc::clone(&mint), methods.clone())
        .await
        .context("build the private mint router")?
        .layer(axum::middleware::from_fn_with_state(
            Counting {
                stats: Arc::clone(&stats),
                limits: None,
                private: true,
            },
            count_quotes,
        ));

    let public_methods = if auto_accept { methods } else { Vec::new() };
    let public = cdk_axum::create_mint_router(Arc::clone(&mint), public_methods)
        .await
        .context("build the public mint router")?
        .layer(axum::middleware::from_fn_with_state(
            Counting {
                stats,
                limits: Some(Arc::new(Mutex::new(Limits::new(
                    issue_limit,
                    Instant::now(),
                )))),
                private: false,
            },
            count_quotes,
        ));
    Ok((public, private))
}

/// What a listener counts, and whether it rations quotes.
#[derive(Clone)]
struct Counting {
    stats: Arc<Stats>,
    limits: Option<Arc<Mutex<Limits>>>,
    private: bool,
}

/// Count every mint quote asked for, and refuse one over the limit.
///
/// Checked when a quote is asked for: that is where the work starts, and a
/// quote refused here never reaches the database.
async fn count_quotes(State(counting): State<Counting>, req: Request, next: Next) -> Response {
    let is_quote =
        req.method() == axum::http::Method::POST && req.uri().path().starts_with("/v1/mint/quote/");
    if !is_quote {
        return next.run(req).await;
    }
    if let Some(limits) = &counting.limits
        && let Err(exhausted) = limits.lock().expect("not poisoned").admit(Instant::now())
    {
        counting
            .stats
            .refused_quotes
            .fetch_add(1, Ordering::Relaxed);
        tracing::debug!(exhausted, "refused a mint quote over the issue limit");
        return (
            StatusCode::TOO_MANY_REQUESTS,
            axum::Json(serde_json::json!({
                "code": 0,
                "detail": "this mint is serving too many quotes: try again shortly",
            })),
        )
            .into_response();
    }
    let counter = if counting.private {
        &counting.stats.private_quotes
    } else {
        &counting.stats.public_quotes
    };
    counter.fetch_add(1, Ordering::Relaxed);
    next.run(req).await
}

#[cfg(test)]
mod tests {
    use cashu::Amount;
    use cashu::amount::{FeeAndAmounts, SplitTarget};
    use cashu::dhke::construct_proofs;
    use cashu::nuts::{
        Keys, MintQuoteBolt11Request, MintQuoteBolt11Response, MintQuoteState as QuoteState,
        PreMintSecrets, Proofs, SwapRequest,
    };
    use cdk_common::QuoteId;
    use cdk_common::mint_quote::{MintQuoteRequest, MintQuoteResponse};

    use super::*;
    use crate::tempdir::Dir;

    fn byte_mint(dir: &Dir) -> MintConfig {
        MintConfig {
            url: "http://127.0.0.1:0".into(),
            unit: "byte".into(),
            seed: vec![7; 32],
            file: dir.path().join("mint.sqlite"),
            max_amount: 1_000_000_000,
        }
    }

    /// Blinded outputs for `amount` against the mint's active byte keyset.
    fn outputs(mint: &Mint, amount: u64) -> (PreMintSecrets, Keys) {
        let id = mint.get_active_keysets()[&CurrencyUnit::Custom("byte".into())];
        let keys = mint
            .keyset_pubkeys(&id)
            .expect("the active keyset's keys")
            .keysets
            .remove(0)
            .keys;
        let denominations: FeeAndAmounts =
            (0, keys.keys().keys().map(|a| u64::from(*a)).collect()).into();
        let secrets =
            PreMintSecrets::random(id, Amount::from(amount), &SplitTarget::None, &denominations)
                .expect("blind the outputs");
        (secrets, keys)
    }

    /// Vouchers issued straight off the mint, the way the market issues them.
    async fn issue(mint: &Mint, amount: u64) -> Proofs {
        let (secrets, keys) = outputs(mint, amount);
        let signatures = mint
            .blind_sign(secrets.blinded_messages())
            .await
            .expect("sign the outputs");
        construct_proofs(signatures, secrets.rs(), secrets.secrets(), &keys)
            .expect("unblind the signatures")
    }

    /// Redeem `proofs` at the mint, for fresh outputs of the same value.
    async fn redeem(mint: &Mint, proofs: Proofs) -> Result<(), cdk::Error> {
        let amount = proofs.iter().map(|p| u64::from(p.amount)).sum();
        let (secrets, _) = outputs(mint, amount);
        mint.process_swap_request(SwapRequest::new(proofs, secrets.blinded_messages()))
            .await
            .map(|_| ())
    }

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

    #[test]
    fn quotes_are_rationed_per_minute_and_refill() {
        let t0 = Instant::now();
        let mut limits = Limits::new(
            IssueLimit {
                quotes_per_minute: 2,
            },
            t0,
        );

        assert_eq!(limits.admit(t0), Ok(()));
        assert_eq!(limits.admit(t0), Ok(()), "a minute's worth at once");
        assert_eq!(limits.admit(t0), Err("quotes asked for"));

        // Half a minute is one quote back, and an hour idle fills it to a
        // minute's worth, not beyond.
        assert_eq!(limits.admit(t0 + Duration::from_secs(30)), Ok(()));
        let later = t0 + Duration::from_secs(3_600);
        assert_eq!(limits.admit(later), Ok(()));
        assert_eq!(limits.admit(later), Ok(()));
        assert_eq!(limits.admit(later), Err("quotes asked for"));
    }

    #[test]
    fn a_zero_rate_switches_the_limit_off() {
        let t0 = Instant::now();
        let mut limits = Limits::new(
            IssueLimit {
                quotes_per_minute: 0,
            },
            t0,
        );
        for _ in 0..1_000 {
            assert_eq!(limits.admit(t0), Ok(()));
        }
    }

    #[tokio::test]
    async fn a_byte_denominated_mint_comes_up_with_an_active_keyset() {
        let dir = Dir::new();
        let mint = build(&byte_mint(&dir)).await.expect("build a byte mint");

        assert!(
            mint.get_active_keysets()
                .contains_key(&CurrencyUnit::Custom("byte".into())),
            "the byte keyset should be active"
        );
    }

    #[tokio::test]
    async fn a_voucher_redeemed_before_a_restart_is_still_spent_after_it() {
        // The keyset comes back after a restart because it is derived from the
        // seed. If the spent-proof set did not come back with it, a voucher
        // already paid out would validate a second time.
        let dir = Dir::new();
        let config = byte_mint(&dir);

        let proofs = {
            let mint = build(&config).await.expect("build the mint");
            let proofs = issue(&mint, 1024).await;
            redeem(&mint, proofs.clone())
                .await
                .expect("the first redemption goes through");
            mint.stop().await.expect("stop the mint");
            proofs
        };

        let mint = build(&config).await.expect("reopen the same database");
        let err = redeem(&mint, proofs)
            .await
            .expect_err("a voucher redeemed before the restart must not redeem again");
        assert!(
            matches!(err, cdk::Error::TokenAlreadySpent),
            "expected the proofs to be spent, got: {err}"
        );
    }

    /// Ask `mint` for a quote of `amount` bytes and check it, as a wallet
    /// would before minting against it.
    async fn quote_and_check(mint: &Mint, amount: u64) -> MintQuoteBolt11Response<QuoteId> {
        let created = mint
            .get_mint_quote(MintQuoteRequest::Bolt11(MintQuoteBolt11Request {
                amount: Amount::from(amount),
                unit: CurrencyUnit::Custom("byte".into()),
                description: None,
                pubkey: None,
            }))
            .await
            .expect("the mint issues a quote");
        let MintQuoteResponse::Bolt11(created) = created else {
            panic!("asked for a Bolt11 quote, got {created:?}");
        };
        match mint
            .check_mint_quote(&created.quote)
            .await
            .expect("check the quote")
        {
            MintQuoteResponse::Bolt11(checked) => checked,
            other => panic!("a Bolt11 quote checked as {other:?}"),
        }
    }

    #[tokio::test]
    async fn quotes_issued_before_a_restart_do_not_collide_with_those_after_it() {
        // The mint keeps every quote's lookup id, and every payment's id, unique
        // across its whole database. Once that database outlives a restart, an
        // id counted from startup would be issued a second time, and the second
        // quote would be refused or never paid.
        let dir = Dir::new();
        let config = byte_mint(&dir);

        let before = {
            let mint = build(&config).await.expect("build the mint");
            let quote = quote_and_check(&mint, 1024).await;
            mint.stop().await.expect("stop the mint");
            quote
        };

        let mint = build(&config).await.expect("reopen the same database");
        let after = quote_and_check(&mint, 1024).await;

        assert_ne!(before.quote, after.quote);
        assert_ne!(before.request, after.request, "lookup ids are unique");
        for quote in [&before, &after] {
            assert_eq!(quote.state, QuoteState::Paid, "{quote:?}");
            assert_eq!(quote.amount_paid, Amount::from(1024));
        }

        // And the quote from before the restart is still on record, still paid.
        let reread = match mint.check_mint_quote(&before.quote).await {
            Ok(MintQuoteResponse::Bolt11(q)) => q,
            other => panic!("the earlier quote should still be known, got {other:?}"),
        };
        assert_eq!(reread.state, QuoteState::Paid);
        assert_eq!(reread.request, before.request);
    }

    /// POST a quote request for 1024 bytes to `router`, returning the status.
    async fn ask_for_a_quote(router: &Router) -> StatusCode {
        use tower::ServiceExt;
        let body = r#"{"amount":1024,"unit":"byte"}"#;
        router
            .clone()
            .oneshot(
                axum::http::Request::post("/v1/mint/quote/bolt11")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(body))
                    .expect("a request"),
            )
            .await
            .expect("a response")
            .status()
    }

    #[tokio::test]
    async fn only_the_private_listener_serves_quotes_unless_auto_accept_is_on() {
        let dir = Dir::new();
        let mint = Arc::new(build(&byte_mint(&dir)).await.expect("build the mint"));
        let stats = Arc::new(Stats::default());
        let (public, private) = routers(
            Arc::clone(&mint),
            false,
            IssueLimit::default(),
            Arc::clone(&stats),
        )
        .await
        .expect("routers");

        assert!(
            ask_for_a_quote(&private).await.is_success(),
            "merchantd's listener pays"
        );
        assert_eq!(ask_for_a_quote(&public).await, StatusCode::NOT_FOUND);
        assert_eq!(stats.private_quotes.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn auto_accept_is_rationed_by_requests_on_the_public_listener_alone() {
        let dir = Dir::new();
        let mint = Arc::new(build(&byte_mint(&dir)).await.expect("build the mint"));
        let stats = Arc::new(Stats::default());
        let limit = IssueLimit {
            quotes_per_minute: 1,
        };
        let (public, private) = routers(Arc::clone(&mint), true, limit, Arc::clone(&stats))
            .await
            .expect("routers");

        assert!(ask_for_a_quote(&public).await.is_success());
        assert_eq!(
            ask_for_a_quote(&public).await,
            StatusCode::TOO_MANY_REQUESTS
        );
        // The private listener is never rationed: selling is merchantd's call.
        assert!(ask_for_a_quote(&private).await.is_success());
        assert!(ask_for_a_quote(&private).await.is_success());
        assert_eq!(stats.refused_quotes.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn a_seed_is_created_once_and_read_back_after() {
        let dir = Dir::new();
        let path = dir.path().join("sub").join("mint.seed");
        let first = load_or_create_seed(&path).expect("create");
        assert_eq!(first.len(), 32);
        assert_eq!(load_or_create_seed(&path).expect("read"), first);
    }
}
