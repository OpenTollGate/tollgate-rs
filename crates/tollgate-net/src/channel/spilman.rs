//! Real Cashu Spilman channels, funded against a real mint.
//!
//! This is the backend that puts value at stake. A channel is a 2-of-2 multisig
//! Cashu token; a ratchet turn is a signed balance update the receiver can take
//! to the mint; settlement is a swap.
//!
//! # No prices here
//!
//! The receiver is configured with a pricing entry that **prices nothing**, so
//! the Spilman layer's "amount due" is always zero and it enforces only the
//! ratchet and the signature. That is deliberate and it is the whole fit: delivery has no price
//! ([`tollgate-vouchers.md`]), and what a peer may draw is decided by
//! `tollgate-core`'s admission control against the grant. The channel layer
//! carries the money; it does not decide how much is owed.
//!
//! # Where the vouchers come from
//!
//! A channel is funded in a mint the **peer** accepts, and the peer's own mint
//! is always reachable — it is the peer we are already talking to. So funding a
//! channel to pay a peer means first holding that peer's vouchers, and how a
//! peer came to hold vouchers is not the protocol's business, any more than how
//! it came to hold sats. Buying them is [`crate::market`]'s job, and this layer
//! only asks it for what a channel needs.
//!
//! [`tollgate-vouchers.md`]: https://github.com/OpenTollGate/tollgate-rs/blob/master/docs/design/core/tollgate-vouchers.md

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use cdk::mint::Mint;
use cdk_spilman::configurable_host::{
    ConfigurableHost, ConfigurableHostConfig, KeysetCacheEntry, StorageConfig, UnitPricingConfig,
};
use cdk_spilman::configurable_networking::fetch_and_cache_keysets;
use cdk_spilman::{
    ChannelState, CloseError, ConfigurableClientHost, MemoryClientStorage, ReqwestClientNetworking,
    SpilmanBridge, SpilmanClientBridge, SpilmanClientNetworking, SpilmanHost,
    SpilmanKeysetRefresher, SpilmanMintClient, extract_nut00_error_code,
};
use tollgate_protocol::{ChannelId, PubKey, Signature};
use tracing::debug;

use super::{CannotSettle, ChannelBackend, FundedChannel, MintNotAccepted, VerifiedChannel};

mod funding;

use funding::Funding;

/// Shortest expiry we will accept on a channel a peer funds to pay us.
///
/// The refund timelock is what a payer would rely on if the receiver vanished,
/// so a receiver insisting on a floor is insisting the payer keep its own
/// protection.
const MIN_EXPIRY_SECONDS: u64 = 3_600;

/// How long a channel *we* fund stays refundable to us.
///
/// Comfortably longer than the floor we ourselves require, because time passes
/// between choosing the expiry and the peer checking it: the vouchers have to
/// be acquired, the channel funded, the Accept sent and the funding verified.
/// Funding at exactly the minimum means every one of those seconds counts
/// against us, and the peer refuses a channel that was valid when it was built.
const CHANNEL_TTL_SECONDS: u64 = 2 * MIN_EXPIRY_SECONDS;

/// Largest single proof amount used when funding.
///
/// Amounts are powers of two, so a channel takes one proof per set bit and the
/// count is what the funding blob and the spent-proof set both scale with. Byte
/// denomination makes the numbers large — a 1 GiB channel is a 30-bit number,
/// so ~30 proofs at worst and about 15 on average.
///
/// Capping this low would be ruinous here: at 8192 the same channel would need
/// 131,072 proofs. The cap exists for money-denominated channels where a small
/// maximum keeps individual proofs cheap; for bytes it has to track the
/// capacity being funded.
const MAX_PROOF_AMOUNT: u64 = 1 << 30;

/// How long one request to a peer's mint may take.
///
/// These calls sit inline in a purchase, so a mint that hangs would otherwise
/// stall buying until the operating system gave up on the socket.
const MINT_REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// What a Spilman backend needs to know about this node.
#[derive(Debug, Clone)]
pub struct SpilmanConfig {
    /// This node's own mint, which its peers fund channels against.
    pub mint: Arc<Mint>,
    /// The URL peers reach that mint on, as advertised in our Offer.
    pub mint_url: String,
    /// The unit our keyset denominates in.
    pub unit: String,
    /// Mints we will take payment in, from the Offer. A channel funded against
    /// anything else is refused.
    pub accepted_mints: Vec<String>,
    /// Secret key this node signs channel state with, hex-encoded.
    pub secret_key_hex: String,
    /// What this node holds, and what it pays peers out of.
    pub wallet: crate::wallet::Wallet,
    /// Which of its holdings is the money — the paper a peer has to accept
    /// before this node can pay it. `None` for a node that only sells.
    pub money: Option<Money>,
}

/// Where a node's money is, and in what.
#[derive(Debug, Clone)]
pub struct Money {
    /// A mint that sells its paper for money.
    pub mint: String,
    /// The unit that mint's paper is denominated in.
    pub unit: String,
}

type Client =
    SpilmanClientBridge<ConfigurableClientHost<MemoryClientStorage>, ReqwestClientNetworking>;

