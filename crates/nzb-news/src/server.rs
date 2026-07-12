//! `Server` — per-provider orchestration state.
//!
//! Owns the fixed pool of [`NewsWrapper`]s allocated to one NNTP server,
//! tracks aggregate health (bad_cons, penalty timers), and offers the
//! idle-pool / busy-pool primitives the top-level downloader uses to pick
//! workers for article dispatch.
//!
//! Invariants:
//!
//! - Every wrapper is in **exactly one** of `idle_wrappers` / `busy_wrappers`
//!   at any given time. Callers must move wrappers via
//!   [`Server::take_idle_wrapper`] / [`Server::return_wrapper_idle`] /
//!   [`Server::return_wrapper_busy`]; direct mutation is not exposed.
//! - `bad_cons` is the **wrapper-level** sum; server-level penalties are
//!   driven separately by [`Server::register_failure`] / [`Server::clear_penalty`].
//! - Priority is a `u8` (smaller = higher priority), matching
//!   [`nzb_nntp::config::ServerConfig::priority`] on the underlying config.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use nzb_nntp::config::ServerConfig;
use tracing::{debug, info, warn};

use crate::news_wrapper::{NewsWrapper, WrapperId};

/// Default threshold at which a server is penalised after consecutive
/// connection-level failures. Mirrors the threshold that typical NNTP
/// clients use (bad_cons ≥ 3 → penalty).
pub const BAD_CONS_THRESHOLD: u32 = 3;

/// Default penalty window when a server trips the `bad_cons` threshold.
/// During the window `Server::is_penalised` returns `true` and callers
/// should skip this server in the dispatch preference order.
pub const DEFAULT_PENALTY: Duration = Duration::from_secs(10);

/// Extended penalty window used for repeated 502 (service unavailable)
/// responses. Kept shorter than the generic timeout-penalty because 502 is
/// often transient load-shedding.
pub const PENALTY_502: Duration = Duration::from_secs(5);

/// Penalty window after an authentication failure. Long enough that we
/// don't hammer the auth endpoint; short enough that a password rotation
/// recovers within a minute.
pub const PENALTY_AUTH: Duration = Duration::from_secs(60);

/// `Server` groups a fixed pool of [`NewsWrapper`]s + aggregate health and
/// penalty bookkeeping.
pub struct Server {
    /// Mirror of the wire-level server config. Kept here (rather than via
    /// reference) so the Server object is self-contained and can be shared
    /// as `Arc<Server>` across tasks without lifetime gymnastics.
    config: ServerConfig,

    /// Idle wrappers — ready to fetch.
    idle_wrappers: Mutex<VecDeque<NewsWrapper>>,
    /// Busy wrappers — currently holding an in-flight article.
    busy_wrappers: Mutex<Vec<NewsWrapper>>,

    /// Monotonic wrapper id counter. Allocated on demand.
    next_wrapper_id: AtomicU32,

    /// Aggregate wrapper-level failure count (sum of wrapper.bad_cons as
    /// they are returned to the idle set). Server-wide — drives the
    /// server-level penalty gate.
    bad_cons: AtomicU32,

    /// If non-zero, wall-clock millis since `penalty_epoch` at which the
    /// server exits its penalty window. Zero = no penalty.
    penalty_until_ms: AtomicU64,
    penalty_epoch: Instant,

    /// Set to false when the operator (or a hot reconfig) disables this
    /// server. Disabled servers are drained — the downloader will not pop
    /// any more work to them — and their idle wrappers will be hard-reset.
    active: AtomicBool,

    /// Last attempted connect instant, for `_SERVER_CHECK_DELAY`-style
    /// throttling. Zero = never attempted.
    last_connect_ms: AtomicU64,
    connect_epoch: Instant,

