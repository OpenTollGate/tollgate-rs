//! HTTP + WebSocket transport server.
//!
//! Endpoints (port 4747 by default — see `docs/design/core/tollgate-protocol.md`):
//!   POST /tollgate/v1/exchange   — HTTP polling transport
//!   GET  /tollgate/v1/ws        — WebSocket upgrade

use axum::{Router, routing::get};

pub async fn serve(listen: &str) -> anyhow::Result<()> {
    let app = Router::new()
        .route("/tollgate/v1/exchange", axum::routing::post(http_exchange))
        .route("/tollgate/v1/ws", get(ws_upgrade));

    let listener = tokio::net::TcpListener::bind(listen).await?;
    tracing::info!(addr = %listener.local_addr()?, "listening");
    axum::serve(listener, app).await?;
    Ok(())
}

async fn http_exchange() -> &'static str {
    // TODO: decode length-prefixed CBOR frames, feed into Driver.
    "tollgate http exchange stub"
}

async fn ws_upgrade() -> &'static str {
    // TODO: upgrade to WebSocket, stream frames through Driver.
    "tollgate ws stub"
}