/// Cashu Spilman channels.
pub struct SpilmanChannels {
    config: SpilmanConfig,
    /// The wallet is async and this backend is not, so its calls are handed
    /// back to the runtime rather than run on a worker.
    runtime: tokio::runtime::Handle,
    /// Our side of channels we fund to pay peers.
    ///
    /// Behind a mutex because the client host keeps its storage in a `RefCell`
    /// — `Send` but not `Sync`, so the lock is what makes the backend shareable
    /// rather than a concession to contention.
    client: Mutex<Client>,
    /// Our side of channels peers fund to pay us.
    server: SpilmanBridge<ConfigurableHost>,
    /// How we reach an accepted mint that is not ours, to settle a channel a
    /// peer funded there.
    remote: ReqwestClientNetworking,
}

impl std::fmt::Debug for SpilmanChannels {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SpilmanChannels")
            .field("mint_url", &self.config.mint_url)
            .field("unit", &self.config.unit)
            .finish_non_exhaustive()
    }
}

impl SpilmanChannels {
    /// Build a backend around this node's mint and key.
    ///
    /// Must be called from inside a tokio runtime: the client networking binds
    /// the current handle so it can drive HTTP from the synchronous trait.
    pub fn new(config: SpilmanConfig) -> Result<Self> {
        let secret = cashu::nuts::SecretKey::from_hex(&config.secret_key_hex)
            .map_err(|e| anyhow!("channel secret key is not valid: {e}"))?;

        let mut client_host = ConfigurableClientHost::new_in_memory();
        client_host.add_key(secret);
        let networking = ReqwestClientNetworking::new(MINT_REQUEST_TIMEOUT)
            .map_err(|e| anyhow!("build the mint client: {e}"))?;
        let client = SpilmanClientBridge::new(client_host, networking);

        // Every mint we accept, trusted for our unit. Accept or refuse is
        // binary — there is no haircut, because what an issuer's paper is worth
        // is expressed in what you pay for it on the market, not in a discount
        // applied at settlement.
        let mints = config
            .accepted_mints
            .iter()
            .map(|url| (url.clone(), vec![config.unit.clone()]))
            .collect();

        let host = ConfigurableHost::new(
            ConfigurableHostConfig {
                mints,
                min_expiry_seconds: MIN_EXPIRY_SECONDS,
                pricing_scale: 1,
                storage: StorageConfig::Memory,
                // A pricing entry with **nothing priced**. The amount due is a
                // linear combination over priced variables, so an empty set
                // makes it zero always: this layer enforces the ratchet and the
                // signature and never second-guesses what is owed. What a peer
                // may draw is core's decision, against the grant.
                //
                // The entry has to exist even though it prices nothing — the
                // host refuses to start if a unit is trusted by a mint but
                // absent from the table.
                pricing: HashMap::from([(
                    config.unit.clone(),
                    UnitPricingConfig {
                        min_capacity: 1,
                        max_amount_per_output: Some(MAX_PROOF_AMOUNT),
                        variables: HashMap::new(),
                    },
                )]),
            },
            &config.secret_key_hex,
        )
        .map_err(|e| anyhow!("build the channel receiver: {e}"))?;

        let remote = ReqwestClientNetworking::new(MINT_REQUEST_TIMEOUT)
            .map_err(|e| anyhow!("build the settlement client: {e}"))?;

        Ok(Self {
            config,
            runtime: tokio::runtime::Handle::current(),
            client: Mutex::new(client),
            server: SpilmanBridge::new(host),
            remote,
        })
    }

    /// The mint URL this node advertises.
    pub fn mint_url(&self) -> &str {
        &self.config.mint_url
    }

    /// The mint a channel a peer funded to pay us was funded in.
    fn funding_mint(&self, channel_id: &str) -> Result<String> {
        let funding = self
            .server
            .host()
            .get_funding_data(channel_id)
            .ok_or_else(|| anyhow!("no funding recorded for channel {channel_id}"))?;
        mint_of_params(&funding.params_json)
    }

    /// Make sure we hold the keys for the keyset a peer funded against, and
    /// return them as cdk-spilman reads them.
    ///
    /// The receiver will not accept a channel on a keyset it cannot verify
    /// signatures against, and there is no way to know ahead of time which
    /// keyset of which accepted mint a peer will choose.
    fn cache_keyset(&self, mint: &str, id: cashu::nuts::Id) -> Result<String> {
        if !self.config.accepted_mints.iter().any(|m| m == mint) {
            return Err(MintNotAccepted(mint.to_owned()).into());
        }

        let keyset_id = id.to_string();
        let info = self
            .client
            .lock()
            .expect("not poisoned")
            .fetch_keyset_info(mint, &keyset_id)
            .map_err(|e| anyhow!("read keyset {keyset_id} from {mint}: {e}"))?;

        self.server
            .host()
            .set_keyset(
                mint,
                id,
                KeysetCacheEntry {
                    info_json: info.clone(),
                    active: true,
                    unit: crate::mint::currency_unit(&self.config.unit),
                },
            )
            .map_err(|e| anyhow!("cache keyset {keyset_id}: {e}"))?;
        Ok(info)
    }
}

/// Seconds since the epoch, for the channel's refund timelock.
///
/// The only wall-clock read in the crate. Everything the protocol measures is
/// relative and comes from the host's monotonic clock.
fn now_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Which mint a channel's parameters say it was funded in.
fn mint_of_params(params_json: &str) -> Result<String> {
    let params: serde_json::Value =
        serde_json::from_str(params_json).context("channel parameters are not JSON")?;
    params["mint"]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| anyhow!("channel parameters name no mint"))
}

/// Where a channel's closing swap goes.
#[derive(Debug, PartialEq, Eq)]
enum Settlement {
    /// Our own mint, in this process.
    Ours,
    /// Another mint we accept, over HTTP.
    Remote,
}

