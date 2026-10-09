//! The driver: turns real events into [`Event`]s, runs [`Action`]s.
//!
//! This is the whole of the host's job. Core decides; this connects those
//! decisions to sockets, a clock, a signer and a channel backend. Everything
//! that would stop the crate below from running on an ESP32 lives here.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tollgate_core::buyer::BuyerPolicy;
use tollgate_core::config::{NodePolicy, PeerPolicy};
use tollgate_core::session::Sessions;
use tollgate_core::{Action, Event, Millis};
use tollgate_protocol::{
    ChannelUpdate, Message, PubKey, ReasonCode, RefusedUpdate, TopUp, TopUpReject,
};
use tracing::{debug, info, warn};

use crate::channel::{self, ChannelBackend, MintNotAccepted};
use crate::control;
use crate::enforcer::Enforcer;
use crate::identity::Identity;
use crate::settle::{Backoff, Settler};
use crate::wire::{self, Identify, Wire};

/// How often the node samples its meters and ticks core.
///
/// The meter sample is what draws grants down, so this also bounds how far past
/// its grant a peer can get before the shaper notices. It has to be well under
/// the smallest grant window a payer can ask for.
const TICK: Duration = Duration::from_millis(100);

/// How long a shutdown keeps retrying settlements that fail.
///
/// Long enough to ride out a mint that blinks, short enough that stopping the
/// node still feels like stopping it. What is left unsettled after this is
/// abandoned: the channel is lost to its refund timelock, as it would be to a
/// power cut.
const SHUTDOWN_SETTLE_GRACE: Duration = Duration::from_secs(5);

/// Most channel fundings held per peer while the enforcer is not selling.
///
/// A peer normally has one in flight — its first channel or a rollover — so
/// this is room for both and a repeat; past it a peer is only making the node
/// hold its bytes.
const MAX_DEFERRED_FUNDINGS: usize = 4;

/// A peer the operator has said something about.
#[derive(Debug, Clone)]
pub struct PeerConfig {
    /// Their identity, known ahead of time.
    pub pubkey: PubKey,
    /// `host:port` of their control plane, if we are the side that dials. The
    /// data plane is the next port up.
    ///
    /// `None` for a peer that dials us. Its policy still applies — refusing a
    /// peer, or carrying it for free, is a decision about who it is and not
    /// about who opened the connection.
    pub endpoint: Option<String>,
    /// Operator overrides for this peer.
    pub policy: PeerPolicy,
}

/// Everything a node needs to start.
#[derive(Debug)]
pub struct NodeConfig {
    /// This node's keypair.
    pub identity: Identity,
    /// What it sells and on what terms.
    pub policy: NodePolicy,
    /// How it buys.
    pub buyer: BuyerPolicy,
    /// Control-plane listen address.
    pub listen: SocketAddr,
    /// Whether a peer's announced key has to agree with the address it
    /// connects from.
    pub identify: Identify,
    /// The URL peers reach this node's mint on, advertised in our Offer.
    pub mint_url: String,
    /// Where this node reaches that mint itself.
    pub mint_local: String,
    /// How long a channel this node funds lives before the refund path opens,
    /// in seconds. The channel backend applies it; core only ever sees the
    /// expiry that results.
    pub channel_ttl_seconds: u64,
    /// Peers to dial. Anyone else has to dial us.
    pub peers: Vec<PeerConfig>,
    /// How to open a connection to a peer, if not the plain way.
    pub connector: Option<wire::Connector>,
}

impl NodeConfig {
    /// Where the loopback data plane listens, one port above the control plane.
    ///
    /// Only meaningful for the loopback enforcer: a kernel enforcer forwards real
    /// traffic and has no socket of its own.
    pub fn data_listen(&self) -> SocketAddr {
        let mut addr = self.listen;
        addr.set_port(self.listen.port() + 1);
        addr
    }
}

