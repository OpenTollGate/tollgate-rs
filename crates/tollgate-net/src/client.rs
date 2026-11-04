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
//! - one of two ways to pay ([`Buying`]): time at a speed, which reserves the
//!   rate the caller sets, or pay per use, which reserves nothing and is drawn
//!   only by what the device moves. The caller may move a running session
//!   from one to the other;
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

use crate::channel::{Funding, SpilmanChannels, SpilmanConfig};
use crate::config::File;
use crate::enforcer::{Enforcer, Loopback};
use crate::node::Node;
use crate::wire::Connector;
use tollgate_core::buyer::BuyerPolicy;

/// The window each purchase of time at a speed asks for, and so the budget it
/// holds: an hour of the rate it reserves, or as long as the gateway allows.
///
/// A reserved budget drains at its rate whether it is used or not, so a long
/// window costs no more than a short one. It means fewer purchases, and a
/// budget that outlives a restart of the gateway or of the session. A raised
/// rate is bought at once whatever the window. What the session can hold is
/// also bounded by its channels: a purchase bigger than the channel in use
/// takes what fits, and the rollover opens the rest.
const WINDOW_MS: u64 = 3_600_000;

/// The window each purchase of pay per use asks for: as long as the gateway
/// allows, which it clamps to its own range (30 days by default).
///
/// Nothing drains a budget that reserves nothing but what the device moves,
/// so a long window only keeps what the device paid for usable for longer. A
/// short one would let a device that stays idle lose its budget at the
/// deadline.
const PAY_PER_USE_WINDOW_MS: u64 = u64::MAX;

/// How long before its budget runs out the session buys again. Below
/// [`tollgate_core::buyer::BuyerPolicy::MIN_SAFE_LEAD_MS`]: the gateway is on
/// the same machine. Nothing is forfeit by buying early, so if purchases land
/// late under load and the phone's flows stall for seconds, raise this first.
const RENEW_LEAD_MS: u64 = 250;

/// How a session pays. The caller chooses, and may change it while the session
/// runs: it applies from the next purchase, which comes at once when it means
/// reserving a different rate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Buying {
    /// Time at a speed: reserve the rate [`run`] is given, and hold an hour
    /// of it ([`WINDOW_MS`]). The budget drains at that rate, idle or not.
    AtSpeed,
    /// Pay for what you use: reserve nothing, hold `budget` units with the
    /// gateway for as long as it allows ([`PAY_PER_USE_WINDOW_MS`]), and add
    /// back what the device used. Idle costs nothing. How fast the device is
    /// carried is the gateway's choice (its `burst.unreserved_rate`).
    PayPerUse {
        /// Units to hold with the gateway. Each purchase brings the budget
        /// back up to this, within what the channels in use will take.
        budget: u64,
        /// Buy again this long before the budget would run out, at the rate
        /// [`run`] is given. It has to cover how late [`ClientSpec::moved`]
        /// can be.
        renew_lead_ms: u64,
    },
    /// The session is about to end and what it paid is moving on: sign all
    /// its channels will take over to the gateway, reserving nothing. What a
    /// channel has not signed is lost with the session; what the gateway holds
    /// is kept for the payer ([`crate::control::Request::MoveBudget`]).
    SignOver,
}

impl Buying {
    /// `base`, the session's buyer policy, buying this way.
    fn policy(self, base: BuyerPolicy) -> BuyerPolicy {
        match self {
            Buying::AtSpeed => BuyerPolicy {
                reserve: true,
                headroom_pct: 100,
                window_ms: WINDOW_MS,
                renew_lead_ms: RENEW_LEAD_MS,
                budget: 0,
                ..base
            },
            Buying::PayPerUse {
                budget,
                renew_lead_ms,
            } => BuyerPolicy {
                reserve: false,
                window_ms: PAY_PER_USE_WINDOW_MS,
                renew_lead_ms,
                budget,
                ..base
            },
            Buying::SignOver => BuyerPolicy {
                reserve: false,
                window_ms: PAY_PER_USE_WINDOW_MS,
                budget: BuyerPolicy::SIGN_ALL,
                ..base
            },
        }
    }
}

/// What the gateway has carried for a device: cumulative units, counted from
/// any point. The session counts from its first reading, and takes a reading
/// below the one before as counting again from zero — the gateway's counters
/// start again with each of its sessions.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Moved {
    /// Units the gateway sent the device: its download.
    pub to_device: u64,
    /// Units the device sent through the gateway: its upload, which the
    /// session weights by the gateway's from-payer weight as the gateway does.
    pub from_device: u64,
}

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
    /// How the session pays, and any change to it while it runs.
    pub buying: watch::Receiver<Buying>,
    /// What the gateway has carried for the device, as the caller reads it
    /// ([`Moved`]); needed to pay per use.
    ///
    /// The session carries none of the device's traffic, so it cannot count
    /// it. A pay-per-use budget is drawn only by that traffic, so the
    /// session's own count of it, which decides when to buy, comes from here
    /// while it pays per use. Time at a speed is counted at its reserved rate,
    /// which these readings, coming late and in lumps, would only overstate.
    pub moved: Option<watch::Receiver<Moved>>,
}