/// Settle in the mint the channel was funded in: a peer may fund against any
/// mint we accept, and only the issuer of the funding proofs can swap them.
fn settlement_for(own_mint_url: &str, funding_mint: &str) -> Settlement {
    if own_mint_url.trim_end_matches('/') == funding_mint.trim_end_matches('/') {
        Settlement::Ours
    } else {
        Settlement::Remote
    }
}

fn channel_id_from_hex(s: &str) -> Result<ChannelId> {
    let bytes = hex::decode(s).with_context(|| format!("channel id {s:?} is not hex"))?;
    let bytes: [u8; 32] = bytes
        .try_into()
        .map_err(|_| anyhow!("channel id {s:?} is not 32 bytes"))?;
    Ok(ChannelId(bytes))
}

fn channel_id_to_hex(id: ChannelId) -> String {
    hex::encode(id.0)
}

fn signature_from_hex(s: &str) -> Result<Signature> {
    let bytes = hex::decode(s).context("signature is not hex")?;
    let bytes: [u8; 64] = bytes
        .try_into()
        .map_err(|_| anyhow!("signature is not 64 bytes"))?;
    Ok(Signature(bytes))
}

impl SpilmanChannels {
    /// Buy `capacity` of a peer's vouchers, paying at its market.
    ///
    /// Three steps and no negotiation: ask what the peer takes, turn money into
    /// that issuer's paper, and hand it over for vouchers. The peer prices the
    /// trade; this side only decides whether to accept the price by going
    /// through with it.
    fn buy_vouchers(&self, mint_url: &str, capacity: u64, keyset_info: &str) -> Result<String> {
        let Some(money) = &self.config.money else {
            bail!(
                "this node holds no money, so it cannot buy the vouchers it would pay {mint_url} with"
            );
        };

        // The peer's market is served beside its mint, at the URL it already
        // advertises.
        let market = crate::market::info_of(mint_url)?;
        let price = market
            .accepts
            .iter()
            .find(|a| {
                a.mint.trim_end_matches('/') == money.mint.trim_end_matches('/')
                    && a.unit == money.unit
            })
            .ok_or_else(|| {
                anyhow!(
                    "{mint_url} does not take {} from {}; it takes {:?}",
                    money.unit,
                    money.mint,
                    market
                        .accepts
                        .iter()
                        .map(|a| format!("{} {}", a.unit, a.mint))
                        .collect::<Vec<_>>()
                )
            })?;

        // Rounded up, so a purchase is never short of what it asked for.
        let owed = (capacity as u128)
            .div_ceil(price.bytes_per_unit as u128)
            .max(1) as u64;
        debug!(
            capacity,
            owed,
            unit = %money.unit,
            mint = %money.mint,
            "buying vouchers"
        );

        let payment = self.take_from_wallet(money, owed)?;
        crate::market::buy(mint_url, capacity, &self.config.unit, keyset_info, &payment)
    }

    /// Take `owed` out of the wallet, topping it up if it is short.
    ///
    /// Spending what is already held is the normal path and is local
    /// arithmetic. The top-up is the exception, and it is deliberately in-line
    /// rather than a background chore: a node that has run out of money has
    /// stopped being able to buy transit, and the operator wants that to show
    /// up as a slow purchase rather than a silent one.
    ///
    /// Where the money mint settles its own invoices this is invisible. Where a
    /// human has to pay one, it fails with the invoice in the log, and the
    /// operator tops up from `tolltop` instead.
    fn take_from_wallet(&self, money: &Money, owed: u64) -> Result<String> {
        let wallet = self.config.wallet.clone();
        let handle = self.runtime.clone();

        tokio::task::block_in_place(|| {
            handle.block_on(async move {
                if let Ok(token) = wallet.spend(&money.mint, &money.unit, owed).await {
                    return Ok(token);
                }

                let held = wallet.balance_of(&money.mint, &money.unit).await;
                debug!(held, owed, "topping up to cover a purchase");
                let top_up = wallet
                    .top_up(
                        &money.mint,
                        &money.unit,
                        owed.saturating_sub(held).max(owed),
                    )
                    .await?;
                crate::market::wait_until_paid(&money.mint, &top_up.quote)
                    .with_context(|| format!("pay {}", top_up.request))?;
                wallet.collect(&top_up).await?;

                wallet
                    .spend(&money.mint, &money.unit, owed)
                    .await
                    .with_context(|| format!("pay {owed} {} after topping up", money.unit))
            })
        })
    }

    /// Open a channel to `peer` from a token of its vouchers, and describe the
    /// funding for the peer.
    fn open(
        &self,
        peer: PubKey,
        mint_url: &str,
        token: &str,
        keyset_info: &str,
    ) -> Result<FundedChannel> {
        let ours = cashu::nuts::SecretKey::from_hex(&self.config.secret_key_hex)
            .map_err(|e| anyhow!("bad secret: {e}"))?
            .public_key()
            .to_hex();

        let client = self.client.lock().expect("not poisoned");
        let opened = client
            .open_channel_from_token(
                token,
                &hex::encode(peer.0),
                &ours,
                now_seconds() + CHANNEL_TTL_SECONDS,
                keyset_info,
                MAX_PROOF_AMOUNT,
            )
            .map_err(|e| anyhow!("open a channel against {mint_url}: {e}"))?;

        let opening = client
            .sign_channel_registration(&opened.channel_id)
            .map_err(|e| anyhow!("prepare funding for {}: {e}", opened.channel_id))?;

        Ok(FundedChannel {
            channel_id: channel_id_from_hex(&opened.channel_id)?,
            capacity: opened.capacity,
            funding: Funding::from_opening(&opening)?.encode(),
        })
    }
}