/// A running node.
pub struct Node {
    identity: Identity,
    sessions: Sessions,
    enforcer: Arc<dyn Enforcer>,
    channels: Arc<dyn ChannelBackend>,
    /// Settlements, and the retries of the ones that fail.
    settler: Settler,
    /// Outbound queue per connected peer.
    links: HashMap<PubKey, mpsc::Sender<Message>>,
    /// Channel fundings a peer sent while the enforcer was not selling to it,
    /// verified once it is. See [`Enforcer::selling`].
    deferred: HashMap<PubKey, Vec<Vec<u8>>>,
    /// What the node is doing, republished each tick for the control socket.
    published: control::Published,
    started: Instant,
}

impl Node {
    /// Build a node. Nothing is listening or dialing until [`Self::run`].
    pub fn new(
        config: &NodeConfig,
        channels: Arc<dyn ChannelBackend>,
        enforcer: Arc<dyn Enforcer>,
    ) -> Self {
        let mut sessions = Sessions::new(
            config.identity.pubkey(),
            config.policy.clone(),
            config.buyer,
        );
        // Nobody is connected yet, so there is nobody to tell: each peer
        // hears its terms in the Offer it gets on connecting.
        for peer in &config.peers {
            let told = sessions.set_peer_policy(peer.pubkey, peer.policy, Millis::ZERO);
            debug_assert!(told.is_empty());
        }

        Self {
            identity: config.identity.clone(),
            sessions,
            enforcer,
            settler: Settler::new(Arc::clone(&channels), Backoff::DEFAULT),
            channels,
            links: HashMap::new(),
            deferred: HashMap::new(),
            published: Default::default(),
            started: Instant::now(),
        }
    }

    /// Retry failed settlements on this schedule instead of the default.
    ///
    /// For tests, which cannot wait a second for the first retry.
    pub fn with_settle_backoff(mut self, backoff: Backoff) -> Self {
        self.settler.set_backoff(backoff);
        self
    }

    /// The snapshot the control socket serves. Cheap to clone and lock-free to
    /// read, so a watcher never holds the event loop up.
    pub fn published(&self) -> control::Published {
        Arc::clone(&self.published)
    }

    /// The enforcer, so a demo or a test can set demand and read counters.
    pub fn enforcer(&self) -> Arc<dyn Enforcer> {
        Arc::clone(&self.enforcer)
    }

    /// This node's identity.
    pub fn pubkey(&self) -> PubKey {
        self.identity.pubkey()
    }

    /// Milliseconds since this node started.
    ///
    /// Core never reads a clock; this is the only place time enters, and it is
    /// monotonic by construction.
    fn now(&self) -> Millis {
        millis_since(self.started)
    }

    /// Listen, dial, and run until `shutdown` resolves or something goes badly
    /// wrong.
    pub async fn run(
        self,
        config: NodeConfig,
        shutdown: impl std::future::Future<Output = ()> + Send,
    ) -> Result<()> {
        let control = TcpListener::bind(config.listen)
            .await
            .with_context(|| format!("bind control plane on {}", config.listen))?;
        self.run_on(control, config, shutdown).await
    }

    /// [`Self::run`], on a control-plane listener the caller already bound.
    ///
    /// For a caller that has to know the port before the node starts but
    /// cannot pick it in advance — tests bind port 0 and read back what the OS
    /// chose, so concurrent runs never contend for the same port.
    pub async fn run_on(
        mut self,
        control: TcpListener,
        config: NodeConfig,
        shutdown: impl std::future::Future<Output = ()> + Send,
    ) -> Result<()> {
        let (wire_tx, mut wire_rx) = mpsc::channel::<Wire>(256);
        // Channel work may block — a real backend talks to a mint — so it runs
        // on a blocking thread and its result comes back here rather than
        // stalling the event loop.
        let (done_tx, mut done_rx) = mpsc::channel::<Event>(256);

        let local = control.local_addr().unwrap_or(config.listen);
        info!(
            pubkey = %self.identity.pubkey(),
            control = %local,
            identify = ?config.identify,
            "node listening"
        );

        // Refusing every connection is the correct behaviour here and a baffling
        // one to debug, so say it once at startup rather than once per peer.
        if config.identify == Identify::Fips && local.is_ipv4() {
            warn!(
                control = %local,
                "listening on IPv4 while checking mesh identity: no peer on fips0 can reach this"
            );
        }

        tokio::spawn(wire::listen(control, wire_tx.clone(), config.identify));

        for peer in &config.peers {
            spawn_dialer(
                peer.clone(),
                wire_tx.clone(),
                config.identify,
                config.connector.clone(),
            );
        }

        let mut ticker = tokio::time::interval(TICK);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        let mut shutdown = std::pin::pin!(shutdown);
        loop {
            tokio::select! {
                Some(event) = wire_rx.recv() => self.on_wire(event, &done_tx).await,
                Some(event) = done_rx.recv() => self.dispatch(event, &done_tx).await,
                _ = ticker.tick() => {
                    self.on_tick(&done_tx).await;
                    self.publish(&config);
                }
                _ = &mut shutdown => break,
            }
        }

        self.wind_down(&done_tx).await;
        Ok(())
    }