/// Run a buyer session until `shutdown` resolves.
///
/// `rate` is the demand, in bytes per second, and may change while the session
/// runs; zero stops buying. For time at a speed it is the rate reserved, and
/// buying is exact — no headroom — because the rate is what the device chose,
/// not an estimate of what it will draw. For pay per use nothing is reserved:
/// it is how fast the device may move, which says how early to buy again.
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
buying: {{ window_ms: {WINDOW_MS}, renew_lead_ms: {RENEW_LEAD_MS} }}
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
    );
    let file: File = serde_yaml::from_str(&yaml).context("describe the session")?;
    // The lead is thin on purpose (see `RENEW_LEAD_MS`), so the warning an
    // operator's thin lead gets would be noise here, once per device.
    let mut config = file
        .resolve_with_chosen_lead()
        .context("resolve the session")?;
    config.connector = connector;
    let base = config.buyer;
    config.buyer = spec.buying.borrow().policy(base);

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

    // The loopback enforcer carries nothing here — the device's traffic goes
    // through the gateway's own gate — but it is where demand lives, and,
    // for pay per use, what the caller says the gateway carried.
    let loopback = Arc::new(Loopback::new());
    let enforcer: Arc<dyn Enforcer> = loopback.clone();
    let demand = tokio::spawn(hold_demand(Arc::clone(&enforcer), rate));
    let meter = spec
        .moved
        .clone()
        .map(|moved| tokio::spawn(hold_moved(loopback, moved, spec.buying.clone())));
    let (policy_tx, policy_rx) = watch::channel(config.buyer);
    let follow = tokio::spawn(follow_buying(spec.buying.clone(), base, policy_tx));

    let listener = tokio::net::TcpListener::bind(config.listen)
        .await
        .context("bind the session's control plane")?;
    let ran = Node::new(&config, channels, enforcer)
        .with_buyer_policy(policy_rx)
        .run_on(listener, config, shutdown)
        .await;
    demand.abort();
    follow.abort();
    if let Some(meter) = meter {
        meter.abort();
    }
    ran
}

/// Keep the demand toward every peer at the latest rate. Re-applied on a
/// short tick because the gateway appears as a peer only once connected.
async fn hold_demand(enforcer: Arc<dyn Enforcer>, mut rate: watch::Receiver<u64>) {
    loop {
        let current = *rate.borrow_and_update();
        for peer in enforcer.peers() {
            enforcer.set_demand(peer, current);
        }
        tokio::select! {
            changed = rate.changed() => if changed.is_err() { return },
            _ = tokio::time::sleep(Duration::from_millis(500)) => {}
        }
    }
}

/// Hand the node each change to how the session buys.
async fn follow_buying(
    mut buying: watch::Receiver<Buying>,
    base: BuyerPolicy,
    policy: watch::Sender<BuyerPolicy>,
) {
    while buying.changed().await.is_ok() {
        let now = *buying.borrow_and_update();
        policy.send_replace(now.policy(base));
    }
}

/// Count what the caller says the gateway carried for the device as this
/// session's own traffic with the gateway, so the buyer draws its count of a
/// pay-per-use budget as the gateway does.
///
/// Only while it pays per use, and once the gateway is a peer: what moved
/// before is not this session's.
async fn hold_moved(
    loopback: Arc<Loopback>,
    mut moved: watch::Receiver<Moved>,
    buying: watch::Receiver<Buying>,
) {
    let mut tally = Tally::default();
    loop {
        let grew = tally.read(*moved.borrow_and_update());
        let per_use = matches!(*buying.borrow(), Buying::PayPerUse { .. });
        if per_use && grew != Moved::default() {
            for peer in loopback.peers() {
                // From this side the gateway is the peer: what it sent the
                // device was received from it, and the device's upload was
                // delivered to it.
                loopback.record_received(peer, grew.to_device);
                loopback.record_delivered(peer, grew.from_device);
            }
        }
        tokio::select! {
            changed = moved.changed() => if changed.is_err() { return },
            _ = tokio::time::sleep(Duration::from_millis(500)) => {}
        }
    }
}

/// Turns cumulative readings into what grew since the last.
#[derive(Debug, Default)]
struct Tally {
    last: Option<Moved>,
}

impl Tally {
    /// What grew since the last reading: nothing at the first, and all of a
    /// reading below the one before, which counts again from zero.
    fn read(&mut self, now: Moved) -> Moved {
        let grew = match self.last {
            None => Moved::default(),
            Some(last) => Moved {
                to_device: since(now.to_device, last.to_device),
                from_device: since(now.from_device, last.from_device),
            },
        };
        self.last = Some(now);
        grew
    }
}

fn since(now: u64, last: u64) -> u64 {
    if now >= last { now - last } else { now }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn moved(to_device: u64, from_device: u64) -> Moved {
        Moved {
            to_device,
            from_device,
        }
    }

    #[test]
    fn a_tally_counts_from_its_first_reading() {
        let mut tally = Tally::default();
        assert_eq!(tally.read(moved(5_000, 100)), Moved::default());
        assert_eq!(tally.read(moved(8_000, 150)), moved(3_000, 50));
        assert_eq!(tally.read(moved(8_000, 150)), Moved::default());
    }

    #[test]
    fn a_reading_below_the_last_counts_again_from_zero() {
        // The gateway's counters start again with each of its sessions.
        let mut tally = Tally::default();
        tally.read(moved(5_000, 100));
        assert_eq!(tally.read(moved(700, 10)), moved(700, 10));
        assert_eq!(tally.read(moved(1_000, 10)), moved(300, 0));
    }
}