impl ChannelBackend for SpilmanChannels {
    fn fund(&self, peer: PubKey, mint_url: &str, capacity: u64) -> Result<FundedChannel> {
        // Which keyset denominates in our unit. A mint may run several — an old
        // one still being redeemed alongside the active one — and only the
        // active one can be funded against.
        let keyset_id = crate::market::active_keyset(mint_url, &self.config.unit)?;
        let keyset_info = self
            .client
            .lock()
            .expect("not poisoned")
            .fetch_keyset_info(mint_url, &keyset_id)
            .map_err(|e| anyhow!("could not read keyset {keyset_id} from {mint_url}: {e}"))?;

        // Buying the peer's vouchers is outside the protocol: a peer arrives
        // holding them or it gets no service, and how it came to hold them is
        // as much its own business as how it came to hold money.
        let token = self
            .buy_vouchers(mint_url, capacity, &keyset_info)
            .with_context(|| format!("buy {capacity} {} from {mint_url}", self.config.unit))?;

        self.open(peer, mint_url, &token, &keyset_info)
    }

    fn verify(&self, _peer: PubKey, funding: &[u8]) -> Result<VerifiedChannel> {
        let funding = Funding::decode(funding)?;

        // A keyset is only acceptable once we hold its keys, and we cannot know
        // in advance which of our accepted mints a peer will fund against — or
        // which keyset that mint had active at the time. So it is fetched when
        // the funding names it, from the mint that issued it.
        let keyset_info = self.cache_keyset(&funding.terms.mint, funding.terms.keyset_id)?;

        // Everything the peer left out, rebuilt from what it sent: the keyset,
        // the secret we share, and from those the proofs themselves.
        let ours = cashu::nuts::SecretKey::from_hex(&self.config.secret_key_hex)
            .map_err(|e| anyhow!("bad secret: {e}"))?;
        let opening = funding.open(&keyset_info, &ours)?;

        // This is where the money is checked: locked to the two of us, against
        // a mint and keyset we accept, and not already spent.
        let funded = self
            .server
            .fund_channel(
                &opening.channel_id,
                0,
                &opening.signature,
                Some(&opening.params),
                Some(&opening.proofs),
            )
            .map_err(|e| anyhow!("peer funding did not verify: {e:?}"))?;

        Ok(VerifiedChannel {
            channel_id: channel_id_from_hex(&opening.channel_id)?,
            capacity: funded.capacity,
            mint_url: funding.terms.mint,
        })
    }

    fn sign_update(&self, channel_id: ChannelId, cumulative: u64) -> Result<Signature> {
        let payment = self
            .client
            .lock()
            .expect("not poisoned")
            .sign_and_record_payment(&channel_id_to_hex(channel_id), cumulative)
            .map_err(|e| anyhow!("sign a channel update: {e}"))?;
        signature_from_hex(&payment.signature)
    }

    fn verify_update(
        &self,
        _peer: PubKey,
        channel_id: ChannelId,
        cumulative: u64,
        signature: Signature,
    ) -> bool {
        // Checks that the channel is open, that the balance fits its capacity
        // and that the signature is the funder's over it, and records nothing:
        // `process_payment` would keep the balance as a side effect, before we
        // know whether the rest of the purchase — or core — agrees. Whether
        // the balance increases is core's check, against the grant; so is how
        // much is owed, since no pricing is configured.
        self.server
            .validate_payment(
                &channel_id_to_hex(channel_id),
                cumulative,
                &hex::encode(signature.0),
                &String::new(),
            )
            .is_ok()
    }

    fn record_update(
        &self,
        _peer: PubKey,
        channel_id: ChannelId,
        cumulative: u64,
        signature: Signature,
    ) -> Result<()> {
        // Validates again and keeps the balance as the channel's latest state,
        // which is what settlement submits. The receiver only ever moves it
        // forward.
        let id = channel_id_to_hex(channel_id);
        self.server
            .process_payment(
                &id,
                cumulative,
                &hex::encode(signature.0),
                None,
                None,
                &String::new(),
            )
            .map_err(|e| anyhow!("record an update on {id}: {e:?}"))?;
        Ok(())
    }

    fn settle(&self, channel_id: ChannelId) -> Result<()> {
        let id = channel_id_to_hex(channel_id);
        // The node retries settlements, so one that already went through has
        // to read as done rather than as a failure to retry.
        if self.server.host().get_channel_state(&id) == ChannelState::Closed {
            return Ok(());
        }
        // No funding recorded for the channel is refused the same way every
        // time, so it is not worth retrying.
        let mint = self
            .funding_mint(&id)
            .map_err(|e| CannotSettle(format!("{id}: {e:#}")))?;
        // The swap goes to the mint that issued the funding. The bridge passes
        // that mint's URL through to each call, so the networking only has to
        // be the right kind.
        let closed = match settlement_for(&self.config.mint_url, &mint) {
            Settlement::Ours => {
                let networking = MintNetworking::new(Arc::clone(&self.config.mint));
                self.server
                    .execute_unilateral_close(&id, &networking, &networking)
            }
            Settlement::Remote => {
                let networking = RemoteMint {
                    http: &self.remote,
                    host: self.server.host(),
                    runtime: self.runtime.clone(),
                };
                self.server
                    .execute_unilateral_close(&id, &networking, &networking)
            }
        };
        match closed {
            Ok(_) | Err(CloseError::AlreadyClosed { .. }) => Ok(()),
            Err(e) if close_error_is_permanent(&e) => {
                Err(CannotSettle(format!("{id} in {mint}: {e}")).into())
            }
            Err(e) => Err(anyhow!("settle {id} in {mint}: {e:?}")),
        }
    }
}