    /// Say goodbye properly.
    ///
    /// A bare FIN reads to a peer as an unclean disconnect and starts the same
    /// cleanup a timeout would, so sending Disconnect first is the difference
    /// between the peer tearing our state down on a timer and doing it now.
    async fn wind_down(&mut self, done: &mpsc::Sender<Event>) {
        info!("shutting down: telling peers and settling");
        // Before the shutdown's own settlements start, so they see the
        // deadline too — and so a retry waiting out a long backoff wakes for
        // one last try rather than holding the node open.
        self.settler.begin_shutdown(SHUTDOWN_SETTLE_GRACE);
        for action in self.sessions.shutdown() {
            self.execute(action, done).await;
        }

        // Give the outbound queues a moment to drain before the sockets go.
        // Nothing is lost if this expires — the peer falls back to its timeout.
        // The settlements get their grace period, and a little over for an
        // attempt already under way when it ends.
        tokio::join!(
            tokio::time::sleep(Duration::from_millis(250)),
            self.settler
                .drain(SHUTDOWN_SETTLE_GRACE + Duration::from_secs(1)),
        );
    }

    async fn on_wire(&mut self, event: Wire, done: &mpsc::Sender<Event>) {
        match event {
            Wire::PeerUp { peer, addr, tx } => {
                self.links.insert(peer, tx);
                // Tie the key the protocol knows to the address the kernel
                // knows, before anything is gated or shaped for this peer.
                self.enforcer.register(peer, addr.ip());
                self.dispatch(Event::PeerConnected { peer }, done).await;
            }
            Wire::PeerDown { peer } => {
                self.links.remove(&peer);
                self.deferred.remove(&peer);
                self.enforcer.remove(peer);
                self.dispatch(Event::PeerDisconnected { peer }, done).await;
            }
            Wire::Message { peer, msg } => {
                // Nothing is sold while the enforcer cannot enforce it: the
                // capacity this node can deliver is zero, and the payer is told
                // so as it would be at any other ceiling. Its money is untouched,
                // since nothing is ratcheted, and core never hears of it.
                if let Message::TopUp(ref t) = msg
                    && !self.enforcer.selling(peer)
                {
                    let reject = TopUpReject {
                        refused: t
                            .updates
                            .iter()
                            .map(|u| RefusedUpdate {
                                channel_id: u.channel_id,
                                cumulative: u.cumulative,
                            })
                            .collect(),
                        max_rate_available: 0,
                        reason: ReasonCode::RateExceedsCapacity,
                    };
                    log_refusal(peer, &reject, Side::Sent);
                    self.send(peer, Message::TopUpReject(reject)).await;
                    return;
                }
                // Core trusts what it is handed, so the signature is checked
                // here — before the message reaches anything that acts on it.
                // Every update in the purchase, since it is honored or refused
                // as a whole and one bad signature makes the whole thing
                // unauthentic. The message goes no further, but core still
                // hears of it: the payer is owed a Reject, and a channel that
                // keeps failing is closed. Checking keeps nothing: the backend
                // records the updates only once core has accepted the
                // purchase, as `Action::RecordUpdates`.
                if let Message::TopUp(ref t) = msg
                    && let Some(bad) = t.updates.iter().find(|u| {
                        !self
                            .channels
                            .verify_update(peer, u.channel_id, u.cumulative, u.signature)
                    })
                {
                    warn!(%peer, "refusing a TopUp that does not verify");
                    let channel_id = bad.channel_id;
                    self.dispatch(Event::TopUpSignatureInvalid { peer, channel_id }, done)
                        .await;
                    return;
                }
                match msg {
                    Message::TopUpReject(ref r) => log_refusal(peer, r, Side::Received),
                    Message::Reject(ref r) => {
                        warn!(%peer, reason = ?r.reason, rejected_type = r.rejected_type, "a peer rejected our message");
                    }
                    _ => {}
                }
                self.dispatch(Event::MessageReceived { peer, msg }, done)
                    .await;
            }
        }
    }

