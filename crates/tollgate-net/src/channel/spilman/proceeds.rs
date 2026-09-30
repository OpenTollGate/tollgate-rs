//! What a settled channel leaves behind, and getting rid of it.
//!
//! `tollgated` keeps nothing a settlement brings in
//! (`docs/design/core/tollgate-daemons.md`): the receiver's share is burned at
//! its mint or handed to `merchantd`, and so is a funder's change. This is the
//! Cashu side of that, spoken to the mint over its standard API.

use std::collections::HashMap;
use std::str::FromStr;

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use cashu::mint_url::MintUrl;
use cashu::nuts::nut00::ProofsMethods;
use cashu::nuts::{
    CheckStateRequest, CheckStateResponse, CurrencyUnit, MeltQuoteCustomRequest, MeltRequest,
    Proofs, RestoreRequest, RestoreResponse, SecretKey, State, SwapRequest, SwapResponse, Token,
};
use cdk::wallet::{HttpClient, MintConnector};
use cdk_common::MeltQuoteState;
use cdk_common::melt::{MeltQuoteCreateResponse, MeltQuoteRequest};
use cdk_spilman::{ClientChannelFunding, EstablishedChannel, MintConnection};

use crate::mint::{BURN, burn_method};

/// A token of `proofs`, issued by `mint`, to hand to `merchantd`.
pub(super) fn token(mint: &str, unit: &CurrencyUnit, proofs: Proofs) -> Result<String> {
    let mint = MintUrl::from_str(mint).map_err(|e| anyhow!("{mint} is not a mint URL: {e}"))?;
    Ok(Token::new(mint, proofs, None, unit.clone()).to_string())
}

fn client(url: &str) -> Result<HttpClient> {
    let url = MintUrl::from_str(url).map_err(|e| anyhow!("{url} is not a mint URL: {e}"))?;
    Ok(HttpClient::new(url, None))
}

/// Burn `proofs` at the mint at `url`, with its [`BURN`] melt method.
///
/// Returns `false`, having done nothing, if the mint offers no burn in `unit`
/// or its input fee would take all of them: what is left is then dropped,
/// which cancels nothing at the issuer but costs this node nothing either.
pub(super) async fn burn(url: &str, unit: &CurrencyUnit, proofs: Proofs) -> Result<bool> {
    let client = client(url)?;
    let info = client
        .get_mint_info()
        .await
        .map_err(|e| anyhow!("read what {url} offers: {e}"))?;
    if info.nuts.nut05.get_settings(unit, &burn_method()).is_none() {
        return Ok(false);
    }

    // What the melt may cover is what the inputs are worth after the mint's
    // input fee, which a mintd does not charge but another mint may.
    let fees: HashMap<_, _> = client
        .get_mint_keysets()
        .await
        .map_err(|e| anyhow!("read {url}'s keysets: {e}"))?
        .keysets
        .into_iter()
        .map(|k| (k.id, k.input_fee_ppk))
        .collect();
    let ppk: u64 = proofs
        .iter()
        .map(|p| fees.get(&p.keyset_id).copied().unwrap_or(0))
        .sum();
    let total = u64::from(proofs.total_amount()?);
    let amount = total.saturating_sub(ppk.div_ceil(1000));
    if amount == 0 {
        return Ok(false);
    }

    let quote = client
        .post_melt_quote(MeltQuoteRequest::Custom(MeltQuoteCustomRequest {
            method: BURN.into(),
            request: amount.to_string(),
            unit: unit.clone(),
            amount: None,
            extra: serde_json::Value::Null,
        }))
        .await
        .map_err(|e| anyhow!("ask {url} to burn {amount} {unit}: {e}"))?;
    let MeltQuoteCreateResponse::Custom((_, quote)) = quote else {
        bail!("{url} answered a burn quote with some other kind");
    };

    match client
        .post_melt(&burn_method(), MeltRequest::new(quote.quote, proofs, None))
        .await
    {
        Ok(melted) if melted.state() == MeltQuoteState::Paid => Ok(true),
        Ok(melted) => bail!("{url} left a burn {}", melted.state()),
        // An earlier attempt went through and its answer was lost: they are
        // gone either way.
        Err(cdk::Error::TokenAlreadySpent) => Ok(true),
        Err(e) => Err(anyhow!("burn {amount} {unit} at {url}: {e}")),
    }
}

/// Take back the change from a channel this node funded, from the mint at
/// `url`.
///
/// The peer's close pays our share into outputs derived from the channel
/// secret, so they are found by asking the mint to restore them (NUT-09) and
/// come back signed for, ready to spend. `None` if the peer has not closed the
/// channel yet: the funding is still unspent.
pub(super) async fn reclaim_change(
    url: &str,
    funding: &ClientChannelFunding,
    ours: SecretKey,
) -> Result<Option<Proofs>> {
    let mint = Mint(client(url)?);
    let change =
        EstablishedChannel::restore_sender_proofs_from_client_funding(funding, ours, &mint)
            .await
            .with_context(|| format!("restore the change at {url}"))?;
    if !change.is_empty() {
        return Ok(Some(change));
    }

    // Nothing to restore is either nothing left over or no close yet, and
    // only the funding's state tells them apart.
    let proofs: Proofs =
        serde_json::from_str(&funding.funding_proofs_json).context("read the funding proofs")?;
    let states = mint.check_state(proofs.ys()?).await?;
    let closed = states.states.iter().all(|s| s.state == State::Spent);
    Ok(closed.then(Proofs::new))
}

/// A mint as cdk-spilman's restore reaches it.
struct Mint(HttpClient);

#[async_trait]
impl MintConnection for Mint {
    async fn process_swap(&self, request: SwapRequest) -> Result<SwapResponse> {
        Ok(self.0.post_swap(request).await?)
    }

    async fn post_restore(&self, request: RestoreRequest) -> Result<RestoreResponse> {
        Ok(self.0.post_restore(request).await?)
    }

    async fn check_state(&self, ys: Vec<cashu::nuts::PublicKey>) -> Result<CheckStateResponse> {
        Ok(self.0.post_check_state(CheckStateRequest { ys }).await?)
    }
}
