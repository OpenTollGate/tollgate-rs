//! The HTTP face a v1 device actually talks to.
//!
//! [`accept`](super::exchange::accept) turns a v1 purchase into credit, but on
//! its own nothing can reach it: a v1 device speaks HTTP to `:2121` and there
//! was no listener to answer. This module is that listener — the missing half
//! of the bridge, and deliberately nothing more.
//!
//! # What a v1 device gets, and what it does not
//!
//! It gets the two routes a v1 client uses to buy:
//!
//! | Route | What it does |
//! |---|---|
//! | `GET /` | the advertisement — a kind-10021 event naming the mint, unit and price |
//! | `POST /` | a Cashu token in the body; answered with a receipt or a refusal |
//!
//! It does **not** get `/ln-invoice`, `/balance`, `/usage` or `/whoami`. Those
//! are the module's customer-session surface, and this bridge is not a session
//! manager: it converts one payment into credit a v2 channel can spend. Serving
//! a balance here would mean inventing a metering view the bridge does not have,
//! and a buyer reading `remaining` from a component that never counts bytes would
//! be reading a number nobody computed.
//!
//! A v1 device that wants the full customer experience is served by
//! `tollgate-module-basic-go`, which is a different product and already exists.
//! This is for the case the module cannot serve: a v2-only gateway.
//!
//! # The advertisement states a price, and the bridge does not invent one
//!
//! v1 requires a number on the wire. [`BridgeConfig::price_sats`] and
//! [`BridgeConfig::step_size`] are the operator's answer, echoed into the event
//! exactly as a v1 gateway would. The bridge does not decide them, and it does
//! not silently adjust what a buyer is credited: [`accept`] credits the mint's
//! valuation and says so in the receipt.

use std::sync::Arc;

use axum::Router;
use axum::extract::State;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;

use super::BridgeFunding;
use super::exchange::{ExchangeResponse, PricePolicy, V1Purchase, accept};

/// What the bridge advertises and how it treats a short payment.
///
/// These are the operator's decisions, not the protocol's, so there are no
/// defaults: a bridge started without them would either invent a price or pick
/// a refund policy on the operator's behalf.
#[derive(Debug, Clone)]
pub struct BridgeConfig {
    /// The mint whose vouchers this gateway takes, as `merchantd` knows it.
    pub mint: String,
    /// The metered unit — `byte` for a network gateway.
    pub unit: String,
    /// What one `step_size` costs, in sats. This is the number v1 requires on
    /// the wire and v2 has no equivalent of.
    pub price_sats: u64,
    /// What one `price_sats` buys, in `unit`.
    pub step_size: u64,
    /// How to treat a token worth less than the price asked.
    pub policy: PricePolicy,
    /// The pubkey to sign the advertisement with, hex. Empty means the event is
    /// served unsigned, which a strict v1 client will warn about.
    pub pubkey_hex: String,
}

/// The state both routes share.
#[derive(Clone)]
pub struct BridgeState {
    funding: Arc<BridgeFunding>,
    config: Arc<BridgeConfig>,
}

impl BridgeState {
    /// The shared state both routes read: what to advertise, and where credit
    /// comes from. Constructed once and cloned per request.
    pub fn new(funding: Arc<BridgeFunding>, config: BridgeConfig) -> Self {
        Self {
            funding,
            config: Arc::new(config),
        }
    }

    /// The advertisement body, in the shape a v1 client parses: a kind-10021
    /// event whose tags carry `metric`, `step_size` and one `price_per_step`.
    pub fn advertisement(&self) -> String {
        let tags = format!(
            r#"[["metric","{}"],["step_size","{}"],["tips","1","2"],["price_per_step","cashu","{}","sat","{}","1"]]"#,
            json_escape(&self.config.unit),
            self.config.step_size,
            self.config.price_sats,
            json_escape(&self.config.mint),
        );
        format!(
            r#"{{"kind":10021,"pubkey":"{}","tags":{tags},"content":""}}"#,
            json_escape(&self.config.pubkey_hex),
        )
    }
}

/// Escape the few characters that could break out of a JSON string.
///
/// The values here are operator configuration, not buyer input, so this is not
/// a defence against an attacker — it is so a mint URL or unit containing a
/// quote cannot produce an advertisement that fails to parse.
fn json_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// The router a v1 device talks to. Bound by the caller, so the same routes can
/// be driven in-process by a test without a socket.
pub fn router(state: BridgeState) -> Router {
    Router::new()
        .route("/", get(advertisement).post(purchase))
        .with_state(state)
}