    /// Republish what the node is doing, for anything watching.
    fn publish(&self, config: &NodeConfig) {
        self.published.store(Arc::new(control::snapshot(
            &self.sessions,
            self.enforcer.as_ref(),
            &hex::encode(self.identity.pubkey().0),
            &config.mint_url,
            self.started.elapsed().as_millis() as u64,
            self.now(),
        )));
    }

    /// Sample the meters and tick core.
    async fn on_tick(&mut self, done: &mpsc::Sender<Event>) {
        self.release_deferred(done).await;
        for peer in self.enforcer.peers() {
            let counters = self.enforcer.counters(peer);
            self.dispatch(Event::Metered { peer, counters }, done).await;

            let rate = self.enforcer.demand(peer);
            self.dispatch(Event::DemandObserved { peer, rate }, done)
                .await;
        }
        self.dispatch(Event::Tick, done).await;
    }

    /// Verify the fundings held back from peers the enforcer now sells to.
    async fn release_deferred(&mut self, done: &mpsc::Sender<Event>) {
        if self.deferred.is_empty() {
            return;
        }
        let ready: Vec<PubKey> = self
            .deferred
            .keys()
            .copied()
            .filter(|peer| self.enforcer.selling(*peer))
            .collect();
        for peer in ready {
            for funding in self.deferred.remove(&peer).unwrap_or_default() {
                debug!(%peer, "verifying a channel funding held while not selling");
                self.execute(Action::VerifyFunding { peer, funding }, done)
                    .await;
            }
        }
    }

    /// Feed one event to core and carry out everything it asks for.
    async fn dispatch(&mut self, event: Event, done: &mpsc::Sender<Event>) {
        let now = self.now();
        for action in self.sessions.handle(event, now) {
            self.execute(action, done).await;
        }
    }

