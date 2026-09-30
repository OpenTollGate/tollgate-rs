//! One buyer session, for a program that speaks TollGate on someone else's
//! behalf.
//!
//! `proxyd` (in `tollgate-suite-wrt`) serves phones that run no TollGate
//! software: for each one it holds a session to the gateway, buying at the rate
//! the phone chose out of what the phone paid. That session is an ordinary
//! node — [`Node`], the Spilman backend and the buying algorithm — reduced to
//! what a buyer needs:
//!
//! - one peer, the gateway, dialled and never listened for;
//! - no charging it: the session says `no_charge`, so the gateway funds no
//!   channel toward it and the phone needs no mint;
//! - a demand the caller sets, and changes while the session runs;
//! - vouchers from whatever [`Funding`] the caller hands it — the phone's own
//!   budget, not a node's wallet;
//! - optionally a [`Connector`], so the connection comes from the phone's
//!   address and the gateway gates the phone, not the proxy.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::sync::watch;

use crate::adapter::{Loopback, ResourceAdapter};
use crate::channel::{Funding, SpilmanChannels, SpilmanConfig};
use crate::config::File;
use crate::node::Node;
use crate::wire::Connector;

/// How long each grant a session buys lasts. A proxied device's rate holds
/// until its user changes it, so long windows cost little in reaction and keep
/// the forfeit on each renewal (lead / window) small.
const WINDOW_MS: u32 = 10_000;

/// How long before a grant runs out the session buys the next. Below
/// [`tollgate_core::buyer::BuyerPolicy::MIN_SAFE_LEAD_MS`] on purpose: the
/// gateway is on the same machine, and at 250 ms of 10 s only 2.5% of each
/// grant is forfeit instead of 30%. If renewals land late under load, the
/// phone's flows stall for seconds — raise this first.
const RENEW_LEAD_MS: u32 = 250;

/// What a buyer session needs to know.
#[derive(Debug, Clone)]
pub struct ClientSpec {
    /// This session's own key, hex. One per device, kept for its life.
    pub secret_hex: String,
    /// The gateway's public key, hex.
    pub gateway: String,
    /// Where the gateway's control plane listens.
    pub endpoint: String,
    /// The gateway's mint, whose vouchers fund the channel.
    pub gateway_mint: String,
    /// Units a channel to the gateway is funded with. Keep it within what the
    /// device has paid for: the funding call is refused past that.
    pub channel_capacity: u64,
}

/// Run a buyer session until `shutdown` resolves.
///
/// `rate` is the demand, in bytes per second, and may change while the session
/// runs; zero stops buying. Buying is exact — no headroom — because the rate
/// is what the device chose, not an estimate of what it will draw.
pub async fn run(
    spec: ClientSpec,
    funding: Arc<dyn Funding>,
    connector: Option<Connector>,
    rate: watch::Receiver<u64>,
    shutdown: impl std::future::Future<Output = ()> + Send,
) -> Result<()> {
    let capacity = spec.channel_capacity.max(1);
    let yaml = format!(
        r#"
identity: {{ secret_key: "{secret}" }}
mint: {{ url: "{mint}" }}
vouchers: {{ accepted_mints: ["{mint}"] }}
network: {{ listen: "127.0.0.1:0" }}
buying: {{ headroom_pct: 100, window_ms: {window_ms}, renew_lead_ms: {renew_lead_ms} }}
channels:
  initial_capacity: {capacity}
  min_capacity: {capacity}
  max_capacity: {capacity}
  capacity_growth_factor: 1.0
peers:
  "{gateway}":
    endpoint: "{endpoint}"
    no_charge: true
"#,
        secret = spec.secret_hex,
        mint = spec.gateway_mint,
        gateway = spec.gateway,
        endpoint = spec.endpoint,
        window_ms = WINDOW_MS,
        renew_lead_ms = RENEW_LEAD_MS,
    );
    let file: File = serde_yaml::from_str(&yaml).context("describe the session")?;
    // The lead is thin on purpose (see `RENEW_LEAD_MS`), so the warning an
    // operator's thin lead gets would be noise here, once per device.
    let mut config = file
        .resolve_with_chosen_lead()
        .context("resolve the session")?;
    config.connector = connector;

    let channels = Arc::new(
        SpilmanChannels::new(SpilmanConfig {
            mint_url: config.mint_url.clone(),
            mint_local: config.mint_local.clone(),
            unit: config.policy.unit.clone(),
            accepted_mints: config.policy.accepted_mints.clone(),
            burned_mints: Vec::new(),
            secret_key_hex: config.identity.secret_hex(),
            funding,
            ttl_seconds: config.channel_ttl_seconds,
        })
        .context("build the channel backend")?,
    );

    // The loopback adapter carries nothing here — the device's traffic goes
    // through the gateway's own gate — but it is where demand lives.
    let adapter: Arc<dyn ResourceAdapter> = Arc::new(Loopback::new());
    tokio::spawn(hold_demand(Arc::clone(&adapter), rate));

    let listener = tokio::net::TcpListener::bind(config.listen)
        .await
        .context("bind the session's control plane")?;
    Node::new(&config, channels, adapter)
        .run_on(listener, config, shutdown)
        .await
}

/// Keep the demand toward every peer at the latest rate. Re-applied on a
/// short tick because the gateway appears as a peer only once connected.
async fn hold_demand(adapter: Arc<dyn ResourceAdapter>, mut rate: watch::Receiver<u64>) {
    loop {
        let current = *rate.borrow_and_update();
        for peer in adapter.peers() {
            adapter.set_demand(peer, current);
        }
        tokio::select! {
            changed = rate.changed() => if changed.is_err() { return },
            _ = tokio::time::sleep(Duration::from_millis(500)) => {}
        }
    }
}
