//! The driver: turns real events into [`Event`]s, runs [`Action`]s.
//!
//! This is the whole of the host's job. Core decides; this connects those
//! decisions to sockets, a clock, a signer and a channel backend. Everything
//! that would stop the crate below from running on an ESP32 lives here.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tollgate_core::buyer::{BuyerPolicy, FUNDING_TIMEOUT_MS};
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
use crate::wire::{self, Connection, PeerIdentity, Wire};

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

/// How long a failed funding is held before core hears of it: one second,
/// doubling to core's own funding timeout.
///
/// Core asks again on the tick after it hears a funding failed, so a mint that
/// is down would otherwise be asked ten times a second, and the failure logged
/// as often. Until core hears, it waits on the request, as it waits on one that
/// is slow; past its funding timeout it would ask again without hearing, so the
/// wait goes no further than that.
const FUNDING_BACKOFF: Backoff = Backoff {
    initial: Duration::from_secs(1),
    max: Duration::from_millis(FUNDING_TIMEOUT_MS),
};

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
    /// Who a connecting peer is: its proven key, or the address it connects
    /// from. `enforcer.identity`.
    pub peer_identity: PeerIdentity,
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

/// The connection a peer is on.
#[derive(Debug)]
struct Link {
    /// Which one, so an event from a connection it replaced can be told apart.
    conn: Connection,
    /// Where it comes from: what the enforcer gates.
    addr: SocketAddr,
    /// Its outbound queue.
    tx: mpsc::Sender<Message>,
}