/// Whether retrying a failed close could ever help.
///
/// A close the bridge refused before reaching the mint — a channel it has no
/// funding for (404), one already gone (410), or one with no payment to claim
/// (400) — is refused the same way every time. So is a mint's rejection of the
/// proofs themselves (NUT-00 codes 10xxx and 11xxx): the channel state is
/// corrupted or already spent. Everything else — the mint unreachable, a
/// keyset still stale after the bridge's own refresh, storage — may pass.
fn close_error_is_permanent(e: &CloseError) -> bool {
    let mint_code = |v: &serde_json::Value| match v {
        serde_json::Value::String(raw) => extract_nut00_error_code(raw),
        other => extract_nut00_error_code(&other.to_string()),
    };
    let proof_error = |code: Option<u32>| code.is_some_and(|c| (10_000..12_000).contains(&c));
    match e {
        CloseError::ValidationFailed { status, .. } => matches!(status, 400 | 404 | 410),
        CloseError::UnknownChannel { .. } => true,
        CloseError::MintRejected { mint_error, .. } => proof_error(mint_code(mint_error)),
        CloseError::MintRejectedAfterRetry { retry_error, .. } => {
            proof_error(mint_code(retry_error))
        }
        _ => false,
    }
}

/// Talks to a mint that is running in this process.
///
/// Settling our own vouchers cancels our own claim, so the swap never leaves
/// the node — no HTTP, and it works during an upstream outage, which is the
/// single largest resilience gain of the voucher model.
struct MintNetworking {
    mint: Arc<Mint>,
    runtime: tokio::runtime::Handle,
}

impl MintNetworking {
    fn new(mint: Arc<Mint>) -> Self {
        Self {
            mint,
            runtime: tokio::runtime::Handle::current(),
        }
    }
}

impl SpilmanMintClient for MintNetworking {
    fn call_mint_swap(&self, _mint_url: &str, swap_request_json: &str) -> Result<String, String> {
        let request: cashu::nuts::SwapRequest =
            serde_json::from_str(swap_request_json).map_err(|e| e.to_string())?;

        let mint = Arc::clone(&self.mint);
        let handle = self.runtime.clone();
        // The backend trait is synchronous and the node already drives it from
        // a blocking thread, so this hands the future back to the runtime
        // rather than blocking a worker.
        let response = tokio::task::block_in_place(|| {
            handle.block_on(async move { mint.process_swap_request(request).await })
        })
        .map_err(|e| e.to_string())?;

        serde_json::to_string(&response).map_err(|e| e.to_string())
    }
}

impl SpilmanKeysetRefresher for MintNetworking {
    fn refresh(&self, _mint: &str) -> Result<(), String> {
        // Ours, and always current.
        Ok(())
    }
}

/// Talks to another mint we accept, over HTTP.
///
/// A peer may fund against any mint in our Offer, and only the mint that issued
/// the funding can swap it, so settling such a channel has to leave the node.
struct RemoteMint<'a> {
    http: &'a ReqwestClientNetworking,
    host: &'a ConfigurableHost,
    runtime: tokio::runtime::Handle,
}

impl SpilmanMintClient for RemoteMint<'_> {
    fn call_mint_swap(&self, mint_url: &str, swap_request_json: &str) -> Result<String, String> {
        SpilmanClientNetworking::call_mint_swap(self.http, mint_url, swap_request_json)
    }
}