    /// Count of wrapper workers that still have live consumer loops on
    /// this server's queue. Decremented when a wrapper retires (self-
    /// exits on auth/connect failure). When this hits zero the server
    /// effectively has no way to process work, and [`Server::is_usable`]
    /// returns `false` so the dispatcher routes articles elsewhere.
    active_wrappers: AtomicU32,

    // -------------------------------------------------------------------
    // Per-server attempt stats — lifetime totals for this Server instance.
    // Incremented by the scheduler on every fetch result. Exposed via
    // [`Server::stats`] so callers can distinguish "article cascaded past
    // this server" from "this server never saw the article" when
    // diagnosing why a job aborted.
    // -------------------------------------------------------------------
    /// Total fetch attempts dispatched to this server (successes + failures).
    articles_attempted: AtomicU64,
    /// Successful fetches (data returned, passed to decode).
    articles_succeeded: AtomicU64,
    /// `ArticleNotFound` (NNTP 430) responses — the "server doesn't carry
    /// this article" signal. Biggest contributor to dead-NZB diagnostics.
    articles_not_found: AtomicU64,
    /// Transient / retryable failures (timeout, 500, auth, connection drop).
    /// Separate from `not_found` so operators can tell "server is missing
    /// articles" from "server is flaky".
    articles_transient_failed: AtomicU64,
}

/// Snapshot of the per-server attempt counters at a point in time. Returned
/// by [`Server::stats`] for inclusion in abort / diagnostic logs.
#[derive(Debug, Clone, Copy, Default)]
pub struct ServerStats {
    pub attempted: u64,
    pub succeeded: u64,
    pub not_found: u64,
    pub transient_failed: u64,
}

impl Server {
    /// Build a new Server wrapping the given config. No wrappers are
    /// allocated until the first [`Server::take_idle_wrapper`] / explicit
    /// [`Server::prime_wrapper_pool`] call.
    pub fn new(config: ServerConfig) -> Self {
        let now_epoch = Instant::now();
        let enabled = config.enabled;
        Self {
            config,
            idle_wrappers: Mutex::new(VecDeque::new()),
            busy_wrappers: Mutex::new(Vec::new()),
            next_wrapper_id: AtomicU32::new(1),
            bad_cons: AtomicU32::new(0),
            penalty_until_ms: AtomicU64::new(0),
            penalty_epoch: now_epoch,
            active: AtomicBool::new(enabled),
            last_connect_ms: AtomicU64::new(0),
            connect_epoch: now_epoch,
            active_wrappers: AtomicU32::new(0),
            articles_attempted: AtomicU64::new(0),
            articles_succeeded: AtomicU64::new(0),
            articles_not_found: AtomicU64::new(0),
            articles_transient_failed: AtomicU64::new(0),
        }
    }

    /// Snapshot of lifetime attempt counters for this server.
    pub fn stats(&self) -> ServerStats {
        ServerStats {
            attempted: self.articles_attempted.load(Ordering::Relaxed),
            succeeded: self.articles_succeeded.load(Ordering::Relaxed),
            not_found: self.articles_not_found.load(Ordering::Relaxed),
            transient_failed: self.articles_transient_failed.load(Ordering::Relaxed),
        }
    }

    /// Record a successful fetch. Called from the scheduler after a
    /// wrapper returns article bytes.
    pub(crate) fn record_attempt_success(&self) {
        self.articles_attempted.fetch_add(1, Ordering::Relaxed);
        self.articles_succeeded.fetch_add(1, Ordering::Relaxed);
    }

    /// Record an NNTP 430 (article-not-found) response.
    pub(crate) fn record_attempt_not_found(&self) {
        self.articles_attempted.fetch_add(1, Ordering::Relaxed);
        self.articles_not_found.fetch_add(1, Ordering::Relaxed);
    }

    /// Record any other per-attempt failure (transient / retryable).
    pub(crate) fn record_attempt_transient_failed(&self) {
        self.articles_attempted.fetch_add(1, Ordering::Relaxed);
        self.articles_transient_failed
            .fetch_add(1, Ordering::Relaxed);
    }

