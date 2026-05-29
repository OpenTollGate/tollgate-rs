//! HTTP + WebSocket transport server.
//!
//! Endpoints (port 4747 by default — see `docs/design/core/tollgate-protocol.md`):
//!   POST /tollgate/v1/exchange   — HTTP polling (2-byte LE length-prefixed CBOR frames)
//!   GET  /tollgate/v1/ws        — WebSocket upgrade (one CBOR message per binary frame)

use axum::{Router, extract::State, response::IntoResponse, routing::get};
use bytes::Bytes;

use crate::driver::Driver;

pub async fn serve(listen: &str, driver: Driver) -> anyhow::Result<()> {
    let app = Router::new()
        .route("/tollgate/v1/exchange", axum::routing::post(http_exchange))
        .route("/tollgate/v1/ws", get(ws_upgrade))
        .with_state(driver);

    let listener = tokio::net::TcpListener::bind(listen).await?;
    tracing::info!(addr = %listener.local_addr()?, "listening");
    axum::serve(listener, app).await?;
    Ok(())
}

/// HTTP polling transport.
///
/// The request body is a sequence of length-prefixed CBOR frames (2-byte LE
/// length + CBOR bytes). We decode them, push through the driver, and return
/// any queued response frames in the same format.
///
/// Peer identity: the Announce message (0x00) inside the first frame carries
/// the sender's pubkey. Until that's decoded, we use the raw bytes as a
/// temporary handle. Full peer tracking requires Announce parsing (next step).
async fn http_exchange(
    State(driver): State<Driver>,
    body: Bytes,
) -> impl IntoResponse {
    // TODO: decode length-prefixed frames, route through driver, return response frames.
    // For now: echo the received length back for smoke-testing.
    tracing::debug!(bytes = body.len(), "http_exchange (stub)");
    format!("received {} bytes", body.len())
}

/// WebSocket upgrade.
async fn ws_upgrade(State(_driver): State<Driver>) -> impl IntoResponse {
    // TODO: upgrade to WebSocket, stream frames through driver per connection.
    tracing::debug!("ws_upgrade (stub)");
    "websocket upgrade stub"
}