    async fn execute(&mut self, action: Action, done: &mpsc::Sender<Event>) {
        match action {
            Action::Send { peer, msg } => {
                match msg {
                    Message::TopUpReject(ref r) => log_refusal(peer, r, Side::Sent),
                    // Kept for operator review: a peer whose purchases keep
                    // failing verification is broken or probing.
                    Message::Reject(ref r) => {
                        warn!(%peer, reason = ?r.reason, rejected_type = r.rejected_type, "rejected a peer's message");
                    }
                    _ => {}
                }
                self.send(peer, msg).await
            }

            Action::SignAndSendTopUp {
                peer,
                ratchets,
                window_ms,
            } => {
                // One signature per channel, one message for the purchase: the
                // grant is their combined increase.
                let mut updates = Vec::with_capacity(ratchets.len());
                for (channel_id, cumulative) in ratchets {
                    match self.channels.sign_update(channel_id, cumulative) {
                        Ok(signature) => updates.push(ChannelUpdate {
                            channel_id,
                            cumulative,
                            signature,
                        }),
                        Err(e) => {
                            warn!(%peer, error = %e, "could not sign a channel update");
                            return;
                        }
                    }
                }
                self.send(peer, Message::TopUp(TopUp { updates, window_ms }))
                    .await;
            }

            Action::RecordUpdates { peer, updates } => {
                // In line, not on a blocking thread: keeping a signed state is
                // local, and a settlement this purchase set off comes next in
                // the same action list and has to see it.
                for u in updates {
                    if let Err(e) =
                        self.channels
                            .record_update(peer, u.channel_id, u.cumulative, u.signature)
                    {
                        warn!(%peer, error = format!("{e:#}"), "could not record a channel update");
                    }
                }
            }

            Action::SetAccess { peer, access } => {
                debug!(%peer, ?access, "access changed");
                self.enforcer.set_access(peer, access);
            }

            Action::SetShapingRate { peer, rate } => {
                debug!(%peer, rate, "shaping rate changed");
                self.enforcer.set_shaping_rate(peer, rate);
            }

            Action::FundChannel {
                peer,
                request,
                mint_url,
                capacity,
            } => {
                // No new money goes out while this node cannot sell. Core is
                // not told: it asks again once its funding timeout passes, and
                // a failure reported now would have it ask every tick.
                if !self.enforcer.selling(peer) {
                    debug!(%peer, request, "not funding a channel while not selling");
                    return;
                }
                let channels = Arc::clone(&self.channels);
                let done = done.clone();
                let started = self.started;
                tokio::task::spawn_blocking(move || {
                    match channels.fund(peer, &mint_url, capacity) {
                        Ok(funded) => {
                            let now = millis_since(started);
                            let _ = done.blocking_send(Event::OutgoingChannelFunded {
                                peer,
                                request,
                                channel_id: funded.channel_id,
                                capacity: funded.capacity,
                                expires_at: funded.expiry.map(|e| channel::expires_at(e, now)),
                                funding: funded.funding,
                            });
                        }
                        Err(e) => {
                            warn!(%peer, error = format!("{e:#}"), "could not fund a channel");
                            // So core can ask again rather than wait out the
                            // funding timeout.
                            let _ =
                                done.blocking_send(Event::OutgoingFundingFailed { peer, request });
                        }
                    }
                });
            }

            Action::VerifyFunding { peer, funding } => {
                // Held, not refused: refusing a funding ends the session, and
                // a session already running is kept through an outage. Verified
                // on the first tick the enforcer sells to this peer again.
                if !self.enforcer.selling(peer) {
                    let held = self.deferred.entry(peer).or_default();
                    if held.len() < MAX_DEFERRED_FUNDINGS {
                        debug!(%peer, "holding a channel funding while not selling");
                        held.push(funding);
                    } else {
                        warn!(%peer, "dropping a channel funding: too many held while not selling");
                    }
                    return;
                }
                let channels = Arc::clone(&self.channels);
                let done = done.clone();
                let started = self.started;
                tokio::task::spawn_blocking(move || {
                    let event = match channels.verify(peer, &funding) {
                        Ok(v) => Event::IncomingFundingVerified {
                            peer,
                            channel_id: v.channel_id,
                            capacity: v.capacity,
                            mint_url: v.mint_url,
                            expires_at: v
                                .expiry
                                .map(|e| channel::expires_at(e, millis_since(started))),
                        },
                        Err(e) => {
                            warn!(%peer, error = format!("{e:#}"), "peer funding did not verify");
                            let reason = if e.downcast_ref::<MintNotAccepted>().is_some() {
                                ReasonCode::MintNotAccepted
                            } else {
                                ReasonCode::FundingInvalid
                            };
                            Event::IncomingFundingRejected { peer, reason }
                        }
                    };
                    let _ = done.blocking_send(event);
                });
            }

            Action::SettleChannel {
                peer,
                channel_id,
                expires_at,
            } => {
                // Worth logging: a channel settling far sooner than expected is
                // what a rollover going wrong looks like from outside.
                debug!(%peer, ?channel_id, ?expires_at, "settling a channel");
                // Core has already let the channel go, so this is the only
                // place a failed settlement can be tried again, and the settler
                // keeps trying — until the channel's refund expiry, past which
                // the funder can reclaim it and settling it buys nothing. Core
                // hands the expiry over on its own clock, which started with
                // this node.
                // An expiry too far off for the clock to represent is none.
                let deadline = expires_at
                    .and_then(|at| self.started.checked_add(Duration::from_millis(at.0)))
                    .map(tokio::time::Instant::from_std);
                self.settler.settle(peer, channel_id, deadline);
            }

            Action::ReclaimChannel {
                peer,
                channel_id,
                capacity,
                expires_at,
            } => {
                // A funding core had given up on came back after the one
                // asked for in its place. Nothing was signed on it and the
                // peer never heard of it, so no receiver will ever close it,
                // and what is locked in it comes back only through the
                // refund path once it expires. Taking that path is not built
                // yet — the same gap as change after a reboot — so the
                // channel is named here for the operator to reclaim.
                let expires_in_s = expires_at.map(|at| at.saturating_since(self.now()) / 1_000);
                warn!(
                    %peer,
                    ?channel_id,
                    capacity,
                    ?expires_in_s,
                    "funded a channel that is no longer wanted; its funds are \
                     locked until its refund path opens, and this node does \
                     not reclaim them itself yet"
                );
            }

            Action::DropPeer { peer } => {
                info!(%peer, "dropping peer");
                self.links.remove(&peer);
                self.deferred.remove(&peer);
                self.enforcer.remove(peer);
            }
        }
    }

