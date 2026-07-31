//! Real Cashu Spilman channels, funded against a real mint.
//!
//! This is the backend that puts value at stake. A channel is a 2-of-2 multisig
//! Cashu token; a ratchet turn is a signed balance update the receiver can take
//! to the mint; settlement is a swap.
//!
//! # No prices here
//!
//! The receiver is configured with an **empty pricing table**, so the Spilman
//! layer's "amount due" is always zero and it enforces only the ratchet and the
//! signature. That is deliberate and it is the whole fit: delivery has no price
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
//! it came to hold sats. [`acquire`] is the direct-from-the-issuer route, which
//! is the only one needed to operate.
//!
//! [`tollgate-vouchers.md`]: https://github.com/OpenTollGate/tollgate-rs/blob/master/docs/design/core/tollgate-vouchers.md

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow};
use cdk::mint::Mint;
use cdk_spilman::configurable_host::{ConfigurableHost, ConfigurableHostConfig, StorageConfig};
use cdk_spilman::{
    ConfigurableClientHost, MemoryClientStorage, ReqwestClientNetworking, SpilmanBridge,
    SpilmanClientBridge, SpilmanNetworking,
};
use tollgate_protocol::{ChannelId, PubKey, Signature};

use super::{ChannelBackend, FundedChannel, VerifiedChannel};

/// How long a funded channel stays refundable to the payer.
const CHANNEL_TTL_SECONDS: u64 = 3_600;

/// Largest single proof amount used when funding. Bigger amounts mean fewer
/// proofs, and proof count is what the funding blob and the spent-proof set
/// both scale with.
const MAX_PROOF_AMOUNT: u64 = 8_192;

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
}

type Client =
    SpilmanClientBridge<ConfigurableClientHost<MemoryClientStorage>, ReqwestClientNetworking>;

