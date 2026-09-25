//! Settling channels until it sticks.
//!
//! Core emits [`Action::SettleChannel`] once and forgets the channel: it has
//! already dropped it from the grant. So a settlement that fails — the mint is
//! briefly unreachable, say — is never asked for again, and after the
//! channel's refund timelock the funder can reclaim all of it, including what
//! it had already paid us. Retrying is the host's job, because it is I/O and
//! time, and core does neither.
//!
//! Each settlement runs as its own task: attempt, and on a transient failure
//! wait out an exponential [`Backoff`] and attempt again, until it succeeds,
//! the backend says no retry can help ([`CannotSettle`]), or a deadline
//! passes. The node's shutdown is one such deadline: it wakes every waiting
//! retry, gives it a short grace period, and abandons what is left rather than
//! holding the process open.
//!
//! Nothing here is persisted: a settlement still being retried when the node
//! stops is forgotten.
//!
//! [`Action::SettleChannel`]: tollgate_core::Action::SettleChannel

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::anyhow;
use tokio::sync::watch;
use tokio::task::JoinSet;
use tokio::time::Instant;
use tollgate_protocol::{ChannelId, PubKey};
use tracing::{debug, info, warn};

use crate::channel::{CannotSettle, ChannelBackend};

/// How long to wait between attempts: doubling from `initial`, capped at `max`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Backoff {
    /// The wait after the first failure.
    pub initial: Duration,
    /// The longest wait, however many failures there have been.
    pub max: Duration,
}

impl Backoff {
    /// One second, doubling to five minutes.
    ///
    /// A mint that blinks is retried almost at once; one that is down for an
    /// hour costs a dozen attempts, not thousands of log lines.
    pub const DEFAULT: Self = Self {
        initial: Duration::from_secs(1),
        max: Duration::from_secs(300),
    };

    /// The wait after `failures` consecutive failures (the first is 1).
    pub fn delay(&self, failures: u32) -> Duration {
        let doublings = failures.saturating_sub(1).min(31);
        self.initial.saturating_mul(1u32 << doublings).min(self.max)
    }

    /// When to try again after `failures` failures, or `None` to give up.
    ///
    /// Without a deadline this never gives up. With one, the wait is clipped so
    /// the last attempt lands on the deadline rather than past it, and once the
    /// deadline has passed there is no next attempt.
    pub fn next(&self, failures: u32, now: Instant, deadline: Option<Instant>) -> Option<Duration> {
        let delay = self.delay(failures);
        match deadline {
            None => Some(delay),
            Some(deadline) if now >= deadline => None,
            Some(deadline) => Some(delay.min(deadline - now)),
        }
    }
}

impl Default for Backoff {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// Runs settlements, retrying the ones that fail.
#[derive(Debug)]
pub(crate) struct Settler {
    channels: Arc<dyn ChannelBackend>,
    backoff: Backoff,
    /// Channels with a settlement running, so a second request for one does
    /// not race the first. Shared with the tasks, which remove themselves.
    in_flight: Arc<Mutex<HashSet<ChannelId>>>,
    tasks: JoinSet<()>,
    /// `Some` once the node is shutting down: the deadline every retry now
    /// works to. Changing it also wakes any retry that is waiting.
    closing: watch::Sender<Option<Instant>>,
}

impl Settler {
    pub(crate) fn new(channels: Arc<dyn ChannelBackend>, backoff: Backoff) -> Self {
        Self {
            channels,
            backoff,
            in_flight: Arc::default(),
            tasks: JoinSet::new(),
            closing: watch::Sender::new(None),
        }
    }

    pub(crate) fn set_backoff(&mut self, backoff: Backoff) {
        self.backoff = backoff;
    }

    /// Settle `channel_id`, retrying until it succeeds.
    ///
    /// `deadline` is the point past which retrying is pointless. Nothing
    /// supplies one yet; a channel's refund expiry is the natural one, since
    /// past it the funder can take the money back.
    ///
    /// A channel already being settled is left to the attempt in progress.
    pub(crate) fn settle(
        &mut self,
        peer: PubKey,
        channel_id: ChannelId,
        deadline: Option<Instant>,
    ) {
        // Reap whatever has finished, so the set holds only live tasks.
        while self.tasks.try_join_next().is_some() {}

        let Some(guard) = InFlight::claim(&self.in_flight, channel_id) else {
            debug!(%peer, ?channel_id, "already settling this channel");
            return;
        };
        let job = Job {
            channels: Arc::clone(&self.channels),
            backoff: self.backoff,
            peer,
            channel_id,
            deadline,
            closing: self.closing.subscribe(),
        };
        self.tasks.spawn(async move {
            let _guard = guard;
            job.run().await;
        });
    }

    /// Stop retrying for long: every settlement gets until `grace` from now.
    ///
    /// Call this before starting the settlements a shutdown asks for, so they
    /// see the deadline too.
    pub(crate) fn begin_shutdown(&self, grace: Duration) {
        self.closing.send_replace(Some(Instant::now() + grace));
    }