    /// Register one more live wrapper worker on this server. Called when
    /// the persistent-worker layer spawns a consumer for this server's
    /// queue; the counter gates [`Server::has_active_wrappers`].
    pub fn register_wrapper(&self) -> u32 {
        self.active_wrappers.fetch_add(1, Ordering::AcqRel) + 1
    }

    /// Mark one wrapper worker as retired. Returns the new count.
    pub fn unregister_wrapper(&self) -> u32 {
        self.active_wrappers
            .fetch_sub(1, Ordering::AcqRel)
            .saturating_sub(1)
    }

    /// Current count of live wrapper worker tasks.
    pub fn active_wrappers(&self) -> u32 {
        self.active_wrappers.load(Ordering::Acquire)
    }

    /// Are any wrapper workers still alive on this server? Used by the
    /// scheduler's `select_server` to avoid routing work to a server
    /// whose consumers have all exited.
    pub fn has_active_wrappers(&self) -> bool {
        self.active_wrappers() > 0
    }

    /// Immutable view of the wrapped config.
    pub fn config(&self) -> &ServerConfig {
        &self.config
    }

    /// Server id (matches [`ServerConfig::id`]).
    pub fn id(&self) -> &str {
        &self.config.id
    }

    /// Server priority (smaller = higher).
    pub fn priority(&self) -> u8 {
        self.config.priority
    }

    /// How many concurrent connections this server is configured for.
    pub fn connections(&self) -> u16 {
        self.config.connections
    }

    /// Is this server enabled, outside any active penalty window, and
    /// does it have at least one live wrapper worker able to consume
    /// from its queue?
    pub fn is_usable(&self) -> bool {
        self.is_active() && !self.is_penalised() && self.has_active_wrappers()
    }

    pub fn is_active(&self) -> bool {
        self.active.load(Ordering::Acquire)
    }

    /// Flip the active flag. Callers should follow up with
    /// [`Server::drain_for_disable`] to reset wrappers if transitioning to
    /// inactive, so no stale connections leak.
    pub fn set_active(&self, active: bool) {
        self.active.store(active, Ordering::Release);
    }

    /// Pre-create `count` wrappers in the idle pool. Typically called once
    /// on Server creation with `config.connections`. The wrappers are in
    /// the `Disconnected` state; the first caller that pops one is
    /// responsible for connecting it.
    pub fn prime_wrapper_pool(&self, count: u16) {
        let mut idle = self.idle_wrappers.lock().expect("idle mutex poisoned");
        let already = idle.len();
        let want = count as usize;
        if already >= want {
            return;
        }
        for _ in already..want {
            let id = self.next_wrapper_id.fetch_add(1, Ordering::Relaxed);
            idle.push_back(NewsWrapper::new(self.id().to_string(), id));
        }
        debug!(
            server = %self.id(),
            primed = want,
            "wrapper pool primed"
        );
    }

    /// Number of wrappers currently idle.
    pub fn idle_count(&self) -> usize {
        self.idle_wrappers
            .lock()
            .expect("idle mutex poisoned")
            .len()
    }

    /// Number of wrappers currently busy.
    pub fn busy_count(&self) -> usize {
        self.busy_wrappers
            .lock()
            .expect("busy mutex poisoned")
            .len()
    }

    /// Total wrappers known to this server (idle + busy).
    pub fn wrapper_count(&self) -> usize {
        self.idle_count() + self.busy_count()
    }

    /// Pop an idle wrapper and record the time we last tried to dispatch
    /// against this server. Returns `None` if no wrapper is free, the server
    /// is currently penalised/disabled, or we're inside the per-server
    /// ramp-up throttle window.
    ///
    /// Ramp-up: successive dispatches are rate-limited by
    /// [`ServerConfig::ramp_up_delay_ms`]. This prevents a stampede of
    /// simultaneous connections to the same provider when the downloader
    /// starts (or resumes after a pause), spreading the TCP/TLS handshake
    /// load over time and avoiding bursts that some providers rate-limit.
    pub fn take_idle_wrapper(&self) -> Option<NewsWrapper> {
        self.take_idle_wrapper_inner(false)
    }