    async fn send(&mut self, peer: PubKey, msg: Message) {
        let Some(link) = self.links.get(&peer) else {
            debug!(%peer, "no link for a message core wanted sent");
            return;
        };
        // A full outbox means the peer is not reading. Dropping is safe for the
        // only message that repeats: TopUp is cumulative, so the next one
        // carries the correct total anyway.
        if link.try_send(msg).is_err() {
            warn!(%peer, "outbox full, dropping a message");
        }
    }
}

/// Milliseconds on the node's clock, for work that finishes off the event loop
/// and has to stamp its result on the same clock [`Node::now`] reads.
fn millis_since(started: Instant) -> Millis {
    Millis(started.elapsed().as_millis() as u64)
}

/// Which end of a refusal we are.
#[derive(Debug, Clone, Copy)]
enum Side {
    /// We refused a peer's purchase.
    Sent,
    /// A peer refused ours.
    Received,
}

/// Record a refused purchase.
///
/// Both ends log it, because it means different things to each: the provider
/// learns a peer is asking for more than it will give, and the payer learns its
/// buying is being clipped and by how much. Neither number is otherwise
/// visible, so without this a peer sits silently pinned at a limit with nothing
/// to say why.
///
/// Severity follows [`ReasonCode::avoidable_from_offer`]. Nearly every reason
/// means the peer ignored something we advertised, or that a revised Offer
/// crossed its message in flight — a real fault, and a warning. A rate refusal
/// is not: the Offer carries no rate ceiling, so being refused and told what
/// would be accepted is how a payer is *supposed* to find the limit. Logging
/// that as a fault would bury the ones that are.
fn log_refusal(peer: PubKey, reject: &TopUpReject, side: Side) {
    let direction = match side {
        Side::Sent => "refused a peer's purchase",
        Side::Received => "a peer refused our purchase",
    };

    let channels = reject.refused.len();
    if reject.reason.avoidable_from_offer() {
        warn!(
            %peer,
            reason = ?reject.reason,
            channels,
            "{direction}: this should not happen against an Offer that was read"
        );
    } else {
        info!(
            %peer,
            channels,
            max_rate_available = reject.max_rate_available,
            "{direction}: rate above what is uncommitted"
        );
    }
}

/// Keep trying to reach a configured peer.
///
/// A peering is a standing relationship, not a one-shot connection, so a
/// refused dial is a reason to wait and try again rather than to give up.
///
/// A peer with no endpoint is one that dials us; there is nothing to reach out
/// to, and its policy is already in place for when it does.
fn spawn_dialer(
    peer: PeerConfig,
    wire_tx: mpsc::Sender<Wire>,
    identify: Identify,
    connector: Option<wire::Connector>,
) {
    let Some(endpoint) = peer.endpoint.clone() else {
        return;
    };
    tokio::spawn(async move {
        loop {
            let dialed = wire::dial(
                &endpoint,
                peer.pubkey,
                wire_tx.clone(),
                identify,
                connector.as_ref(),
            );
            if let Err(e) = dialed.await {
                debug!(%endpoint, error = %e, "control dial failed");
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    });
}