    /// Wait for every settlement to finish, abandoning any still running after
    /// `limit`.
    pub(crate) async fn drain(&mut self, limit: Duration) {
        let all = async { while self.tasks.join_next().await.is_some() {} };
        if tokio::time::timeout(limit, all).await.is_err() {
            warn!(
                abandoned = self.tasks.len(),
                "shutting down with channels still unsettled"
            );
            self.tasks.abort_all();
        }
    }
}

/// A channel's place in [`Settler::in_flight`], given up when its task ends
/// however it ends.
struct InFlight {
    set: Arc<Mutex<HashSet<ChannelId>>>,
    channel_id: ChannelId,
}

impl InFlight {
    fn claim(set: &Arc<Mutex<HashSet<ChannelId>>>, channel_id: ChannelId) -> Option<Self> {
        set.lock()
            .expect("not poisoned")
            .insert(channel_id)
            .then(|| Self {
                set: Arc::clone(set),
                channel_id,
            })
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        if let Ok(mut set) = self.set.lock() {
            set.remove(&self.channel_id);
        }
    }
}

/// One channel's settlement, from first attempt to last.
struct Job {
    channels: Arc<dyn ChannelBackend>,
    backoff: Backoff,
    peer: PubKey,
    channel_id: ChannelId,
    deadline: Option<Instant>,
    closing: watch::Receiver<Option<Instant>>,
}

impl Job {
    async fn run(mut self) {
        let (peer, channel_id) = (self.peer, self.channel_id);
        let mut failures: u32 = 0;
        loop {
            // A real backend talks to a mint, so the call goes to a blocking
            // thread rather than stalling the runtime.
            let channels = Arc::clone(&self.channels);
            let result = tokio::task::spawn_blocking(move || channels.settle(channel_id))
                .await
                .unwrap_or_else(|e| Err(anyhow!("settlement task failed: {e}")));

            let error = match result {
                Ok(()) => {
                    if failures > 0 {
                        info!(%peer, ?channel_id, attempts = failures + 1, "settled a channel after retrying");
                    } else {
                        debug!(%peer, ?channel_id, "settled a channel");
                    }
                    return;
                }
                Err(e) => e,
            };
            failures = failures.saturating_add(1);

            if error.downcast_ref::<CannotSettle>().is_some() {
                warn!(%peer, ?channel_id, error = format!("{error:#}"), "cannot settle a channel; not retrying");
                return;
            }

            let deadline = earliest(self.deadline, *self.closing.borrow());
            let Some(delay) = self.backoff.next(failures, Instant::now(), deadline) else {
                warn!(%peer, ?channel_id, attempts = failures, error = format!("{error:#}"), "giving up settling a channel");
                return;
            };
            warn!(
                %peer,
                ?channel_id,
                attempt = failures,
                retry_in = ?delay,
                error = format!("{error:#}"),
                "could not settle a channel; will retry"
            );

            tokio::select! {
                _ = tokio::time::sleep(delay) => {}
                // Shutdown moved the deadline in: stop waiting and try now.
                changed = self.closing.changed() => {
                    if changed.is_err() {
                        // The settler is gone, and the node with it.
                        return;
                    }
                }
            }
        }
    }
}

fn earliest(a: Option<Instant>, b: Option<Instant>) -> Option<Instant> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use anyhow::{Result, bail};
    use tollgate_protocol::Signature;

    use super::*;
    use crate::channel::{FundedChannel, VerifiedChannel};

    #[test]
    fn the_wait_doubles_from_one_second_to_a_five_minute_cap() {
        let b = Backoff::DEFAULT;
        let waits: Vec<u64> = (1..=11).map(|n| b.delay(n).as_secs()).collect();
        assert_eq!(waits, [1, 2, 4, 8, 16, 32, 64, 128, 256, 300, 300]);
    }

    #[test]
    fn the_wait_never_overflows() {
        let b = Backoff::DEFAULT;
        assert_eq!(b.delay(u32::MAX), b.max);
        assert_eq!(b.delay(0), b.initial, "zero failures reads as the first");
    }

    #[test]
    fn without_a_deadline_it_never_gives_up() {
        let now = Instant::now();
        assert_eq!(
            Backoff::DEFAULT.next(1_000, now, None),
            Some(Duration::from_secs(300))
        );
    }

    #[test]
    fn a_deadline_clips_the_wait_and_then_ends_the_retries() {
        let b = Backoff::DEFAULT;
        let now = Instant::now();
        let deadline = now + Duration::from_secs(3);

        // Well inside the deadline, the backoff decides.
        assert_eq!(b.next(1, now, Some(deadline)), Some(Duration::from_secs(1)));
        // The last attempt lands on the deadline, not past it.
        assert_eq!(
            b.next(3, now + Duration::from_secs(1), Some(deadline)),
            Some(Duration::from_secs(2))
        );
        // And after it there is none.
        assert_eq!(b.next(4, deadline, Some(deadline)), None);
    }

    #[test]
    fn the_earlier_deadline_wins() {
        let now = Instant::now();
        let soon = now + Duration::from_secs(1);
        let later = now + Duration::from_secs(9);
        assert_eq!(earliest(Some(later), Some(soon)), Some(soon));
        assert_eq!(earliest(None, Some(later)), Some(later));
        assert_eq!(earliest(None, None), None);
    }