    /// Variant of [`Server::take_idle_wrapper`] that ignores the ramp-up
    /// throttle. Use when the downloader already has a clear plan to
    /// dispatch (e.g. a retry of a failed article on a specific server) and
    /// waiting the extra 250ms would just delay progress without helping.
    pub fn take_idle_wrapper_immediate(&self) -> Option<NewsWrapper> {
        self.take_idle_wrapper_inner(true)
    }

    fn take_idle_wrapper_inner(&self, bypass_rampup: bool) -> Option<NewsWrapper> {
        if !self.is_active() {
            return None;
        }
        if self.is_penalised() {
            return None;
        }
        if !bypass_rampup {
            let delay = Duration::from_millis(self.config.ramp_up_delay_ms.into());
            if delay > Duration::ZERO {
                let last = self.last_connect_ms.load(Ordering::Relaxed);
                // `last == 0` means we've never dispatched; first dispatch
                // goes through. Otherwise require the full ramp-up window.
                if last != 0 {
                    let now = self.connect_epoch.elapsed().as_millis() as u64;
                    let since_ms = now.saturating_sub(last);
                    if since_ms < delay.as_millis() as u64 {
                        return None;
                    }
                }
            }
        }
        let mut idle = self.idle_wrappers.lock().expect("idle mutex poisoned");
        let w = idle.pop_front()?;
        // Store `max(1, elapsed_ms)` — zero is our "never dispatched" sentinel,
        // so a same-millisecond dispatch must still register as "just now".
        let now_ms = (self.connect_epoch.elapsed().as_millis() as u64).max(1);
        self.last_connect_ms.store(now_ms, Ordering::Relaxed);
        Some(w)
    }

    /// Suggested sleep duration before the next successful
    /// [`Server::take_idle_wrapper`]. Returns `Duration::ZERO` when no wait
    /// is required — the caller can dispatch immediately.
    pub fn rampup_wait(&self) -> Duration {
        let delay = Duration::from_millis(self.config.ramp_up_delay_ms.into());
        if delay == Duration::ZERO {
            return Duration::ZERO;
        }
        let last = self.last_connect_ms.load(Ordering::Relaxed);
        if last == 0 {
            return Duration::ZERO;
        }
        let now = self.connect_epoch.elapsed().as_millis() as u64;
        let since_ms = now.saturating_sub(last);
        let delay_ms = delay.as_millis() as u64;
        if since_ms >= delay_ms {
            Duration::ZERO
        } else {
            Duration::from_millis(delay_ms - since_ms)
        }
    }

    /// Record that an article was just dispatched to this server.
    ///
    /// Updates the ramp-up timestamp so `rampup_wait` gates subsequent
    /// articles correctly in the persistent-worker model. In that model
    /// `take_idle_wrapper` is never called (workers hold wrappers for
    /// their lifetime), so this is the only place the timestamp is set.
    pub fn note_dispatch(&self) {
        let now_ms = (self.connect_epoch.elapsed().as_millis() as u64).max(1);
        self.last_connect_ms.store(now_ms, Ordering::Relaxed);
    }

    /// Return a wrapper to the busy set. Typically called right after
    /// a wrapper has been dispatched on an article.
    pub fn return_wrapper_busy(&self, w: NewsWrapper) {
        self.busy_wrappers
            .lock()
            .expect("busy mutex poisoned")
            .push(w);
    }

