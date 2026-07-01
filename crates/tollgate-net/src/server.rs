//! HTTP + WebSocket transport server.
//!
//! Endpoints (port 4747 by default — see `docs/design/core/tollgate-protocol.md`):
//!   POST /tollgate/v1/exchange   — HTTP polling (2-byte LE length-prefixed CBOR frames)
//!   GET  /tollgate/v1/ws        — WebSocket upgrade (one CBOR message per binary frame)
//!   GET  /portal[/...]          — captive-portal SPA (static files + index.html fallback).
//!                                 Served to ALL clients regardless of auth/firewall state,
//!                                 so a paid user can still reach the portal (nodogsplash
//!                                 only serves the splash to unauthenticated clients).
//!                                 Enabled via `Config::portal_dir`; absent by default.

use std::net::SocketAddr;
use std::path::PathBuf;

use axum::Router;
use axum::extract::{ConnectInfo, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use bytes::Bytes;

use tollgate_protocol::{Announce, MessageType, decode_frames, encode_frame, peek_type};

use crate::driver::Driver;

pub async fn serve(
    listen: &str,
    driver: Driver,
    portal_dir: Option<PathBuf>,
) -> anyhow::Result<()> {
    let app = router(driver, portal_dir);
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

fn router(driver: Driver, portal_dir: Option<PathBuf>) -> Router {
    let mut app = Router::new()
        .route("/tollgate/v1/exchange", axum::routing::post(http_exchange))
        .route("/tollgate/v1/ws", get(ws_upgrade));

    // Persistent captive-portal UI. nodogsplash only serves the splash page to
    // UNAUTHENTICATED clients, so once a peer pays and is let through the
    // firewall it can no longer reach the portal. Mounting the built SPA at
    // GET /portal on this server — reachable regardless of firewall/auth state —
    // fixes that. `ServeDir` serves the static assets; `.fallback(ServeFile)`
    // serves index.html for unknown sub-paths so client-side routes (e.g.
    // /portal/balance) resolve to the SPA entry with HTTP 200 — the client-side
    // router then takes over. Note we use `fallback`, NOT `not_found_service`:
    // the latter wraps the inner service in `SetStatus(NOT_FOUND)`, so it would
    // serve index.html but report 404 — correct for a "pretty 404 page" but
    // wrong for SPA routing, where the entry must be 200 so the router boots.
    // Disabled when `portal_dir` is unset (the default).
    if let Some(dir) = portal_dir.as_ref() {
        let spa = tower_http::services::ServeDir::new(dir)
            .fallback(tower_http::services::ServeFile::new(dir.join("index.html")));
        app = app.nest_service("/portal", spa);
    }

    app.with_state(driver)
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
                    // The HTTP transport re-sends Announce on every poll, so only the
                    // first (a genuinely new peer) is logged at INFO; the keep-alive
                    // repeats drop to DEBUG to keep the log readable.
                    if driver.peer_connected(&hex, peer_ip).await {
                        tracing::info!(
                            peer = %hex,
                            version = announce.version,
                            unit = %announce.unit,
                            ip = ?peer_ip,
                            "peer announced"
                        );
                    } else {
                        tracing::debug!(peer = %hex, ip = ?peer_ip, "peer re-announced");
                    }
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
        Driver::new(
            BootstrapWallet::new(vec![]),
            IpAdapter::new(),
            identity,
            tollgate_core::Price::default(),
            "bytes",
            Vec::new(),
        )
    }

    #[tokio::test]
    async fn announce_establishes_peer_and_returns_ok() {
        let app = router(test_driver(), None);

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
        let app = router(test_driver(), None);

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

    /// Build a throwaway portal fixture dir with an index.html (SPA entry) and
    /// one CSS asset, so the /portal tests exercise ServeDir + SPA fallback
    /// without depending on an externally-built frontend.
    fn portal_fixture() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "tollgate-portal-test-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("index.html"),
            b"<!doctype html><html><head><title>TollGate Portal</title></head><body><div id=\"root\"></div></body></html>",
        )
        .unwrap();
        std::fs::write(dir.join("style.css"), b"body{color:#000}").unwrap();
        dir
    }

    #[tokio::test]
    async fn portal_endpoint_serves_spa_index_and_fallback() {
        let dir = portal_fixture();
        let app = router(test_driver(), Some(dir.clone()));

        // GET /portal returns the SPA entry (200 + text/html).
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/portal")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let ct = resp
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .expect("content-type header");
        assert!(
            ct.to_str().unwrap().contains("text/html"),
            "expected text/html, got {ct:?}"
        );
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(
            std::str::from_utf8(&body).unwrap().contains("<html"),
            "body is not the SPA html: {}",
            String::from_utf8_lossy(&body)
        );

        // A static asset under /portal/ is served from the same root.
        let asset = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/portal/style.css")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(asset.status(), StatusCode::OK);

        // SPA fallback: an unknown client-side route still returns index.html
        // (this is what lets React Router handle /portal/balance etc.).
        let deep = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/portal/balance/overview")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(deep.status(), StatusCode::OK);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
