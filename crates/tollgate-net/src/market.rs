//! Selling this node's vouchers.
//!
//! **Not part of the TollGate protocol.** No TollGate message buys, sells or
//! swaps a voucher; acquiring them happens before a session and outside it.
//! This is a separate endpoint under its own path, exactly as
//! `docs/design/market/market-protocol.md` describes, and a node that does not
//! serve it is still fully functional — its peers simply have to arrive holding
//! its vouchers already.
//!
//! # Why this cannot be a Lightning quote
//!
//! A mint normally issues against a paid bolt11 invoice. That does not work for
//! a byte-denominated keyset, and not because of a missing feature: **bolt11 is
//! denominated in msat, so there is no such thing as an invoice for 1024
//! bytes.** Quoting bytes in money is the market's job — it means pricing this
//! issuer's capacity against sats, which is the layer the design deliberately
//! leaves out of the protocol and which has no implementation yet.
//!
//! So the issuer signs directly instead of going through a payment method.
//!
//! # What is simulated
//!
//! **This gives vouchers away.** There is no payment, no quote and no
//! authentication: ask and receive. That is the market being stubbed, and it is
//! the one piece of this node that would be irresponsible to run facing anyone
//! you do not trust. What it stands in for is a real acquisition route — a
//! Lightning payment priced by a market, a direct sale, a swap against another
//! mint's paper.
//!
//! Everything downstream of it is real: the proofs it returns are ordinary
//! blind signatures from a real keyset, they fund real channels, and they land
//! in a real spent-proof set.

use std::sync::Arc;

use anyhow::Result;
use axum::extract::State;
use axum::routing::post;
use axum::{Json, Router};
use cdk::mint::Mint;
use cdk::nuts::{BlindSignature, BlindedMessage};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

/// Where the market lives, relative to the node's mint URL.
pub const ISSUE_PATH: &str = "/market/v1/issue";

/// Blinded outputs a buyer wants signed.
#[derive(Debug, Deserialize)]
pub struct IssueRequest {
    /// The blinded messages, already split into powers of two by the buyer.
    pub outputs: Vec<BlindedMessage>,
}

/// The signatures over them.
#[derive(Debug, Serialize)]
pub struct IssueResponse {
    /// One per output, in the same order.
    pub signatures: Vec<BlindSignature>,
}

/// The market router, to be mounted alongside the mint.
pub fn router(mint: Arc<Mint>) -> Router {
    Router::new()
        .route(ISSUE_PATH, post(issue))
        .with_state(mint)
}

/// Issue vouchers against this node's capacity.
///
/// Issuing is selling capacity before delivering it — a rooftop antenna paying
/// for itself against next month's bytes. What the node charges for that is the
/// operator's business and happens somewhere else; here it is free.
async fn issue(
    State(mint): State<Arc<Mint>>,
    Json(request): Json<IssueRequest>,
) -> Result<Json<IssueResponse>, (axum::http::StatusCode, String)> {
    let wanted: u64 = request
        .outputs
        .iter()
        .map(|o| u64::from(o.amount))
        .fold(0, u64::saturating_add);

    match mint.blind_sign(request.outputs).await {
        Ok(signatures) => {
            info!(units = wanted, "issued vouchers against our own capacity");
            Ok(Json(IssueResponse { signatures }))
        }
        Err(e) => {
            warn!(units = wanted, error = %e, "could not issue vouchers");
            Err((axum::http::StatusCode::BAD_REQUEST, e.to_string()))
        }
    }
}