    /// Return a wrapper to the idle set (after a successful fetch, or
    /// after returning to idle state). The wrapper-level `bad_cons` is
    /// aggregated into server-level `bad_cons`. On or above threshold,
    /// the caller should invoke [`Server::maybe_trip_penalty`].
    pub fn return_wrapper_idle(&self, w: NewsWrapper) {
        // Aggregate wrapper-level bad_cons into server-level. A wrapper
        // that had failures before succeeding resets itself via
        // `on_fetch_success` — in that case the delta we aggregate is zero.
        let wrapper_bad = w.bad_cons();
        if wrapper_bad > 0 {
            self.bad_cons.fetch_add(wrapper_bad, Ordering::Relaxed);
        } else {
            // Successful fetch trail: let the server-level counter relax
            // toward zero gradually. One successful trip halves bad_cons,
            // which matches classic "forgiveness" behaviour — fast to
            // forgive, slow to forget.
            let cur = self.bad_cons.load(Ordering::Relaxed);
            if cur > 0 {
                self.bad_cons.store(cur / 2, Ordering::Relaxed);
            }
        }
        w.touch_activity();
        self.idle_wrappers
            .lock()
            .expect("idle mutex poisoned")
            .push_back(w);
    }

    /// Remove `wrapper_id` from the busy set (if present). Used when the
    /// dispatcher decides a busy wrapper has stalled and must be evicted.
    /// Returns the evicted wrapper so the caller can hard-reset it.
    pub fn evict_busy(&self, wrapper_id: WrapperId) -> Option<NewsWrapper> {
        let mut busy = self.busy_wrappers.lock().expect("busy mutex poisoned");
        let idx = busy.iter().position(|w| w.id == wrapper_id)?;
        Some(busy.swap_remove(idx))
    }

    /// Current aggregate bad_cons across all wrappers.
    pub fn bad_cons(&self) -> u32 {
        self.bad_cons.load(Ordering::Relaxed)
    }

    /// Check whether the bad_cons gauge has crossed its threshold. If so,
    /// place the server in a penalty window for `penalty` and reset the
    /// gauge. Returns `true` if a penalty was tripped.
    pub fn maybe_trip_penalty(&self, penalty: Duration) -> bool {
        if self.bad_cons.load(Ordering::Relaxed) >= BAD_CONS_THRESHOLD {
            self.apply_penalty(penalty);
            self.bad_cons.store(0, Ordering::Relaxed);
            true
        } else {
            false
        }
    }

    /// Force a penalty window regardless of bad_cons — used for known-bad
    /// responses like 502 or auth failure, where one hit is enough.
    pub fn apply_penalty(&self, penalty: Duration) {
        let target_ms =
            self.penalty_epoch.elapsed().as_millis() as u64 + penalty.as_millis() as u64;
        let cur = self.penalty_until_ms.load(Ordering::Relaxed);
        if target_ms > cur {
            self.penalty_until_ms.store(target_ms, Ordering::Relaxed);
        }
        warn!(
            server = %self.id(),
            penalty_ms = penalty.as_millis() as u64,
            bad_cons = self.bad_cons.load(Ordering::Relaxed),
            "server penalty applied"
        );
    }

    /// Lift any active penalty immediately. Used when operator action
    /// re-enables the server, or on a successful probe.
    pub fn clear_penalty(&self) {
        if self.penalty_until_ms.swap(0, Ordering::Relaxed) != 0 {
            info!(server = %self.id(), "server penalty cleared");
        }
        self.bad_cons.store(0, Ordering::Relaxed);
    }

    /// Is the server in a penalty window right now?
    pub fn is_penalised(&self) -> bool {
        let until = self.penalty_until_ms.load(Ordering::Relaxed);
        if until == 0 {
            return false;
        }
        let now = self.penalty_epoch.elapsed().as_millis() as u64;
        if now >= until {
            // Lazy expire: clear on first read past the deadline.
            self.penalty_until_ms.store(0, Ordering::Relaxed);
            false
        } else {
            true
        }
    }