    /// A backend whose settlement fails `failures` times, then succeeds.
    #[derive(Debug)]
    struct Flaky {
        failures: u32,
        permanent: bool,
        calls: AtomicU32,
        settled: AtomicU32,
        /// How long one settlement takes, to hold one in flight.
        latency: Duration,
    }

    impl Flaky {
        fn new(failures: u32) -> Self {
            Self {
                failures,
                permanent: false,
                calls: AtomicU32::new(0),
                settled: AtomicU32::new(0),
                latency: Duration::ZERO,
            }
        }
    }

    impl ChannelBackend for Flaky {
        fn fund(&self, _: PubKey, _: &str, _: u64) -> Result<FundedChannel> {
            unimplemented!()
        }
        fn verify(&self, _: PubKey, _: &[u8]) -> Result<VerifiedChannel> {
            unimplemented!()
        }
        fn sign_update(&self, _: ChannelId, _: u64) -> Result<Signature> {
            unimplemented!()
        }
        fn verify_update(&self, _: PubKey, _: ChannelId, _: u64, _: Signature) -> bool {
            unimplemented!()
        }
        fn record_update(&self, _: PubKey, _: ChannelId, _: u64, _: Signature) -> Result<()> {
            unimplemented!()
        }
        fn settle(&self, _: ChannelId) -> Result<()> {
            std::thread::sleep(self.latency);
            let call = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
            if self.permanent {
                return Err(CannotSettle("never seen it".into()).into());
            }
            if call <= self.failures {
                bail!("mint unreachable");
            }
            self.settled.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    fn fast() -> Backoff {
        Backoff {
            initial: Duration::from_millis(1),
            max: Duration::from_millis(8),
        }
    }

    fn peer() -> PubKey {
        PubKey([2; 33])
    }

    #[tokio::test]
    async fn a_failed_settlement_is_retried_until_it_succeeds() {
        let backend = Arc::new(Flaky::new(4));
        let mut settler = Settler::new(backend.clone(), fast());

        settler.settle(peer(), ChannelId([1; 32]), None);
        settler.drain(Duration::from_secs(5)).await;

        assert_eq!(
            backend.calls.load(Ordering::SeqCst),
            5,
            "four failures, then success"
        );
        assert_eq!(backend.settled.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_permanent_failure_is_not_retried() {
        let backend = Arc::new(Flaky {
            permanent: true,
            ..Flaky::new(0)
        });
        let mut settler = Settler::new(backend.clone(), fast());

        settler.settle(peer(), ChannelId([1; 32]), None);
        settler.drain(Duration::from_secs(5)).await;

        assert_eq!(backend.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_channel_already_settling_is_not_settled_twice_at_once() {
        let backend = Arc::new(Flaky {
            latency: Duration::from_millis(50),
            ..Flaky::new(0)
        });
        let mut settler = Settler::new(backend.clone(), fast());

        settler.settle(peer(), ChannelId([1; 32]), None);
        settler.settle(peer(), ChannelId([1; 32]), None);
        // A different channel is not held up by it.
        settler.settle(peer(), ChannelId([2; 32]), None);
        settler.drain(Duration::from_secs(5)).await;

        assert_eq!(backend.calls.load(Ordering::SeqCst), 2);

        // Once it has finished, asking again is allowed — and harmless, since
        // settling is idempotent.
        settler.settle(peer(), ChannelId([1; 32]), None);
        settler.drain(Duration::from_secs(5)).await;
        assert_eq!(backend.calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn a_deadline_ends_the_retries() {
        let backend = Arc::new(Flaky::new(u32::MAX));
        let mut settler = Settler::new(backend.clone(), fast());

        settler.settle(
            peer(),
            ChannelId([1; 32]),
            Some(Instant::now() + Duration::from_millis(30)),
        );
        settler.drain(Duration::from_secs(5)).await;

        let calls = backend.calls.load(Ordering::SeqCst);
        assert!(calls >= 2, "retried before the deadline: {calls} calls");
    }

    #[tokio::test]
    async fn shutdown_cuts_a_long_wait_short_and_bounds_the_retries() {
        // A backoff whose first wait alone would outlast the test.
        let slow = Backoff {
            initial: Duration::from_secs(3_600),
            max: Duration::from_secs(3_600),
        };
        let backend = Arc::new(Flaky::new(u32::MAX));
        let mut settler = Settler::new(backend.clone(), slow);

        settler.settle(peer(), ChannelId([1; 32]), None);
        // Let the first attempt fail and the retry start waiting.
        while backend.calls.load(Ordering::SeqCst) == 0 {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }

        let started = std::time::Instant::now();
        settler.begin_shutdown(Duration::from_millis(50));
        settler.drain(Duration::from_secs(5)).await;

        assert!(
            started.elapsed() < Duration::from_secs(2),
            "shutdown waited on the backoff"
        );
        assert!(
            backend.calls.load(Ordering::SeqCst) >= 2,
            "shutdown should wake the retry for one more attempt"
        );
    }
}