/// Cashu Spilman channels.
pub struct SpilmanChannels {
    config: SpilmanConfig,
    /// Our side of channels we fund to pay peers.
    ///
    /// Behind a mutex because the client host keeps its storage in a `RefCell`
    /// — `Send` but not `Sync`, so the lock is what makes the backend shareable
    /// rather than a concession to contention.
    client: Mutex<Client>,
    /// Our side of channels peers fund to pay us.
    server: SpilmanBridge<ConfigurableHost>,
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
        let client = SpilmanClientBridge::new(client_host, ReqwestClientNetworking::new());

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
                min_expiry_seconds: CHANNEL_TTL_SECONDS,
                pricing_scale: 1,
                storage: StorageConfig::Memory,
                // Empty on purpose: with no pricing entry for the unit the
                // amount due is always zero, so this layer enforces the ratchet
                // and the signature and nothing else. What a peer may draw is
                // core's decision, against the grant.
                pricing: HashMap::new(),
            },
            &config.secret_key_hex,
        )
        .map_err(|e| anyhow!("build the channel receiver: {e}"))?;

        Ok(Self {
            config,
            client: Mutex::new(client),
            server: SpilmanBridge::new(host),
        })
    }

    /// The mint URL this node advertises.
    pub fn mint_url(&self) -> &str {
        &self.config.mint_url
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

/// What travels in `Accept` and `RolloverInit`.
///
/// Spilman carries funding with the first payment; the TollGate handshake
/// confirms a channel before any grant, so the opening payment is pulled out
/// and sent here instead. Its balance is zero — it buys nothing, it only opens.
#[derive(serde::Serialize, serde::Deserialize)]
struct FundingBlob {
    channel_id: String,
    balance: u64,
    signature: String,
    params: serde_json::Value,
    funding_proofs: serde_json::Value,
}

impl ChannelBackend for SpilmanChannels {
    fn fund(&self, peer: PubKey, mint_url: &str, capacity: u64) -> Result<FundedChannel> {
        let keyset_info = self
            .client
            .lock()
            .expect("not poisoned")
            .fetch_keyset_info(mint_url, "")
            .map_err(|e| anyhow!("could not read keysets from {mint_url}: {e}"))?;

        // Acquiring the peer's vouchers is outside the protocol; a peer arrives
        // holding them or it gets no service.
        let token = acquire(mint_url, capacity, &self.config.unit, &keyset_info)
            .with_context(|| format!("acquire {capacity} {} from {mint_url}", self.config.unit))?;

        let ours = cashu::nuts::SecretKey::from_hex(&self.config.secret_key_hex)
            .map_err(|e| anyhow!("bad secret: {e}"))?
            .public_key()
            .to_hex();

        let client = self.client.lock().expect("not poisoned");
        let opened = client
            .open_channel_from_token(
                &token,
                &hex::encode(peer.0),
                &ours,
                now_seconds() + CHANNEL_TTL_SECONDS,
                &keyset_info,
                MAX_PROOF_AMOUNT,
            )
            .map_err(|e| anyhow!("open a channel against {mint_url}: {e}"))?;

        let opening = client
            .create_payment_with_funding(&opened.channel_id, 0)
            .map_err(|e| anyhow!("prepare funding for {}: {e}", opened.channel_id))?;

        let blob = FundingBlob {
            channel_id: opened.channel_id.clone(),
            balance: opening.balance,
            signature: opening.signature.clone(),
            params: opening
                .params
                .clone()
                .ok_or_else(|| anyhow!("the opening payment carried no channel parameters"))?,
            funding_proofs: serde_json::to_value(
                opening
                    .funding_proofs
                    .as_deref()
                    .ok_or_else(|| anyhow!("the opening payment carried no proofs"))?,
            )?,
        };

        Ok(FundedChannel {
            channel_id: channel_id_from_hex(&opened.channel_id)?,
            capacity: opened.capacity,
            funding: serde_json::to_vec(&blob)?,
        })
    }

    fn verify(&self, _peer: PubKey, funding: &[u8]) -> Result<VerifiedChannel> {
        let blob: FundingBlob =
            serde_json::from_slice(funding).context("funding blob is not the expected shape")?;
        let proofs: Vec<cashu::nuts::Proof> = serde_json::from_value(blob.funding_proofs)
            .context("funding blob carried unreadable proofs")?;

        // This is where the money is checked: locked to the two of us, against
        // a mint and keyset we accept, and not already spent.
        let funded = self
            .server
            .fund_channel(
                &blob.channel_id,
                blob.balance,
                &blob.signature,
                Some(&blob.params),
                Some(&proofs),
            )
            .map_err(|e| anyhow!("peer funding did not verify: {e:?}"))?;

        Ok(VerifiedChannel {
            channel_id: channel_id_from_hex(&blob.channel_id)?,
            capacity: funded.capacity,
        })
    }

    fn sign_update(&self, channel_id: ChannelId, cumulative: u64) -> Result<Signature> {
        let payment = self
            .client
            .lock()
            .expect("not poisoned")
            .create_payment(&channel_id_to_hex(channel_id), cumulative)
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
        // Checks the signature against the channel's key material and that the
        // balance only ever increases. With no pricing configured it never
        // second-guesses how much is owed — that is core's job.
        self.server
            .process_payment(
                &channel_id_to_hex(channel_id),
                cumulative,
                &hex::encode(signature.0),
                None,
                None,
                &String::new(),
            )
            .is_ok()
    }

    fn settle(&self, channel_id: ChannelId) -> Result<()> {
        let id = channel_id_to_hex(channel_id);
        self.server
            .execute_unilateral_close(&id, &MintNetworking::new(Arc::clone(&self.config.mint)))
            .map_err(|e| anyhow!("settle {id}: {e:?}"))?;
        Ok(())
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

impl SpilmanNetworking for MintNetworking {
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

    fn refresh_all_keysets(&self, _mint: &str) -> Result<(), String> {
        // Ours, and always current.
        Ok(())
    }
}

/// Acquire `amount` units of a mint's vouchers.
///
/// **Outside the protocol.** This is the direct-from-the-issuer route from the
/// market documents. Against a mint whose payment backend auto-pays its own
/// quotes it costs nothing, which is exactly why it lives behind a feature flag.
fn acquire(mint_url: &str, amount: u64, unit: &str, keyset_info: &str) -> Result<String> {
    let http = |method: &str, url: &str, body: &str| -> Result<String, String> {
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
    };

    let proofs = cdk_spilman::mint_proofs_from_mint(mint_url, amount, keyset_info, &http)
        .map_err(|e| anyhow!("mint {amount} from {mint_url}: {e}"))?;

    cdk_spilman::build_cashu_b_token(mint_url, unit, &proofs)
        .map_err(|e| anyhow!("build a token: {e}"))
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
    fn a_signature_of_the_wrong_length_is_rejected() {
        assert!(signature_from_hex(&hex::encode([0u8; 64])).is_ok());
        assert!(signature_from_hex(&hex::encode([0u8; 32])).is_err());
    }
}