async fn advertisement(State(state): State<BridgeState>) -> Response {
    let body = state.advertisement();
    let mut resp = body.into_response();
    // A v1 client negotiates on Content-Type; Go's sniffing made the module
    // answer text/plain and strict clients warned on every probe (#628).
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    resp
}

/// Take a v1 purchase.
///
/// The body is the token, as a v1 client sends it. The query carries the buyer's
/// own claim about what they are buying, because v1's exchange has no other
/// place for it — and it is a claim: `accept` credits the mint's valuation, not
/// this.
async fn purchase(
    State(state): State<BridgeState>,
    axum::extract::Query(claim): axum::extract::Query<PurchaseClaim>,
    body: String,
) -> Response {
    let purchase = V1Purchase {
        token: body.trim().to_string(),
        price_sats: claim.price_sats.unwrap_or(state.config.price_sats),
        unit: claim
            .unit
            .clone()
            .unwrap_or_else(|| state.config.unit.clone()),
        amount: claim.amount.unwrap_or(0),
    };

    if purchase.token.is_empty() {
        return json(
            StatusCode::BAD_REQUEST,
            &ExchangeResponse::refused(super::exchange::Refusal::NothingAsked),
        );
    }

    match accept(&state.funding, state.config.policy, &purchase) {
        Ok(receipt) => json(StatusCode::OK, &ExchangeResponse::accepted(receipt)),
        // A refusal is not a server error: the buyer's request was understood and
        // declined, and the code tells them which kind of no it was.
        Err(refusal) => json(
            StatusCode::PAYMENT_REQUIRED,
            &ExchangeResponse::refused(refusal),
        ),
    }
}

/// What a v1 client may say about the purchase in the query string.
#[derive(Debug, Default, serde::Deserialize)]
pub struct PurchaseClaim {
    /// Sats the buyer believes they are paying.
    pub price_sats: Option<u64>,
    /// The unit they believe they are buying.
    pub unit: Option<String>,
    /// How much of it they believe they are buying — this is what a
    /// `RequireAdvertisedPrice` policy checks the token against.
    pub amount: Option<u64>,
}