/// A running node.
pub struct Node {
    identity: Identity,
    sessions: Sessions,
    enforcer: Arc<dyn Enforcer>,
    channels: Arc<dyn ChannelBackend>,
    /// Settlements, and the retries of the ones that fail.
    settler: Settler,
    /// Fundings toward each peer that have failed in a row. Shared with the
    /// funding tasks, which count a failure and clear the count on success.
    funding_failures: Arc<Mutex<HashMap<PubKey, u32>>>,
    /// The connection each peer is on, and its outbound queue.
    links: HashMap<PubKey, Link>,
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
            funding_failures: Arc::default(),
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
            identity = %config.peer_identity,
            "node listening"
        );

        // Refusing every connection is the correct behaviour here and a baffling
        // one to debug, so say it once at startup rather than once per peer.
        if config.peer_identity == PeerIdentity::Pubkey && local.is_ipv4() {
            warn!(
                control = %local,
                "listening on IPv4 while checking mesh identity: no peer on fips0 can reach this"
            );
        }

        tokio::spawn(wire::listen(control, wire_tx.clone(), config.peer_identity));

        for peer in &config.peers {
            spawn_dialer(
                peer.clone(),
                wire_tx.clone(),
                config.peer_identity,
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
            Wire::PeerUp {
                peer,
                conn,
                addr,
                tx,
            } => {
                // A peer reconnecting before its old connection's end has
                // reached us. The new connection replaces the old one, as
                // core's session does; what the host held for the old one
                // goes the way it would have had the old one ended first, so
                // the enforcer counts from zero, as the session's new meter
                // does, and gates the address the peer is at now.
                let replaced = self.links.insert(peer, Link { conn, addr, tx });
                if let Some(old) = replaced {
                    debug!(
                        %peer,
                        old = %old.addr,
                        new = %addr,
                        "a peer reconnected before its old connection ended"
                    );
                    self.deferred.remove(&peer);
                    self.enforcer.remove(peer);
                }
                // Tie the key the protocol knows to the address the kernel
                // knows, before anything is gated or shaped for this peer.
                self.enforcer.register(peer, addr.ip());
                self.dispatch(Event::PeerConnected { peer }, done).await;
            }
            Wire::PeerDown { peer, conn } => {
                // The end of a connection already replaced is not the peer's:
                // tearing down now would take the connection it is on with it.
                if self.replaced(peer, conn) {
                    debug!(%peer, "a replaced connection ended");
                    return;
                }
                self.links.remove(&peer);
                self.deferred.remove(&peer);
                self.enforcer.remove(peer);
                self.dispatch(Event::PeerDisconnected { peer }, done).await;
            }
            Wire::Message { peer, conn, .. } if self.replaced(peer, conn) => {
                // Said on a connection the peer has since replaced, to a
                // session that has since started over.
                debug!(%peer, "dropping a message from a replaced connection");
            }
            Wire::Message { peer, msg, .. } => {
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

    /// Whether `conn` is a connection of `peer`'s that another has replaced.
    ///
    /// Not merely one that is not current: once core has dropped a peer there
    /// is no link at all, and its connection's end is still that connection's
    /// to report.
    fn replaced(&self, peer: PubKey, conn: Connection) -> bool {
        self.links.get(&peer).is_some_and(|link| link.conn != conn)
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
                let failures = Arc::clone(&self.funding_failures);
                tokio::spawn(async move {
                    let funded = tokio::task::spawn_blocking(move || {
                        channels.fund(peer, &mint_url, capacity)
                    })
                    .await
                    .unwrap_or_else(|e| Err(anyhow::anyhow!("funding task failed: {e}")));
                    match funded {
                        Ok(funded) => {
                            let failed = failures
                                .lock()
                                .expect("not poisoned")
                                .remove(&peer)
                                .unwrap_or(0);
                            if failed > 0 {
                                info!(%peer, failed, "funded a channel after failing");
                            }
                            let now = millis_since(started);
                            let _ = done
                                .send(Event::OutgoingChannelFunded {
                                    peer,
                                    request,
                                    channel_id: funded.channel_id,
                                    capacity: funded.capacity,
                                    expires_at: funded.expiry.map(|e| channel::expires_at(e, now)),
                                    funding: funded.funding,
                                })
                                .await;
                        }
                        Err(e) => {
                            let attempt = {
                                let mut failures = failures.lock().expect("not poisoned");
                                let count = failures.entry(peer).or_default();
                                *count = count.saturating_add(1);
                                *count
                            };
                            let retry_in = FUNDING_BACKOFF.delay(attempt);
                            warn!(
                                %peer,
                                attempt,
                                retry_in_s = retry_in.as_secs_f32(),
                                error = format!("{e:#}"),
                                "could not fund a channel"
                            );
                            // Core asks again on the next tick after it hears,
                            // so it hears once the wait is over.
                            tokio::time::sleep(retry_in).await;
                            let _ = done
                                .send(Event::OutgoingFundingFailed { peer, request })
                                .await;
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
        if link.tx.try_send(msg).is_err() {
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
    identity: PeerIdentity,
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
                identity,
                connector.as_ref(),
            );
            if let Err(e) = dialed.await {
                debug!(%endpoint, error = %e, "control dial failed");
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use std::net::IpAddr;
    use std::sync::atomic::{AtomicBool, Ordering};

    use anyhow::bail;
    use tollgate_core::access::AccessLevel;
    use tollgate_core::meter::Counters;
    use tollgate_protocol::{ChannelId, Signature};

    use super::*;
    use crate::channel::{FundedChannel, LocalChannels, VerifiedChannel};
    use crate::enforcer::Loopback;

    /// An enforcer that remembers who it was told about and from where, and,
    /// like the kernel ones, ignores a registration for a peer it already has.
    #[derive(Debug, Default)]
    struct Registry(Mutex<HashMap<PubKey, IpAddr>>);

    impl Registry {
        fn addr(&self, peer: PubKey) -> Option<IpAddr> {
            self.0.lock().unwrap().get(&peer).copied()
        }
    }

    impl Enforcer for Registry {
        fn register(&self, peer: PubKey, addr: IpAddr) {
            self.0.lock().unwrap().entry(peer).or_insert(addr);
        }
        fn set_access(&self, _: PubKey, _: AccessLevel) {}
        fn set_shaping_rate(&self, _: PubKey, _: u64) {}
        fn counters(&self, _: PubKey) -> Counters {
            Counters::default()
        }
        fn demand(&self, _: PubKey) -> u64 {
            0
        }
        fn set_demand(&self, _: PubKey, _: u64) {}
        fn shaping_rate(&self, _: PubKey) -> u64 {
            0
        }
        fn peers(&self) -> Vec<PubKey> {
            self.0.lock().unwrap().keys().copied().collect()
        }
        fn remove(&self, peer: PubKey) {
            self.0.lock().unwrap().remove(&peer);
        }
    }

    fn node(enforcer: Arc<dyn Enforcer>) -> Node {
        let identity = Identity::generate();
        let channels = Arc::new(LocalChannels::new(identity.clone()));
        node_with(identity, channels, enforcer)
    }

    fn node_with(
        identity: Identity,
        channels: Arc<dyn ChannelBackend>,
        enforcer: Arc<dyn Enforcer>,
    ) -> Node {
        let config = NodeConfig {
            identity: identity.clone(),
            policy: NodePolicy::default(),
            buyer: BuyerPolicy::default(),
            listen: "127.0.0.1:0".parse().unwrap(),
            peer_identity: PeerIdentity::Address,
            mint_url: "http://127.0.0.1/unused".into(),
            mint_local: "http://127.0.0.1/unused".into(),
            channel_ttl_seconds: 3_600,
            peers: Vec::new(),
            connector: None,
        };
        Node::new(&config, channels, enforcer)
    }

    #[tokio::test]
    async fn a_late_disconnect_leaves_the_connection_that_replaced_it_alone() {
        let registry = Arc::new(Registry::default());
        let mut node = node(registry.clone());
        let (done, _done_rx) = mpsc::channel(256);
        let peer = Identity::generate().pubkey();

        // The peer's first connection, then a second — from another address —
        // before the node has heard the first one end.
        let (first, second) = (Connection::next(), Connection::next());
        let (first_tx, _first_rx) = mpsc::channel(64);
        let (second_tx, mut second_rx) = mpsc::channel(64);
        let second_addr: SocketAddr = "10.0.0.8:40002".parse().unwrap();
        node.on_wire(
            Wire::PeerUp {
                peer,
                conn: first,
                addr: "10.0.0.7:40001".parse().unwrap(),
                tx: first_tx,
            },
            &done,
        )
        .await;
        node.on_wire(
            Wire::PeerUp {
                peer,
                conn: second,
                addr: second_addr,
                tx: second_tx,
            },
            &done,
        )
        .await;
        // The first connection's end arrives late.
        node.on_wire(Wire::PeerDown { peer, conn: first }, &done)
            .await;

        // The second connection is the peer's, whole: a link to send on, a
        // session in core, and the enforcer gating the address it came from.
        assert!(node.links.contains_key(&peer), "the live link was dropped");
        assert!(
            node.sessions.peer(&peer).is_some(),
            "the live session was ended"
        );
        assert_eq!(registry.addr(peer), Some(second_addr.ip()));
        while second_rx.try_recv().is_ok() {}
        node.send(
            peer,
            Message::Disconnect(tollgate_protocol::Disconnect {
                reason: ReasonCode::Other,
            }),
        )
        .await;
        assert!(
            second_rx.try_recv().is_ok(),
            "nothing reaches the live link"
        );

        // Its own end is still the end.
        node.on_wire(Wire::PeerDown { peer, conn: second }, &done)
            .await;
        assert!(!node.links.contains_key(&peer));
        assert!(node.sessions.peer(&peer).is_none());
        assert_eq!(registry.addr(peer), None);
    }

    /// A wallet whose mint can be taken down: funding fails while it is.
    #[derive(Debug)]
    struct Unreachable {
        inner: LocalChannels,
        down: AtomicBool,
    }

    impl ChannelBackend for Unreachable {
        fn fund(&self, peer: PubKey, mint_url: &str, capacity: u64) -> Result<FundedChannel> {
            if self.down.load(Ordering::SeqCst) {
                bail!("mint unreachable");
            }
            self.inner.fund(peer, mint_url, capacity)
        }
        fn verify(&self, peer: PubKey, funding: &[u8]) -> Result<VerifiedChannel> {
            self.inner.verify(peer, funding)
        }
        fn sign_update(&self, channel_id: ChannelId, cumulative: u64) -> Result<Signature> {
            self.inner.sign_update(channel_id, cumulative)
        }
        fn verify_update(
            &self,
            peer: PubKey,
            id: ChannelId,
            cumulative: u64,
            sig: Signature,
        ) -> bool {
            self.inner.verify_update(peer, id, cumulative, sig)
        }
        fn record_update(
            &self,
            peer: PubKey,
            id: ChannelId,
            cumulative: u64,
            sig: Signature,
        ) -> Result<()> {
            self.inner.record_update(peer, id, cumulative, sig)
        }
        fn settle(&self, channel_id: ChannelId) -> Result<()> {
            self.inner.settle(channel_id)
        }
    }

    /// Ask for a channel toward `peer`, as core does, and time the answer.
    async fn fund(
        node: &mut Node,
        peer: PubKey,
        request: u64,
        done: &mpsc::Sender<Event>,
        answers: &mut mpsc::Receiver<Event>,
    ) -> (Event, Duration) {
        let asked = tokio::time::Instant::now();
        node.execute(
            Action::FundChannel {
                peer,
                request,
                mint_url: "http://127.0.0.1/unused".into(),
                capacity: 1_000,
            },
            done,
        )
        .await;
        let answer = answers
            .recv()
            .await
            .expect("the node answers every request");
        (answer, asked.elapsed())
    }

    #[tokio::test(start_paused = true)]
    async fn a_failed_funding_is_reported_after_a_growing_wait() {
        // A mint that is down was asked again on every tick: core asks on the
        // tick after it hears a funding failed, and it heard at once.
        let identity = Identity::generate();
        let wallet = Arc::new(Unreachable {
            inner: LocalChannels::new(identity.clone()),
            down: AtomicBool::new(true),
        });
        let mut node = node_with(identity, wallet.clone(), Arc::new(Loopback::new()));
        let (done, mut answers) = mpsc::channel(16);
        let peer = Identity::generate().pubkey();

        // One second, doubling, and never longer than core would wait on a
        // request that is not answered at all.
        let mut waits = Vec::new();
        for request in 1..=7 {
            let (answer, waited) = fund(&mut node, peer, request, &done, &mut answers).await;
            assert!(matches!(
                answer,
                Event::OutgoingFundingFailed { request: r, .. } if r == request
            ));
            waits.push(waited.as_secs());
        }
        assert_eq!(waits, [1, 2, 4, 8, 16, 30, 30]);

        // Once the mint is back the channel comes back at once, and the next
        // failure starts from a second again.
        wallet.down.store(false, Ordering::SeqCst);
        let (answer, waited) = fund(&mut node, peer, 8, &done, &mut answers).await;
        assert!(matches!(
            answer,
            Event::OutgoingChannelFunded { request: 8, .. }
        ));
        assert_eq!(waited.as_secs(), 0);

        wallet.down.store(true, Ordering::SeqCst);
        let (_, waited) = fund(&mut node, peer, 9, &done, &mut answers).await;
        assert_eq!(waited.as_secs(), 1);

        // Each peer waits on its own failures.
        let (_, waited) = fund(
            &mut node,
            Identity::generate().pubkey(),
            10,
            &done,
            &mut answers,
        )
        .await;
        assert_eq!(waited.as_secs(), 1);
    }
}
