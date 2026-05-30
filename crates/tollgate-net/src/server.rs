//! HTTP + WebSocket transport server.
//!
//! Endpoints (port 4747 by default — see `docs/design/core/tollgate-protocol.md`):
//!   POST /tollgate/v1/exchange   — HTTP polling (2-byte LE length-prefixed CBOR frames)
//!   GET  /tollgate/v1/ws        — WebSocket upgrade (one CBOR message per binary frame)

use std::net::SocketAddr;

use axum::Router;
use axum::extract::{ConnectInfo, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use bytes::Bytes;

use tollgate_protocol::{Announce, MessageType, decode_frames, encode_frame, peek_type};

use crate::driver::Driver;

pub async fn serve(listen: &str, driver: Driver) -> anyhow::Result<()> {
    let app = router(driver);
    let listener = tokio::net::TcpListener::bind(listen).await?;
    tracing::info!(addr = %listener.local_addr()?, "listening");
    // ConnectInfo gives each handler the peer's source address for firewall gating.
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await?;
    Ok(())
}

fn router(driver: Driver) -> Router {
    Router::new()
        .route("/tollgate/v1/exchange", axum::routing::post(http_exchange))
        .route("/tollgate/v1/ws", get(ws_upgrade))
        .with_state(driver)
}

/// HTTP polling transport. The request body is zero or more length-prefixed
/// CBOR frames. We establish the peer from its Announce (first message of a
/// session), route the rest through the driver, and return any queued response
/// frames in the same framing.
async fn http_exchange(
    State(driver): State<Driver>,
    extensions: axum::http::Extensions,
    body: Bytes,
) -> Response {
    let frames = match decode_frames(&body) {
        Ok(f) => f,
        Err(e) => {
            return (StatusCode::BAD_REQUEST, format!("bad framing: {e:?}")).into_response();
        }
    };

    // ConnectInfo is injected into request extensions by
    // `into_make_service_with_connect_info`; absent in tests using `oneshot`.
    let peer_ip = extensions
        .get::<ConnectInfo<SocketAddr>>()
        .map(|c| c.0.ip());

    // The Announce establishes the peer identity for this exchange. Without
    // transport-layer auth (the IP default), the pubkey comes from Announce.
    let mut peer_hex: Option<String> = None;

    for frame in frames {
        match peek_type(frame) {
            Some(MessageType::Announce) => match Announce::decode(frame) {
                Ok(announce) => {
                    let hex = hex::encode(announce.public_key().as_bytes());
                    driver.peer_connected(&hex, peer_ip).await;
                    peer_hex = Some(hex);
                }
                Err(e) => tracing::warn!(err = %e, "malformed Announce"),
            },
            Some(_) => match &peer_hex {
                Some(hex) => driver.message_received(hex, frame.to_vec()).await,
                None => tracing::warn!("message received before Announce; ignoring"),
            },
            None => tracing::warn!("unknown or malformed message; ignoring"),
        }
    }

    // Return any messages the driver queued for this peer during the exchange,
    // each as its own length-prefixed frame.
    let mut response = Vec::new();
    if let Some(hex) = &peer_hex {
        for message in driver.drain_outbox(hex).await {
            if encode_frame(&message, &mut response).is_err() {
                tracing::error!("queued message exceeds max frame length; dropping");
            }
        }
    }

    (StatusCode::OK, response).into_response()
}

/// WebSocket upgrade.
async fn ws_upgrade(State(_driver): State<Driver>) -> Response {
    // TODO: upgrade to a WebSocket, stream one CBOR message per binary frame
    // through the driver per connection.
    (
        StatusCode::NOT_IMPLEMENTED,
        "websocket transport not yet implemented",
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    use crate::adapter::IpAdapter;
    use crate::config::{Config, Identity};
    use crate::wallet::BootstrapWallet;

    fn test_driver() -> Driver {
        let identity = Arc::new(Identity::load_or_generate(&Config::default()).unwrap());
        Driver::new(BootstrapWallet::new(vec![]), IpAdapter::new(), identity)
    }

    #[tokio::test]
    async fn announce_establishes_peer_and_returns_ok() {
        let app = router(test_driver());

        let pk = tollgate_protocol::PublicKey::from_bytes([2u8; 33]);
        let announce = Announce::new(1, pk, "bytes", 0).encode();
        let body = tollgate_protocol::frame(&announce).unwrap();

        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/tollgate/v1/exchange")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn malformed_framing_is_rejected() {
        let app = router(test_driver());

        // A length prefix claiming 9 bytes but with no body.
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/tollgate/v1/exchange")
                    .body(Body::from(vec![0x09, 0x00]))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }
}