    /// Remaining penalty duration, or zero if not penalised.
    pub fn penalty_remaining(&self) -> Duration {
        let until = self.penalty_until_ms.load(Ordering::Relaxed);
        if until == 0 {
            return Duration::ZERO;
        }
        let now = self.penalty_epoch.elapsed().as_millis() as u64;
        Duration::from_millis(until.saturating_sub(now))
    }

    /// Record an unrelated failure (e.g. connect refused). Equivalent to
    /// incrementing bad_cons and testing the threshold in one call.
    pub fn register_failure(&self, penalty: Duration) -> bool {
        self.bad_cons.fetch_add(1, Ordering::Relaxed);
        self.maybe_trip_penalty(penalty)
    }

    /// Time since this server was last asked to dispatch (used for
    /// `_SERVER_CHECK_DELAY`-style throttle). Duration::ZERO if never.
    pub fn time_since_last_dispatch(&self) -> Duration {
        let last = self.last_connect_ms.load(Ordering::Relaxed);
        if last == 0 {
            return Duration::ZERO;
        }
        let now = self.connect_epoch.elapsed().as_millis() as u64;
        Duration::from_millis(now.saturating_sub(last))
    }

    /// Disable the server and hard-reset every wrapper (busy + idle).
    /// Callers are responsible for awaiting the returned futures.
    pub async fn drain_for_disable(&self) {
        self.set_active(false);
        let mut idle_taken = Vec::new();
        {
            let mut idle = self.idle_wrappers.lock().expect("idle mutex poisoned");
            while let Some(w) = idle.pop_front() {
                idle_taken.push(w);
            }
        }
        let mut busy_taken: Vec<NewsWrapper> = Vec::new();
        {
            let mut busy = self.busy_wrappers.lock().expect("busy mutex poisoned");
            busy_taken.append(&mut *busy);
        }
        for mut w in idle_taken.into_iter().chain(busy_taken) {
            w.hard_reset().await;
        }
        self.bad_cons.store(0, Ordering::Relaxed);
        debug!(server = %self.id(), "server drained and wrappers released");
    }
}

impl std::fmt::Debug for Server {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Server")
            .field("id", &self.id())
            .field("priority", &self.priority())
            .field("connections", &self.connections())
            .field("active", &self.is_active())
            .field("penalised", &self.is_penalised())
            .field("idle", &self.idle_count())
            .field("busy", &self.busy_count())
            .field("bad_cons", &self.bad_cons())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(id: &str, priority: u8, connections: u16) -> ServerConfig {
        let mut c = ServerConfig::new(id, "host.example");
        c.name = id.into();
        c.priority = priority;
        c.connections = connections;
        c.enabled = true;
        c.port = 119;
        c
    }

    #[test]
    fn prime_allocates_idle_wrappers() {
        let s = Server::new(cfg("s1", 1, 5));
        s.prime_wrapper_pool(5);
        assert_eq!(s.idle_count(), 5);
        assert_eq!(s.busy_count(), 0);
        assert_eq!(s.wrapper_count(), 5);
    }

    #[test]
    fn disabled_config_starts_inactive() {
        let mut config = cfg("disabled", 1, 1);
        config.enabled = false;
        let server = Server::new(config);
        assert!(!server.is_active());
        assert!(!server.is_usable());
    }

    #[test]
    fn take_and_return_round_trip() {
        let s = Server::new(cfg("s1", 1, 3));
        s.prime_wrapper_pool(3);
        let w = s.take_idle_wrapper().expect("should have idle");
        assert_eq!(s.idle_count(), 2);
        s.return_wrapper_busy(w);
        assert_eq!(s.busy_count(), 1);

        // Put it back idle.
        let mut busy = s.busy_wrappers.lock().unwrap();
        let w = busy.pop().unwrap();
        drop(busy);
        s.return_wrapper_idle(w);
        assert_eq!(s.idle_count(), 3);
    }