impl SpilmanKeysetRefresher for RemoteMint<'_> {
    fn refresh(&self, mint: &str) -> Result<(), String> {
        // Not ours, so its keysets can rotate without our knowing, and the
        // close outputs have to be made for the one it has active now.
        let handle = self.runtime.clone();
        tokio::task::block_in_place(|| handle.block_on(fetch_and_cache_keysets(self.host, mint)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_channel_id_round_trips_through_hex() {
        let id = ChannelId([7; 32]);
        assert_eq!(
            channel_id_from_hex(&channel_id_to_hex(id)).expect("parse"),
            id
        );
    }

    #[test]
    fn a_channel_id_of_the_wrong_length_is_rejected() {
        assert!(channel_id_from_hex(&hex::encode([0u8; 16])).is_err());
        assert!(channel_id_from_hex("nonsense").is_err());
    }

    #[test]
    fn a_channel_funded_in_our_mint_settles_in_process() {
        assert_eq!(
            settlement_for("http://ours:3338", "http://ours:3338"),
            Settlement::Ours
        );
        // A trailing slash is the same mint.
        assert_eq!(
            settlement_for("http://ours:3338/", "http://ours:3338"),
            Settlement::Ours
        );
    }

    #[test]
    fn a_channel_funded_in_another_mint_settles_there() {
        assert_eq!(
            settlement_for("http://ours:3338", "http://theirs:3338"),
            Settlement::Remote
        );
    }

    #[test]
    fn the_funding_mint_is_read_from_the_channel_parameters() {
        let params = r#"{"mint":"http://theirs:3338","unit":"byte"}"#;
        assert_eq!(mint_of_params(params).expect("mint"), "http://theirs:3338");
        assert!(mint_of_params(r#"{"unit":"byte"}"#).is_err());
        assert!(mint_of_params("nonsense").is_err());
    }

    #[test]
    fn a_signature_of_the_wrong_length_is_rejected() {
        assert!(signature_from_hex(&hex::encode([0u8; 64])).is_ok());
        assert!(signature_from_hex(&hex::encode([0u8; 32])).is_err());
    }

    /// Byte mints on loopback ports and backends around them, shared by the
    /// purchase, funding and settlement tests below.
    mod live {
        use std::str::FromStr;
        use std::sync::atomic::{AtomicU64, Ordering};

        use cashu::amount::{FeeAndAmounts, SplitTarget};
        use cashu::mint_url::MintUrl;
        use cashu::nuts::{CurrencyUnit, PreMintSecrets, Token};
        use cdk_spilman::parse_keyset_info_from_json;

        use super::*;
        use crate::mint::MintConfig;

        pub(super) const PAYER: [u8; 32] = [1; 32];
        pub(super) const PAYEE: [u8; 32] = [2; 32];

        pub(super) fn pubkey(secret: [u8; 32]) -> PubKey {
            PubKey(
                cashu::nuts::SecretKey::from_slice(&secret)
                    .expect("key")
                    .public_key()
                    .to_bytes(),
            )
        }

        /// A byte mint served over HTTP, as a peer reaches it.
        pub(super) async fn serve_mint(seed: u8) -> (Arc<Mint>, String) {
            let addr = std::net::TcpListener::bind("127.0.0.1:0")
                .and_then(|l| l.local_addr())
                .expect("a free port");
            let url = format!("http://{addr}");
            let mint = Arc::new(
                crate::mint::build(&MintConfig {
                    url: url.clone(),
                    unit: "byte".into(),
                    seed: vec![seed; 32],
                    max_amount: u64::MAX,
                })
                .await
                .expect("build the mint"),
            );
            tokio::spawn(crate::mint::serve(
                Arc::clone(&mint),
                axum::Router::new(),
                addr,
                std::future::pending(),
            ));
            for _ in 0..100 {
                if tokio::net::TcpStream::connect(addr).await.is_ok() {
                    return (mint, url);
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
            panic!("the mint never came up on {addr}");
        }

        /// A directory that removes itself, for a wallet nothing reads again.
        pub(super) struct TempDir(std::path::PathBuf);

        impl Drop for TempDir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }

        /// A backend running `mint` at `mint_url` and taking payment in
        /// `accepted_mints`, with a wallet it never uses.
        pub(super) async fn backend(
            mint: &Arc<Mint>,
            mint_url: &str,
            accepted_mints: Vec<String>,
            secret: [u8; 32],
        ) -> (SpilmanChannels, TempDir) {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let dir = TempDir(std::env::temp_dir().join(format!(
                "tollgate-spilman-test-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            )));
            let wallet = crate::wallet::Wallet::open(dir.0.join("wallet.sqlite"), [7; 64], "byte")
                .await
                .expect("open a wallet");
            let backend = SpilmanChannels::new(SpilmanConfig {
                mint: Arc::clone(mint),
                mint_url: mint_url.into(),
                unit: "byte".into(),
                accepted_mints,
                secret_key_hex: hex::encode(secret),
                wallet,
                money: None,
            })
            .expect("build the backend");
            (backend, dir)
        }

        /// The active byte keyset of the mint at `mint_url`, as `payer` reads
        /// it to fund a channel there.
        pub(super) fn keyset_info(payer: &SpilmanChannels, mint_url: &str) -> String {
            let keyset_id = crate::market::active_keyset(mint_url, "byte").expect("keyset");
            payer
                .client
                .lock()
                .expect("not poisoned")
                .fetch_keyset_info(mint_url, &keyset_id)
                .expect("keyset info")
        }

        /// A token of `amount` vouchers, signed by the mint directly — the
        /// same thing the market does once it has been paid.
        pub(super) fn vouchers(
            mint: &Mint,
            mint_url: &str,
            keyset_info: &str,
            amount: u64,
        ) -> String {
            let info = parse_keyset_info_from_json(keyset_info).expect("keyset info");
            let mut amounts = info.amounts_largest_first.clone();
            amounts.reverse();
            let fees: FeeAndAmounts = (info.input_fee_ppk, amounts).into();
            let premint =
                PreMintSecrets::random(info.keyset_id, amount.into(), &SplitTarget::None, &fees)
                    .expect("blinded messages");
            let signatures = tokio::runtime::Handle::current()
                .block_on(mint.blind_sign(premint.blinded_messages()))
                .expect("blind sign");
            let proofs = cashu::dhke::construct_proofs(
                signatures,
                premint.rs(),
                premint.secrets(),
                &info.active_keys,
            )
            .expect("unblind");
            Token::new(
                MintUrl::from_str(mint_url).expect("mint url"),
                proofs,
                None,
                CurrencyUnit::Custom("byte".into()),
            )
            .to_string()
        }
    }

    /// A receiver with a real mint served over HTTP, and a payer holding its
    /// vouchers.
    struct Pair {
        mint: Arc<Mint>,
        mint_url: String,
        receiver: SpilmanChannels,
        receiver_key: PubKey,
        payer: SpilmanChannels,
        payer_key: PubKey,
        _wallets: [live::TempDir; 2],
    }

    impl Pair {
        async fn start() -> Self {
            let (mint, mint_url) = live::serve_mint(3).await;
            let accepted = vec![mint_url.clone()];
            let (receiver, receiver_wallet) =
                live::backend(&mint, &mint_url, accepted.clone(), live::PAYEE).await;
            let (payer, payer_wallet) =
                live::backend(&mint, &mint_url, accepted, live::PAYER).await;

            Self {
                mint,
                mint_url,
                receiver,
                receiver_key: live::pubkey(live::PAYEE),
                payer,
                payer_key: live::pubkey(live::PAYER),
                _wallets: [receiver_wallet, payer_wallet],
            }
        }

        /// Issue `amount` vouchers straight from the mint — what the market
        /// would sell — and open a channel on them that the receiver has
        /// verified. Runs on a blocking thread: every call here is.
        fn open_channel(&self, amount: u64) -> ChannelId {
            let keyset_info = live::keyset_info(&self.payer, &self.mint_url);
            let token = live::vouchers(&self.mint, &self.mint_url, &keyset_info, amount);

            let funded = self
                .payer
                .open(self.receiver_key, &self.mint_url, &token, &keyset_info)
                .expect("open a channel");
            let verified = self
                .receiver
                .verify(self.payer_key, &funded.funding)
                .expect("the receiver takes the funding");
            verified.channel_id
        }

        /// The balance the receiver would settle `channel_id` at.
        fn recorded(&self, channel_id: ChannelId) -> u64 {
            self.receiver
                .server
                .host()
                .get_balance(&channel_id_to_hex(channel_id))
                .map_or(0, |p| p.balance)
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_purchase_with_one_bad_signature_records_nothing_on_the_good_channel() {
        // A purchase spanning a rollover: two channels, one update each. The
        // host checks both before core sees the TopUp, and refuses it whole
        // when the second does not verify. Checking the first must not have
        // kept it, or the receiver would settle channel 1 at a state no grant
        // paid for.
        let pair = Pair::start().await;
        tokio::task::spawn_blocking(move || {
            // Different capacities: a channel id hashes its terms, whose
            // timestamps are whole seconds, so two identical channels opened
            // within one second would collide.
            let first = pair.open_channel(1_000);
            let second = pair.open_channel(1_001);
            let before = pair.recorded(first);

            let good = pair.payer.sign_update(first, 600).expect("sign");
            let mut bad = pair.payer.sign_update(second, 400).expect("sign");
            bad.0[10] ^= 0x01;

            assert!(
                pair.receiver
                    .verify_update(pair.payer_key, first, 600, good),
                "the first update is genuine"
            );
            assert!(
                !pair
                    .receiver
                    .verify_update(pair.payer_key, second, 400, bad),
                "the second is not"
            );
            assert_eq!(
                pair.recorded(first),
                before,
                "verifying the first update kept nothing"
            );

            // Once core accepts a purchase, recording is what moves it.
            pair.receiver
                .record_update(pair.payer_key, first, 600, good)
                .expect("record");
            assert_eq!(pair.recorded(first), 600);
        })
        .await
        .expect("the test ran");
    }

    /// Real funding, end to end: a byte mint on a loopback port, two backends,
    /// a channel opened by one and verified by the other through the blob.
    mod funded {
        use super::live::{PAYEE, PAYER, backend, keyset_info, pubkey, serve_mint, vouchers};
        use super::*;

        const GIB: u64 = 1 << 30;

        /// The blob as it was before: cdk-spilman's opening payment, as JSON.
        fn legacy_size(opening: &cdk_spilman::Payment) -> usize {
            serde_json::to_vec(&serde_json::json!({
                "channel_id": opening.channel_id,
                "balance": opening.balance,
                "signature": opening.signature,
                "params": opening.params,
                "funding_proofs": opening.funding_proofs,
            }))
            .expect("serialize")
            .len()
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn a_channel_funded_by_one_side_verifies_on_the_other() {
            let (mint, url) = serve_mint(7).await;
            let (payer, _payer_wallet) = backend(&mint, &url, vec![url.clone()], PAYER).await;
            let (payee, _payee_wallet) = backend(&mint, &url, vec![url.clone()], PAYEE).await;

            // Every backend call blocks, as it does when the node drives it.
            tokio::task::spawn_blocking(move || {
                let keyset_info = keyset_info(&payer, &url);

                // Exactly 1 GiB is a single proof; one byte short of it is
                // thirty, the most a channel that size can take.
                for capacity in [GIB, GIB - 1] {
                    let token = vouchers(&mint, &url, &keyset_info, capacity);
                    let funded = payer
                        .open(pubkey(PAYEE), &url, &token, &keyset_info)
                        .expect("fund");
                    assert_eq!(funded.capacity, capacity);

                    let opening = payer
                        .client
                        .lock()
                        .expect("not poisoned")
                        .sign_channel_registration(&channel_id_to_hex(funded.channel_id))
                        .expect("opening payment");
                    let proofs = opening.funding_proofs.as_ref().map_or(0, Vec::len);
                    let (old, new) = (legacy_size(&opening), funded.funding.len());
                    println!(
                        "capacity {capacity}: {proofs} proofs, JSON blob {old} bytes, CBOR blob {new} bytes"
                    );
                    assert!(new * 4 < old, "{new} bytes is not much smaller than {old}");

                    // A mint signature the payer cannot vouch for is refused.
                    let mut forged = funded.funding.clone();
                    let at = forged.len() - 40;
                    forged[at] ^= 1;
                    assert!(payee.verify(pubkey(PAYER), &forged).is_err());

                    // A mint we do not accept is refused as that, before its
                    // keyset is fetched from an address nothing answers on.
                    let mut elsewhere = Funding::decode(&funded.funding).expect("decode");
                    elsewhere.terms.mint = "http://127.0.0.1:9".into();
                    let refused = payee
                        .verify(pubkey(PAYER), &elsewhere.encode())
                        .expect_err("an unaccepted mint");
                    assert!(refused.downcast_ref::<MintNotAccepted>().is_some());

                    // Terms that would split the funding into more proofs than
                    // were sent are refused before any is derived.
                    let mut split = Funding::decode(&funded.funding).expect("decode");
                    split.terms.maximum_amount = 1;
                    assert!(payee.verify(pubkey(PAYER), &split.encode()).is_err());

                    let verified = payee
                        .verify(pubkey(PAYER), &funded.funding)
                        .expect("the payee accepts the funding");
                    assert_eq!(verified.channel_id, funded.channel_id);
                    assert_eq!(verified.capacity, capacity);
                    assert_eq!(verified.mint_url, url);
                }
            })
            .await
            .expect("funding");
        }
    }

    /// Real settlement, end to end: two byte mints on loopback ports, a payee
    /// running one and accepting both, and a payer funding a channel in each.
    mod settled {
        use super::live::{PAYEE, PAYER, backend, keyset_info, pubkey, serve_mint, vouchers};
        use super::*;

        const CAPACITY: u64 = 1 << 20;

        /// Fund a channel from `payer` to `payee` in the mint at `mint_url`,
        /// have the payee verify it, and pay `balance` over it.
        fn pay_over_channel(
            payer: &SpilmanChannels,
            payee: &SpilmanChannels,
            mint: &Mint,
            mint_url: &str,
            balance: u64,
        ) -> ChannelId {
            let keyset_info = keyset_info(payer, mint_url);
            let token = vouchers(mint, mint_url, &keyset_info, CAPACITY);
            let funded = payer
                .open(pubkey(PAYEE), mint_url, &token, &keyset_info)
                .expect("open");

            let verified = payee
                .verify(pubkey(PAYER), &funded.funding)
                .expect("verify");
            assert_eq!(verified.mint_url, mint_url);

            let signature = payer
                .sign_update(verified.channel_id, balance)
                .expect("sign");
            assert!(payee.verify_update(pubkey(PAYER), verified.channel_id, balance, signature));
            verified.channel_id
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn a_channel_settles_in_the_mint_it_was_funded_in() {
            let (ours, ours_url) = serve_mint(7).await;
            let (theirs, theirs_url) = serve_mint(8).await;
            let accepted = vec![ours_url.clone(), theirs_url.clone()];
            let (payer, _payer_wallet) =
                backend(&theirs, &theirs_url, accepted.clone(), PAYER).await;
            let (payee, _payee_wallet) = backend(&ours, &ours_url, accepted, PAYEE).await;

            // Every backend call blocks, as it does when the node drives it.
            tokio::task::spawn_blocking(move || {
                // Funded in another mint: only that mint can swap the proofs.
                let elsewhere = pay_over_channel(&payer, &payee, &theirs, &theirs_url, 1000);
                payee
                    .settle(elsewhere)
                    .expect("settle a channel funded in another mint");

                // Funded in our own: settled in process, as before.
                let at_home = pay_over_channel(&payer, &payee, &ours, &ours_url, 1000);
                payee
                    .settle(at_home)
                    .expect("settle a channel funded in our mint");
            })
            .await
            .expect("settling");
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn settling_a_channel_with_no_recorded_funding_is_an_error() {
            let (ours, ours_url) = serve_mint(7).await;
            let (payee, _payee_wallet) =
                backend(&ours, &ours_url, vec![ours_url.clone()], PAYEE).await;
            tokio::task::spawn_blocking(move || {
                assert!(payee.settle(ChannelId([9; 32])).is_err());
            })
            .await
            .expect("settling");
        }
    }

    #[test]
    fn only_a_close_no_retry_can_fix_is_permanent() {
        let permanent = [
            CloseError::unknown_channel(),
            CloseError::from_preparation_error(cdk_spilman::ClosePreparationError::not_found(
                "unknown",
            )),
            CloseError::from_preparation_error(cdk_spilman::ClosePreparationError::bad_request(
                "No payment",
            )),
            // Token already spent: the proofs are gone, whoever spent them.
            CloseError::mint_rejected(serde_json::json!({"code": 11001, "detail": "spent"})),
            CloseError::mint_rejected(serde_json::Value::String(
                r#"{"code":10002,"detail":"bad proof"}"#.into(),
            )),
        ];
        for e in &permanent {
            assert!(close_error_is_permanent(e), "{e} should be permanent");
        }

        let transient = [
            CloseError::mint_rejected(serde_json::Value::String("connection refused".into())),
            CloseError::mint_rejected_after_retry(
                serde_json::json!({"code": 12001}),
                serde_json::json!({"code": 12001}),
            ),
            CloseError::storage_failed("disk full"),
        ];
        for e in &transient {
            assert!(!close_error_is_permanent(e), "{e} should be retried");
        }
    }
}