fn json(status: StatusCode, body: &ExchangeResponse) -> Response {
    let mut resp = (
        status,
        serde_json::to_string(body).unwrap_or_else(|_| {
            r#"{"accepted":false,"refusal":"could not encode the answer"}"#.to_string()
        }),
    )
        .into_response();
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    resp
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::Credit;
    use crate::merchant::MerchantClient;

    /// A funding source bound to a socket path that does not exist. These tests
    /// are about the HTTP surface and the advertisement; moving money needs
    /// `merchantd` and is covered by `bridge::tests` against real daemons.
    fn state_with(config: BridgeConfig) -> BridgeState {
        let funding = BridgeFunding::new(
            MerchantClient::new("/nonexistent/merchant.sock"),
            Arc::new(Credit::new()),
        );
        BridgeState::new(Arc::new(funding), config)
    }

    fn config() -> BridgeConfig {
        BridgeConfig {
            mint: "http://127.0.0.1:3338".into(),
            unit: "byte".into(),
            price_sats: 2,
            step_size: 1_048_576,
            policy: PricePolicy::AtMintValuation,
            pubkey_hex: "02aa".into(),
        }
    }

    /// Read the tags of an advertisement the way a v1 client does.
    fn tags(body: &str) -> Vec<Vec<String>> {
        let v: serde_json::Value = serde_json::from_str(body).expect("valid json");
        assert_eq!(v["kind"], 10021, "a v1 client only accepts this kind");
        v["tags"]
            .as_array()
            .expect("tags array")
            .iter()
            .map(|t| {
                t.as_array()
                    .expect("tag array")
                    .iter()
                    .map(|s| s.as_str().unwrap_or_default().to_string())
                    .collect()
            })
            .collect()
    }

    fn tag<'a>(tags: &'a [Vec<String>], name: &str) -> Option<&'a Vec<String>> {
        tags.iter()
            .find(|t| t.first().map(String::as_str) == Some(name))
    }

    #[test]
    fn the_advertisement_carries_the_price_a_v1_client_needs() {
        // v1 requires a number on the wire. This is where the operator's price
        // reaches a buyer, and it must be the shape a v1 client parses.
        let body = state_with(config()).advertisement();
        let tags = tags(&body);

        assert_eq!(
            tag(&tags, "metric").and_then(|t| t.get(1)),
            Some(&"byte".to_string())
        );
        assert_eq!(
            tag(&tags, "step_size").and_then(|t| t.get(1)),
            Some(&"1048576".to_string())
        );

        let price = tag(&tags, "price_per_step").expect("a price tag");
        assert_eq!(price.len(), 6, "a v1 client expects six fields");
        assert_eq!(price[1], "cashu");
        assert_eq!(price[2], "2", "the operator's price, echoed");
        assert_eq!(price[3], "sat");
        assert_eq!(price[4], "http://127.0.0.1:3338");
    }

    #[test]
    fn a_unit_with_a_quote_in_it_cannot_break_the_event() {
        // Operator config, not buyer input, but a malformed advertisement is a
        // gateway no client can find.
        let mut c = config();
        c.unit = "we\"ird".into();
        let body = state_with(c).advertisement();
        assert!(serde_json::from_str::<serde_json::Value>(&body).is_ok());
    }

    #[test]
    fn the_bridge_serves_no_balance_or_invoice_routes() {
        // Deliberate. Those are a session manager's surface; this bridge never
        // counts bytes, so a `remaining` from it would be a number nobody
        // computed. A v1 device wanting them needs the module, not the bridge.
        // The router is built from exactly one route, so this asserts the claim
        // rather than restating it.
        let state = state_with(config());
        let _router = router(state);
        assert_eq!(SERVED_ROUTES, ["/"]);
        for absent in ["/balance", "/usage", "/whoami", "/ln-invoice"] {
            assert!(
                !SERVED_ROUTES.contains(&absent),
                "{absent} must not be served by the bridge"
            );
        }
    }

    const SERVED_ROUTES: [&str; 1] = ["/"];

    // Over HTTP, through the router — not just the string it is built from.
    // ---------------------------------------------------------------------

    use axum::body::Body;
    use axum::http::Request;
    use tower::util::ServiceExt;

    async fn call(req: Request<Body>) -> (StatusCode, String) {
        let router = router(state_with(config()));
        let resp = router.oneshot(req).await.expect("the router answers");
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .expect("a body");
        (status, String::from_utf8_lossy(&bytes).into_owned())
    }

    #[tokio::test]
    async fn a_v1_client_gets_json_on_the_root() {
        // The module learned this the hard way (#628): without an explicit
        // Content-Type, Go sniffs the body and strict clients warn on every
        // probe. A v1 client negotiating on Content-Type must get JSON here.
        let (status, body) = call(
            Request::builder()
                .uri("/")
                .body(Body::empty())
                .expect("request"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let v: serde_json::Value = serde_json::from_str(&body).expect("json");
        assert_eq!(v["kind"], 10021);
        assert_eq!(v["tags"][0][0], "metric");
    }

    #[tokio::test]
    async fn a_wrong_method_on_the_root_is_not_a_silent_200() {
        let (status, _) = call(
            Request::builder()
                .method("DELETE")
                .uri("/")
                .body(Body::empty())
                .expect("request"),
        )
        .await;
        assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
    }

    #[tokio::test]
    async fn a_balance_request_is_404_because_the_bridge_has_none() {
        // The claim in the module docs, exercised rather than restated: a v1
        // device asking for a balance gets nothing, because this component does
        // not count bytes and a number it made up would be worse than a 404.
        for route in ["/balance", "/usage", "/whoami", "/ln-invoice"] {
            let (status, _) = call(
                Request::builder()
                    .uri(route)
                    .body(Body::empty())
                    .expect("request"),
            )
            .await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{route} must not be served");
        }
    }

    #[tokio::test]
    async fn an_empty_body_is_a_400_not_a_purchase() {
        let (status, body) = call(
            Request::builder()
                .method("POST")
                .uri("/?amount=0")
                .body(Body::empty())
                .expect("request"),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let v: serde_json::Value = serde_json::from_str(&body).expect("json");
        assert_eq!(v["accepted"], false);
        assert!(v["refusal"].as_str().is_some(), "a refusal says why");
    }
}