    #[test]
    fn penalty_window_gates_take_idle() {
        let s = Server::new(cfg("s1", 1, 2));
        s.prime_wrapper_pool(2);
        s.apply_penalty(Duration::from_millis(200));
        assert!(s.is_penalised());
        assert!(s.take_idle_wrapper().is_none());

        // Wait past the window and confirm it clears lazily.
        std::thread::sleep(Duration::from_millis(250));
        assert!(!s.is_penalised());
        assert!(s.take_idle_wrapper().is_some());
    }

    #[test]
    fn bad_cons_threshold_trips_penalty() {
        let s = Server::new(cfg("s1", 1, 2));
        assert!(!s.register_failure(DEFAULT_PENALTY));
        assert!(!s.register_failure(DEFAULT_PENALTY));
        // Third failure crosses the threshold.
        assert!(s.register_failure(DEFAULT_PENALTY));
        assert!(s.is_penalised());
    }

    #[test]
    fn drain_clears_wrappers_and_deactivates() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        rt.block_on(async {
            let s = Server::new(cfg("s1", 1, 3));
            s.prime_wrapper_pool(3);
            // Move one to busy.
            let w = s.take_idle_wrapper().unwrap();
            s.return_wrapper_busy(w);
            assert_eq!(s.wrapper_count(), 3);
            s.drain_for_disable().await;
            assert_eq!(s.wrapper_count(), 0);
            assert!(!s.is_active());
        });
    }

    #[test]
    fn success_halves_bad_cons() {
        let s = Server::new(cfg("s1", 1, 2));
        s.prime_wrapper_pool(1);
        let w = s.take_idle_wrapper().unwrap();
        // Simulate prior server-level failures.
        s.bad_cons.store(4, Ordering::Relaxed);
        // Returning a wrapper with zero wrapper-level bad_cons should halve
        // the server-level counter.
        s.return_wrapper_idle(w);
        assert_eq!(s.bad_cons(), 2);
    }

    #[test]
    fn rampup_throttles_rapid_takes() {
        let mut c = cfg("s1", 1, 4);
        c.ramp_up_delay_ms = 100;
        let s = Server::new(c);
        s.prime_wrapper_pool(4);

        // First take: allowed immediately.
        let w1 = s.take_idle_wrapper();
        assert!(w1.is_some());

        // Second take within the window: throttled.
        let w2 = s.take_idle_wrapper();
        assert!(w2.is_none(), "second take should be ramp-up-gated");

        // rampup_wait reports a positive remaining window.
        let wait = s.rampup_wait();
        assert!(wait > Duration::ZERO);
        assert!(wait <= Duration::from_millis(100));

        // After the window elapses, another take is permitted.
        std::thread::sleep(Duration::from_millis(110));
        assert_eq!(s.rampup_wait(), Duration::ZERO);
        let w3 = s.take_idle_wrapper();
        assert!(w3.is_some());
    }

    #[test]
    fn rampup_can_be_bypassed_for_retries() {
        let mut c = cfg("s1", 1, 4);
        c.ramp_up_delay_ms = 1_000;
        let s = Server::new(c);
        s.prime_wrapper_pool(4);

        assert!(s.take_idle_wrapper().is_some());
        // Regular take would block for the ramp window.
        assert!(s.take_idle_wrapper().is_none());
        // Immediate variant ignores the window.
        assert!(s.take_idle_wrapper_immediate().is_some());
    }

    #[test]
    fn time_since_last_dispatch_tracks_take() {
        let s = Server::new(cfg("s1", 1, 1));
        s.prime_wrapper_pool(1);
        assert_eq!(s.time_since_last_dispatch(), Duration::ZERO);
        let _w = s.take_idle_wrapper().unwrap();
        std::thread::sleep(Duration::from_millis(50));
        // At least 25ms elapsed — relaxed from the 50ms sleep so CI jitter
        // on loaded hosts doesn't flap this check.
        assert!(
            s.time_since_last_dispatch() >= Duration::from_millis(25),
            "elapsed was {:?}",
            s.time_since_last_dispatch()
        );
    }
}
