//! Download engine — shared NNTP worker pool that services all active jobs.
//!
//! Architecture:
//! - A single long-lived [`WorkerPool`] is owned by [`crate::queue_manager::QueueManager`].
//! - For each enabled server, exactly `server.connections` workers are
//!   spawned and live as long as the server stays enabled. When the server
//!   list or per-server connection limit changes, the pool reconciles.
//! - Jobs register a [`JobContext`] and push their work items into a
//!   [`SharedWorkQueue`]. Workers pop items tagged with `job_id` and look up
//!   per-job state (assembler, progress sink, pause/cancel flags) via the
//!   shared [`JobContextMap`].
//! - Pause / cancel / completion are per-job flags; workers themselves are
//!   never torn down on job transitions. Pausing a job causes workers holding
//!   one of its items to return that item to the queue and pull something
//!   else. Cancelling a job drains its items and drops in-flight results.
//! - Transient provider outages leave work queued until circuit-breaker
//!   cooldown expires; only definitive provider outcomes resolve an article.
//!
//! Retry logic (per article):
//! 1. Try the article on the current server up to [`MAX_TRIES_PER_SERVER`]
//!    times, reconnecting on transient errors.
//! 2. On `ArticleNotFound` (430) — requeue with the current server added to
//!    `tried_servers`; another worker on a different server picks it up.
//! 3. On connection loss — requeue and reconnect.
//! 4. On decode error — treated like "not available on this server", try
//!    another.
//! 5. An article is missing only when every enabled provider explicitly
//!    returned `430`. Circuit-broken and transiently failing providers do
//!    not contribute evidence of absence.
//! 6. A job only fails if failed articles exceed the threshold and no par2
//!    recovery is possible.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use tokio::sync::{Notify, mpsc};
use tokio::task::JoinHandle;
use tracing::{debug, error, info, trace, warn};

use nzb_core::config::ServerConfig;
use nzb_core::models::NzbJob;
use nzb_decode::FileAssembler;
use nzb_decode::yenc::decode_yenc;
use nzb_nntp::Pipeline;
use nzb_nntp::budget::{BudgetPermit, BudgetRegistry, BudgetTier, ServerBudget};
use nzb_nntp::connection::{ConnectionState, NntpConnection};
use nzb_nntp::error::NntpError;

use crate::bandwidth::BandwidthLimiter;

fn increment_counter(name: &'static str) {
    opentelemetry::global::meter_provider()
        .meter("nzb-dispatch")
        .u64_counter(name)
        .build()
        .add(1, &[]);
}

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Max times to retry an article on the SAME server before trying the next.
const MAX_TRIES_PER_SERVER: u32 = 3;
/// How many timeouts, dropped connections or unclassified errors one article
/// absorbs on a server that is otherwise up, before the error counts as that
/// server's definitive outcome. A server-wide outage (502, 480, 403) is not
/// counted: the circuit breaker handles those.
const MAX_OUTAGE_TRIES_PER_SERVER: u32 = 10;
/// Delay between reconnection attempts.
const RECONNECT_DELAY: Duration = Duration::from_secs(5);
/// Max reconnect attempts before giving up on a server for this session.
const MAX_RECONNECT_ATTEMPTS: u32 = 5;
/// Stagger delay between worker initial connections to avoid thundering herd.
/// Each worker waits conn_idx * WORKER_RAMP_DELAY before its first connect.
const WORKER_RAMP_DELAY: Duration = Duration::from_millis(15);
/// Consecutive connect failures before circuit-breaking a server.
const CIRCUIT_BREAK_THRESHOLD: u32 = 3;
/// Cooldown after auth/permission failure (bad credentials, 502, account blocked).
const AUTH_FAILURE_COOLDOWN: Duration = Duration::from_secs(120);
/// Cooldown after transient connection failures exceed threshold.
const TRANSIENT_FAILURE_COOLDOWN: Duration = Duration::from_secs(30);
/// Supervisor tick interval for detecting stuck jobs.
const SUPERVISOR_INTERVAL: Duration = Duration::from_secs(1);

/// Default maximum idle time before a worker is evicted by the supervisor.
/// Tunable per pool via [`WorkerPool::set_max_worker_idle`]. Production code
/// uses the default; the test harness shrinks this to make Phase 5 tests
/// converge in seconds rather than minutes.
const DEFAULT_MAX_WORKER_IDLE: Duration = Duration::from_secs(60);
/// Worker idle poll interval when the shared queue is empty.
const WORKER_IDLE_POLL: Duration = Duration::from_millis(500);
/// How often an idle worker checks that its socket is still open. Providers
/// close idle sessions on their side; without a probe the worker would only
/// notice at its next fetch, and the open-socket count would stay high.
/// Tunable per pool via [`WorkerPool::set_idle_keepalive`].
const DEFAULT_IDLE_KEEPALIVE: Duration = Duration::from_secs(30);
/// How long a worker with nothing to fetch keeps its socket (and its share
/// of the server's connection budget) before closing it. A waiter at another
/// tier that could take the permit gets it sooner, as soon as the worker sees
/// it. Closing idle sockets frees the provider's connection slots and keeps
/// the open-socket count honest when the queue is empty, as SABnzbd does.
/// Tunable per pool via [`WorkerPool::set_idle_release`].
const DEFAULT_IDLE_RELEASE: Duration = Duration::from_secs(30);
/// Upper bound on one idle keepalive round trip.
const IDLE_KEEPALIVE_TIMEOUT: Duration = Duration::from_secs(10);
/// Message-id used by the idle keepalive `STAT`. The provider answers 430,
/// which proves the session is alive without transferring an article.
const IDLE_KEEPALIVE_MESSAGE_ID: &str = "rustnzb-keepalive@invalid";

/// Phase 7: capacity of the per-job progress channel. The handler reads
/// this at ~articles per second; under DB-lock contention or
/// post-processing pauses it can fall behind. With this cap, the worst
/// case is bounded buffering plus a `WARN` from
/// [`try_send_progress`] when the channel is full.
pub const PROGRESS_CHANNEL_CAPACITY: usize = 10_000;

/// Send a progress update to the per-job channel. On `Full` (handler
/// backpressure), log a warning and drop the update — the alternative is
/// awaiting and stalling the worker, which would cascade into the entire
/// download pipeline. On `Closed` (handler shut down), drop silently.
///
/// Workers should call this through their `JobContext` rather than
/// touching `progress_tx` directly.
fn try_send_progress(tx: &mpsc::Sender<ProgressUpdate>, job_id: &str, update: ProgressUpdate) {
    if let Err(e) = tx.try_send(update) {
        match e {
            mpsc::error::TrySendError::Full(_) => {
                warn!(
                    job_id,
                    capacity = PROGRESS_CHANNEL_CAPACITY,
                    "Progress channel full — dropping update (handler backpressure)"
                );
            }
            mpsc::error::TrySendError::Closed(_) => {
                // Handler has shut down. Workers can't recover this state;
                // dropping is correct.
            }
        }
    }
}

/// Extra rounds an article refused by every server gets before it fails, so
/// each server is asked `1 + CONFIRM_ASKS_PER_SERVER` times. A provider is
/// known to answer `430` for an article it holds while busy, and the damage
/// ledger only confirms a hole from refusals spread past
/// [`nzb_core::damage_ledger::HOLE_CONFIRM_DELAY_SECS`]. One fast round
/// across all servers never is.
pub const CONFIRM_ASKS_PER_SERVER: u32 = 2;

/// Wait before each confirm round. Longer than the ledger's busy-provider
/// guard, so the confirming refusal counts.
const CONFIRM_ROUND_DELAY: Duration =
    Duration::from_millis(nzb_core::damage_ledger::HOLE_CONFIRM_DELAY_SECS as u64 * 1000 + 250);

/// Report one server attempt's outcome for `item` to the queue layer.
fn send_outcome(ctx: &JobContext, item: &WorkItem, outcome: nzb_core::Outcome) {
    try_send_progress(
        &ctx.progress_tx,
        &item.job_id,
        ProgressUpdate::ArticleOutcome {
            job_id: item.job_id.clone(),
            file_id: item.file_id.clone(),
            segment_number: item.segment_number,
            message_id: item.message_id.clone(),
            outcome,
        },
    );
}

fn transport(server: &ServerConfig) -> nzb_core::Outcome {
    nzb_core::Outcome::Transport {
        server: server.id.clone(),
    }
}

fn refused(server: &ServerConfig) -> nzb_core::Outcome {
    nzb_core::Outcome::Refused {
        server: server.id.clone(),
    }
}

// ---------------------------------------------------------------------------
// Global connection tracking — permits from the shared per-server budget
// ---------------------------------------------------------------------------

/// Tracks the downloader's share of each server's NNTP connection budget.
/// Each server has a [`ServerBudget`] (from a [`BudgetRegistry`] shared with
/// the WebDAV side) whose allowance is the configured `connections` limit.
/// Workers acquire a [`BudgetTier::Download`] permit *before* connecting and
/// hold it while they have a socket; an idle worker closes its socket and
/// gives the permit back, so an idle downloader holds none of the allowance.
/// The permit's `Drop` releases it synchronously back to the budget. Once the
/// allowance is out, across the downloader and every other consumer of the
/// budget, the next acquire awaits.
///
/// **Limit changes at runtime** are handled differently for grow vs shrink:
///
/// - **Grow** (e.g. 5 → 10): the budget's allowance is raised in place;
///   existing slot holders are unaffected.
/// - **Shrink** (e.g. 10 → 5): the budget's allowance is lowered and the
///   `ServerSlot` is replaced with a new generation. Old permit holders keep
///   counting against the budget until they drop, so the provider never sees
///   more than the new limit plus the workers still winding down, and no new
///   permit is granted until the total is back under the limit. Workers
///   detect the replacement via [`ConnectionTracker::slot_is_current`] and
///   exit on the next iteration.
pub struct ConnectionTracker {
    pools: Mutex<HashMap<String, ServerSlot>>,
    budgets: Arc<BudgetRegistry>,
    next_generation: AtomicU64,
}

#[derive(Clone)]
struct ServerSlot {
    name: String,
    limit: usize,
    budget: Arc<ServerBudget>,
    /// Changes whenever the slot is replaced (shrink, rename, re-add).
    generation: u64,
    /// Download permits held by this generation's workers.
    held: Arc<AtomicUsize>,
    connected: Arc<AtomicUsize>,
    /// Sockets currently open (connected and authenticated), busy or idle.
    open: Arc<AtomicUsize>,
}

impl ConnectionTracker {
    /// A tracker with its own budgets, shared with nothing else.
    pub fn new() -> Self {
        Self::with_budgets(Arc::new(BudgetRegistry::default()))
    }

    /// A tracker drawing from `budgets`, shared with other consumers of the
    /// same servers.
    pub fn with_budgets(budgets: Arc<BudgetRegistry>) -> Self {
        Self {
            pools: Mutex::new(HashMap::new()),
            budgets,
            next_generation: AtomicU64::new(0),
        }
    }

    /// The budgets this tracker draws from.
    pub fn budgets(&self) -> &Arc<BudgetRegistry> {
        &self.budgets
    }

    /// Set or update the per-server connection limit.
    ///
    /// - First call for a `server_id`: sets the server's budget to `limit`.
    /// - Subsequent call with the same `limit` and `server_name`: no-op.
    /// - Grow (`limit > current`): raises the budget in place.
    /// - Shrink or rename: replaces the slot. Old permit holders detach
    ///   naturally via [`slot_is_current`].
    pub fn set_limit(&self, server_id: &str, server_name: &str, limit: usize) {
        let mut pools = self.pools.lock();
        match pools.get_mut(server_id) {
            Some(slot) if slot.limit == limit && slot.name == server_name => {
                // No change.
            }
            Some(slot) if limit > slot.limit && slot.name == server_name => {
                let added = limit - slot.limit;
                let old = slot.limit;
                self.budgets.ensure(server_id, limit);
                slot.limit = limit;
                info!(
                    server_id,
                    server = %server_name,
                    old_limit = old,
                    new_limit = limit,
                    added,
                    "Connection pool grew in place"
                );
            }
            existing => {
                // Capture diagnostics before re-borrowing `pools` for insert.
                let (prev_limit, prev_name) = match existing {
                    Some(s) => (Some(s.limit), Some(s.name.clone())),
                    None => (None, None),
                };
                pools.insert(
                    server_id.to_string(),
                    ServerSlot {
                        name: server_name.to_string(),
                        limit,
                        budget: self.budgets.ensure(server_id, limit),
                        generation: self.next_generation.fetch_add(1, Ordering::Relaxed),
                        held: Arc::new(AtomicUsize::new(0)),
                        connected: Arc::new(AtomicUsize::new(0)),
                        open: Arc::new(AtomicUsize::new(0)),
                    },
                );
                if let Some(prev) = prev_limit {
                    let renamed = prev_name.as_deref() != Some(server_name);
                    info!(
                        server_id,
                        server = %server_name,
                        old_limit = prev,
                        new_limit = limit,
                        renamed,
                        "Connection pool replaced (shrink or rename); old slots wind down"
                    );
                } else {
                    info!(
                        server_id,
                        server = %server_name,
                        limit,
                        "Connection pool created"
                    );
                }
            }
        }
    }

    /// Forget a server entirely (e.g. on `update_servers` removing it).
    /// Existing permit holders are unaffected; they will detect that their
    /// slot is no longer current and exit.
    pub fn remove_server(&self, server_id: &str) {
        self.pools.lock().remove(server_id);
    }

    /// Acquire a connection slot for `server_id`. Awaits if the pool is at
    /// the limit. Returns `None` if the server isn't registered or its
    /// limit is zero.
    pub async fn acquire(&self, server_id: &str) -> Option<ConnectionSlot> {
        // Snapshot the ServerSlot under lock, release the lock before await.
        let server_slot = {
            let pools = self.pools.lock();
            pools.get(server_id).cloned()?
        };
        if server_slot.limit == 0 {
            return None;
        }
        let permit = server_slot.budget.acquire(BudgetTier::Download).await;
        server_slot.held.fetch_add(1, Ordering::Relaxed);
        Some(ConnectionSlot {
            server_id: server_id.to_string(),
            server_name: server_slot.name,
            generation: server_slot.generation,
            held: server_slot.held,
            connected: server_slot.connected,
            is_connected: false,
            open: server_slot.open,
            is_open: false,
            budget: server_slot.budget,
            permit: Some(permit),
        })
    }

    /// Returns true if `slot` was acquired from the *current* slot for its
    /// server. False if the limit was shrunk (slot replaced) or
    /// the server was removed — the worker should exit at its next safe
    /// checkpoint.
    pub fn slot_is_current(&self, slot: &ConnectionSlot) -> bool {
        matches!(self.slot_status(slot), SlotStatus::Current)
    }

    /// Like [`slot_is_current`] but distinguishes the reason a slot is no
    /// longer current — useful for diagnostics on the worker exit path.
    pub fn slot_status(&self, slot: &ConnectionSlot) -> SlotStatus {
        let pools = self.pools.lock();
        match pools.get(&slot.server_id) {
            Some(server_slot) => {
                if server_slot.generation == slot.generation {
                    SlotStatus::Current
                } else {
                    SlotStatus::PoolReplaced
                }
            }
            None => SlotStatus::ServerRemoved,
        }
    }

    /// `(server_id, active, limit)` triples for the live pool. `active` is
    /// the download permits held by the current generation, which the
    /// budget keeps `<= limit`.
    pub fn snapshot(&self) -> Vec<(String, usize, usize)> {
        let pools = self.pools.lock();
        pools
            .iter()
            .map(|(id, slot)| {
                let active = slot.held.load(Ordering::Relaxed).min(slot.limit);
                (id.clone(), active, slot.limit)
            })
            .collect()
    }

    /// `(server_id, active, limit)` triples for sockets currently transferring
    /// an article. Authenticated sockets waiting for work are idle/free.
    pub fn connected_snapshot(&self) -> Vec<(String, usize, usize)> {
        let pools = self.pools.lock();
        pools
            .iter()
            .map(|(id, slot)| {
                (
                    id.clone(),
                    slot.connected.load(Ordering::Relaxed).min(slot.limit),
                    slot.limit,
                )
            })
            .collect()
    }

    /// `(server_id, open, limit)` triples for NNTP sockets actually open,
    /// busy or idle. A worker whose socket the provider closed, or which is
    /// waiting for work without a socket, is not counted.
    pub fn open_snapshot(&self) -> Vec<(String, usize, usize)> {
        let pools = self.pools.lock();
        pools
            .iter()
            .map(|(id, slot)| {
                (
                    id.clone(),
                    slot.open.load(Ordering::Relaxed).min(slot.limit),
                    slot.limit,
                )
            })
            .collect()
    }

    /// Total download permits held across all servers in the live pool.
    /// Permits held by replaced slots are NOT counted — they'll go away as
    /// the holding workers exit.
    pub fn total(&self) -> usize {
        let pools = self.pools.lock();
        pools.values().map(|s| s.held.load(Ordering::Relaxed)).sum()
    }
}

impl Default for ConnectionTracker {
    fn default() -> Self {
        Self::new()
    }
}

/// Why a `ConnectionSlot` may no longer match its server's live pool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotStatus {
    /// Slot is still attached to the current slot for its server.
    Current,
    /// The slot was replaced (set_limit shrunk or rebuilt the pool).
    /// Old permit holders should exit at their next safe checkpoint.
    PoolReplaced,
    /// The server has been removed entirely from the tracker.
    ServerRemoved,
}

/// RAII handle for one worker's NNTP connection slot. Workers hold a
/// `ConnectionSlot` for their entire lifetime, but its budget permit only
/// while they have a socket to fetch with: an idle worker closes its socket
/// and gives the permit back ([`ConnectionSlot::release_permit`]), then
/// takes one again before reconnecting ([`ConnectionSlot::reacquire_permit`]).
/// A permit still held is released synchronously on drop.
pub struct ConnectionSlot {
    server_id: String,
    server_name: String,
    /// The slot generation the permit was issued under. Used by
    /// `ConnectionTracker::slot_is_current` to detect a stale slot after a
    /// `set_limit` shrink (which replaces the slot).
    generation: u64,
    held: Arc<AtomicUsize>,
    connected: Arc<AtomicUsize>,
    is_connected: bool,
    open: Arc<AtomicUsize>,
    is_open: bool,
    budget: Arc<ServerBudget>,
    permit: Option<BudgetPermit>,
}

impl ConnectionSlot {
    pub fn server_id(&self) -> &str {
        &self.server_id
    }
    pub fn server_name(&self) -> &str {
        &self.server_name
    }

    /// The server budget this slot draws its permit from.
    pub fn budget(&self) -> &Arc<ServerBudget> {
        &self.budget
    }

    /// Whether the slot currently holds a budget permit.
    pub fn has_permit(&self) -> bool {
        self.permit.is_some()
    }

    /// Give the budget permit back. Call only once the socket is closed.
    pub fn release_permit(&mut self) {
        if self.permit.take().is_some() {
            self.held.fetch_sub(1, Ordering::Relaxed);
        }
    }

    /// Wait for a budget permit, if the slot does not hold one.
    pub async fn reacquire_permit(&mut self) {
        if self.permit.is_none() {
            self.permit = Some(self.budget.acquire(BudgetTier::Download).await);
            self.held.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn mark_active(&mut self) {
        if !self.is_connected {
            self.connected.fetch_add(1, Ordering::Relaxed);
            self.is_connected = true;
        }
    }

    fn mark_inactive(&mut self) {
        if self.is_connected {
            self.connected.fetch_sub(1, Ordering::Relaxed);
            self.is_connected = false;
        }
    }

    fn mark_open(&mut self) {
        if !self.is_open {
            self.open.fetch_add(1, Ordering::Relaxed);
            self.is_open = true;
        }
    }

    fn mark_closed(&mut self) {
        if self.is_open {
            self.open.fetch_sub(1, Ordering::Relaxed);
            self.is_open = false;
        }
    }

    fn activity(&mut self) -> ConnectionActivity<'_> {
        self.mark_active();
        ConnectionActivity { slot: self }
    }
}

impl Drop for ConnectionSlot {
    fn drop(&mut self) {
        self.mark_inactive();
        self.mark_closed();
        self.release_permit();
    }
}

/// Marks a connection as actively transferring an article for the lifetime
/// of the guard. An authenticated but idle socket does not consume a "used"
/// connection in API/UI accounting.
struct ConnectionActivity<'a> {
    slot: &'a mut ConnectionSlot,
}

impl Drop for ConnectionActivity<'_> {
    fn drop(&mut self) {
        self.slot.mark_inactive();
    }
}

// ---------------------------------------------------------------------------
// Server health tracking (circuit breaker)
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct ServerHealth {
    pub consecutive_failures: u32,
    pub disabled_until: Option<Instant>,
    pub reason: Option<String>,
    pub is_auth_failure: bool,
}

impl Default for ServerHealth {
    fn default() -> Self {
        Self::new()
    }
}

impl ServerHealth {
    pub fn new() -> Self {
        Self {
            consecutive_failures: 0,
            disabled_until: None,
            reason: None,
            is_auth_failure: false,
        }
    }

    pub fn is_available(&self) -> bool {
        match self.disabled_until {
            None => true,
            Some(until) => Instant::now() >= until,
        }
    }

    pub fn record_failure(&mut self, is_auth: bool, reason: &str) {
        self.consecutive_failures += 1;
        self.is_auth_failure = is_auth;
        self.reason = Some(reason.to_string());

        if is_auth || self.consecutive_failures >= CIRCUIT_BREAK_THRESHOLD {
            let cooldown = if is_auth {
                AUTH_FAILURE_COOLDOWN
            } else {
                TRANSIENT_FAILURE_COOLDOWN
            };
            self.disabled_until = Some(Instant::now() + cooldown);
        }
    }

    pub fn record_success(&mut self) {
        *self = Self::new();
    }
}

pub type ServerHealthMap = Arc<Mutex<HashMap<String, ServerHealth>>>;

// ---------------------------------------------------------------------------
// Progress update messages
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub enum ProgressUpdate {
    /// Every enabled NNTP provider is temporarily unavailable. The job stays
    /// active and will retry automatically when a provider recovers.
    WaitingForProviders { job_id: String, message: String },
    /// At least one NNTP provider recovered after a
    /// [`ProgressUpdate::WaitingForProviders`] notification.
    ProvidersAvailable { job_id: String },
    ArticleComplete {
        job_id: String,
        file_id: String,
        segment_number: u32,
        decoded_bytes: u64,
        file_complete: bool,
        server_id: Option<String>,
        /// Filename declared by the yEnc header. This lets the queue layer
        /// recognize obfuscated PAR2 files before terminal completion.
        yenc_filename: Option<String>,
    },
    /// An article could not be retrieved. `failure` carries the typed
    /// classification of *why* (NotFound, ServerDown, AuthFailed, …).
    /// See `crate::article_failure` for the taxonomy.
    ArticleFailed {
        job_id: String,
        file_id: String,
        segment_number: u32,
        failure: crate::article_failure::ArticleFailure,
    },
    /// What one server said about one article on one attempt, in the damage
    /// ledger's vocabulary. Emitted once per server attempt, before the
    /// article's terminal `ArticleComplete`/`ArticleFailed`, so the queue
    /// layer can record per-server evidence. Only a "not here" answer (430
    /// and 411/412/420/423) is `Refused`; timeouts, connection loss and
    /// decode errors are `Transport`.
    ArticleOutcome {
        job_id: String,
        file_id: String,
        segment_number: u32,
        message_id: String,
        outcome: nzb_core::Outcome,
    },
    JobFinished {
        job_id: String,
        success: bool,
        articles_failed: usize,
        download_time_secs: f64,
        /// In-flight PAR2 slice verdicts, when the job's PAR2 index arrived
        /// during the download (WI-1228).
        slice_map: Option<nzb_core::models::SliceMapSummary>,
        /// The in-flight repair fold, when damage was seen during the download
        /// (WI-148). Post-processing solves from it instead of re-reading the
        /// intact slices.
        repair_fold: Option<nzb_decode::SharedFold>,
    },
    JobAborted {
        job_id: String,
        reason: String,
        articles_failed: usize,
        download_time_secs: f64,
    },
}

// ---------------------------------------------------------------------------
// Work item
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub(crate) struct WorkItem {
    pub(crate) job_id: String,
    pub(crate) file_id: String,
    pub(crate) filename: String,
    pub(crate) message_id: String,
    pub(crate) segment_number: u32,
    /// Servers already tried for this article (by server ID).
    pub(crate) tried_servers: Vec<String>,
    /// Definitive per-provider outcomes. Transient failures are not recorded
    /// because they provide no evidence about article availability.
    pub(crate) provider_outcomes: HashMap<String, crate::article_failure::ArticleFailureKind>,
    /// Number of attempts on the current server.
    pub(crate) tries_on_current: u32,
    /// Timeouts and dropped connections seen for this article while the
    /// server was otherwise healthy. Reset when the article moves server.
    pub(crate) outage_tries: u32,
    /// Confirm rounds already spent on this article; see
    /// [`CONFIRM_ASKS_PER_SERVER`].
    pub(crate) confirm_rounds: u32,
}

// ---------------------------------------------------------------------------
// Per-job context
// ---------------------------------------------------------------------------

/// Per-job state that workers reference via `item.job_id`.
///
/// Everything a worker needs to process an article for a specific job lives
/// here. The queue manager owns one `Arc<JobContext>` per active job; the
/// pool holds a clone in its [`JobContextMap`] so workers can look it up.
pub(crate) struct JobContext {
    pub job_id: String,
    pub work_dir: PathBuf,
    pub assembler: Arc<FileAssembler>,
    pub progress_tx: mpsc::Sender<ProgressUpdate>,
    pub yenc_names: Arc<Mutex<HashMap<String, String>>>,
    pub nzb_filenames: HashMap<String, String>,
    /// Articles that still need a definitive result (success or all-server
    /// failure). When this reaches zero, `JobFinished` is emitted.
    pub articles_remaining: AtomicUsize,
    pub articles_failed: AtomicUsize,
    pub paused: AtomicBool,
    pub cancelled: AtomicBool,
    /// Optional abort reason — if set when articles_remaining hits zero
    /// (or when cancellation fires), `JobAborted` is emitted instead of
    /// `JobFinished`.
    pub abort_reason: Mutex<Option<String>>,
    pub total_decode_us: Arc<AtomicU64>,
    pub total_assemble_us: Arc<AtomicU64>,
    pub total_articles_decoded: Arc<AtomicU64>,
    pub engine_start: Instant,
    active_started: Mutex<Option<Instant>>,
    active_elapsed: Mutex<Duration>,
    /// Total bytes across all files (for perf summary throughput).
    pub total_bytes: u64,
    /// The `=ybegin size=` values seen per file, as `(first, min, max, logged)`.
    /// The range is logged once per file, the first time a later segment
    /// widens it. When the NZB declared a size, the record is only that log:
    /// obfuscated reposts put a different original file's `size=` on every
    /// segment, and rejecting those articles corrupts a file that PAR2 would
    /// verify clean. When the NZB declared none, `first` is the write bound;
    /// a later, smaller `size=` lowers `min` but never the bound.
    pinned_file_sizes: PinnedSizes,
    /// Ensures JobFinished/JobAborted is only emitted once.
    finished: AtomicBool,
    /// Avoid repeatedly reporting the same all-provider outage while the
    /// workers wait for circuit-breaker cooldown.
    waiting_for_providers_reported: AtomicBool,
    /// The job's download-phase span. Taken and closed when the job reaches
    /// its terminal state; carries the cumulative decode and assemble times.
    span: Mutex<Option<tracing::Span>>,
    /// In-flight PAR2 slice verification for this job.
    pub(crate) slices: Arc<crate::slice_tracker::JobSlices>,
    /// Recovery volumes by NZB file id → `(first exponent, blocks)`. Empty
    /// when the job has no PAR2 index or a volume's size is unknown, which
    /// turns deferral off for the job.
    par2_volumes: HashMap<String, (u32, u32)>,
    /// Recovery volumes held back until the slice map shows damage.
    held: crate::held_volumes::HeldVolumes,
}

pub(crate) type JobContextMap = Arc<Mutex<HashMap<String, Arc<JobContext>>>>;

impl JobContext {
    fn new(
        job: &NzbJob,
        assembler: Arc<FileAssembler>,
        progress_tx: mpsc::Sender<ProgressUpdate>,
        total_articles: usize,
    ) -> Self {
        let nzb_filenames = job
            .files
            .iter()
            .map(|f| (f.id.clone(), f.filename.clone()))
            .collect();
        Self {
            job_id: job.id.clone(),
            work_dir: job.work_dir.clone(),
            assembler,
            progress_tx,
            yenc_names: Arc::new(Mutex::new(HashMap::new())),
            nzb_filenames,
            articles_remaining: AtomicUsize::new(total_articles),
            articles_failed: AtomicUsize::new(0),
            paused: AtomicBool::new(false),
            cancelled: AtomicBool::new(false),
            abort_reason: Mutex::new(None),
            total_decode_us: Arc::new(AtomicU64::new(0)),
            total_assemble_us: Arc::new(AtomicU64::new(0)),
            total_articles_decoded: Arc::new(AtomicU64::new(0)),
            engine_start: Instant::now(),
            active_started: Mutex::new(Some(Instant::now())),
            active_elapsed: Mutex::new(Duration::ZERO),
            total_bytes: job.total_bytes,
            pinned_file_sizes: Arc::new(Mutex::new(HashMap::new())),
            finished: AtomicBool::new(false),
            waiting_for_providers_reported: AtomicBool::new(false),
            slices: Arc::new(crate::slice_tracker::JobSlices::new(job)),
            par2_volumes: deferrable_volumes(job),
            held: Default::default(),
            span: Mutex::new(Some(tracing::info_span!(
                parent: None,
                "download",
                job_id = %job.id,
                job_name = %job.name,
                articles_decoded = tracing::field::Empty,
                total_bytes = tracing::field::Empty,
                decode_secs = tracing::field::Empty,
                assemble_secs = tracing::field::Empty,
                active_secs = tracing::field::Empty,
            ))),
        }
    }

    fn report_waiting_for_providers(&self, message: &str) {
        if self
            .waiting_for_providers_reported
            .compare_exchange(false, true, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
        {
            try_send_progress(
                &self.progress_tx,
                &self.job_id,
                ProgressUpdate::WaitingForProviders {
                    job_id: self.job_id.clone(),
                    message: message.to_string(),
                },
            );
        }
    }

    fn report_providers_available(&self) {
        if self
            .waiting_for_providers_reported
            .swap(false, Ordering::Relaxed)
        {
            try_send_progress(
                &self.progress_tx,
                &self.job_id,
                ProgressUpdate::ProvidersAvailable {
                    job_id: self.job_id.clone(),
                },
            );
        }
    }

    fn pause_clock(&self) {
        let mut started = self.active_started.lock();
        if let Some(at) = started.take() {
            *self.active_elapsed.lock() += at.elapsed();
        }
    }

    fn resume_clock(&self) {
        let mut started = self.active_started.lock();
        if started.is_none() {
            *started = Some(Instant::now());
        }
    }

    fn active_elapsed(&self) -> Duration {
        let elapsed = *self.active_elapsed.lock();
        elapsed
            + self
                .active_started
                .lock()
                .map_or(Duration::ZERO, |at| at.elapsed())
    }

    /// Decrement articles_remaining. If it reaches zero, run deobfuscation
    /// and emit the job-finished/aborted terminal update. Idempotent.
    fn resolve_one(&self) {
        // Still owning one remaining article keeps the count above zero, so a
        // release here can never race the data-end decision below.
        self.release_held(false);
        let prev = self.articles_remaining.fetch_sub(1, Ordering::Relaxed);
        if prev != 1 {
            return;
        }
        if self.release_held(true) {
            return;
        }
        self.emit_terminal();
    }

    /// Release held recovery volumes the slice map says the job needs.
    /// In flight the damage is the tracker's running estimate; at data end
    /// (`at_data_end`, no article outstanding) it is the exact map, with
    /// undecided slices counted as damaged. Returns true when work was added
    /// back to the queue.
    fn release_held(&self, at_data_end: bool) -> bool {
        if self.held.is_empty()
            || self.cancelled.load(Ordering::Relaxed)
            || self.abort_reason.lock().is_some()
        {
            return false;
        }
        let released = if at_data_end {
            match self.slices.summary() {
                Some(map) => {
                    let damage = map.damaged + map.unknown;
                    if damage == 0 {
                        return false;
                    }
                    let need = damage + crate::held_volumes::REPAIR_MARGIN;
                    if damage > self.held.total_blocks() {
                        // Hopeless on the volumes this NZB carries; fetch
                        // them all and leave the verdict to repair (WI-145).
                        warn!(
                            job_id = %self.job_id,
                            damage,
                            recovery_blocks = self.held.total_blocks(),
                            "Damage exceeds every recovery volume on hand"
                        );
                    }
                    self.held.release_for(need)
                }
                // No slice map to judge by: fetch every volume.
                None => self.held.release_all(),
            }
        } else {
            let damage = self.slices.damage_hint();
            if damage == 0 {
                return false;
            }
            self.held
                .release_for(damage + crate::held_volumes::REPAIR_MARGIN)
        };
        let Some((queue, items)) = released else {
            return false;
        };
        info!(
            job_id = %self.job_id,
            articles = items.len(),
            at_data_end,
            "Releasing held PAR2 recovery volumes"
        );
        self.articles_remaining
            .fetch_add(items.len(), Ordering::Relaxed);
        queue.submit_items(items);
        true
    }

    /// Emit the terminal (JobFinished / JobAborted) message. Safe to call
    /// multiple times; only the first call does anything.
    fn emit_terminal(&self) {
        if self.finished.swap(true, Ordering::Relaxed) {
            return;
        }

        // Run deobfuscation before signalling completion so post-processing
        // sees the final filenames.
        self.deobfuscate_files();

        let download_elapsed = self.engine_start.elapsed();
        let active_elapsed = self.active_elapsed();
        let decode_total_us = self.total_decode_us.load(Ordering::Relaxed);
        let assemble_total_us = self.total_assemble_us.load(Ordering::Relaxed);
        let articles_decoded = self.total_articles_decoded.load(Ordering::Relaxed);
        let elapsed_us = download_elapsed.as_micros().max(1);
        let throughput_mbps = (self.total_bytes as f64 / download_elapsed.as_secs_f64().max(0.001))
            / (1024.0 * 1024.0);
        info!(
            job_id = %self.job_id,
            elapsed_secs = download_elapsed.as_secs_f64(),
            total_bytes = self.total_bytes,
            throughput_mbps = format!("{throughput_mbps:.2}"),
            "Download phase complete"
        );
        info!(
            job_id = %self.job_id,
            articles_decoded,
            decode_secs = format!("{:.3}", decode_total_us as f64 / 1_000_000.0),
            assemble_secs = format!("{:.3}", assemble_total_us as f64 / 1_000_000.0),
            decode_pct = format!("{:.1}", decode_total_us as f64 / elapsed_us as f64 * 100.0),
            assemble_pct = format!("{:.1}", assemble_total_us as f64 / elapsed_us as f64 * 100.0),
            "Decode timing summary (cumulative across all workers)"
        );
        crate::telemetry::record_stage("download", None, download_elapsed.as_secs_f64());
        crate::telemetry::record_stage("decode", None, decode_total_us as f64 / 1_000_000.0);
        crate::telemetry::record_stage("assemble", None, assemble_total_us as f64 / 1_000_000.0);
        if let Some(span) = self.span.lock().take() {
            span.record("articles_decoded", articles_decoded);
            span.record("total_bytes", self.total_bytes);
            span.record("decode_secs", decode_total_us as f64 / 1_000_000.0);
            span.record("assemble_secs", assemble_total_us as f64 / 1_000_000.0);
            span.record("active_secs", active_elapsed.as_secs_f64());
        }

        let abort_reason = self.abort_reason.lock().clone();
        if let Some(reason) = abort_reason {
            let failed = self.articles_failed.load(Ordering::Relaxed);
            try_send_progress(
                &self.progress_tx,
                &self.job_id,
                ProgressUpdate::JobAborted {
                    job_id: self.job_id.clone(),
                    reason,
                    articles_failed: failed,
                    download_time_secs: active_elapsed.as_secs_f64(),
                },
            );
            return;
        }

        let failed = self.articles_failed.load(Ordering::Relaxed);
        let slice_map = self.slices.summary();
        if let Some(map) = &slice_map {
            info!(
                job_id = %self.job_id,
                slices = map.states.len(),
                damaged = map.damaged,
                unknown = map.unknown,
                recovery_blocks = map.recovery_block_count,
                "In-flight PAR2 slice map"
            );
        }
        try_send_progress(
            &self.progress_tx,
            &self.job_id,
            ProgressUpdate::JobFinished {
                job_id: self.job_id.clone(),
                success: failed == 0,
                articles_failed: failed,
                download_time_secs: active_elapsed.as_secs_f64(),
                slice_map,
                repair_fold: self.slices.fold(),
            },
        );
    }

    /// Choose the best filename between NZB subject and yEnc header per file
    /// and rename on disk if needed. Called exactly once at job completion.
    fn deobfuscate_files(&self) {
        let renames = self.yenc_names.lock();
        for (file_id, yenc_name) in renames.iter() {
            let Some(nzb_name) = self.nzb_filenames.get(file_id) else {
                continue;
            };
            if nzb_name == yenc_name {
                continue;
            }
            let clean_yenc = std::path::Path::new(yenc_name.as_str())
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(yenc_name);
            if clean_yenc.is_empty() || nzb_name == clean_yenc {
                continue;
            }

            let nzb_has_ext = has_known_extension(nzb_name);
            let yenc_has_ext = has_known_extension(clean_yenc);

            let (old_name, new_name) = if yenc_has_ext && !nzb_has_ext {
                (nzb_name.as_str(), clean_yenc)
            } else if nzb_has_ext && !yenc_has_ext {
                continue;
            } else if yenc_has_ext && nzb_has_ext {
                (nzb_name.as_str(), clean_yenc)
            } else {
                continue;
            };

            let old_path = self.work_dir.join(old_name);
            let new_path = self.work_dir.join(new_name);
            if old_path.exists() && !new_path.exists() {
                if let Err(e) = std::fs::rename(&old_path, &new_path) {
                    warn!(
                        job_id = %self.job_id,
                        from = %old_name,
                        to = %new_name,
                        "Failed to deobfuscate file: {e}"
                    );
                } else {
                    info!(
                        job_id = %self.job_id,
                        from = %old_name,
                        to = %new_name,
                        "Deobfuscated file"
                    );
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Shared work queue
// ---------------------------------------------------------------------------

/// Multi-job queue with per-server round-robin fairness and PAR2-first
/// priority within each submission.
///
/// Items submitted via [`SharedWorkQueue::submit_items`] are inserted so that
/// PAR2 index and volume files land ahead of data files (matching the prior
/// per-job ordering), while data files land at the tail. Cross-job ordering
/// is FIFO by submission time until a server has served a job; it then prefers
/// another eligible job before returning to that job's backlog.
pub(crate) struct SharedWorkQueue {
    inner: Mutex<QueueState>,
    notify: Notify,
}

struct QueueState {
    items: VecDeque<WorkItem>,
    /// Last job selected by each server. This is intentionally per server:
    /// separate NNTP providers can make independent fair scheduling choices.
    last_served: HashMap<String, String>,
}

impl SharedWorkQueue {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(QueueState {
                items: VecDeque::new(),
                last_served: HashMap::new(),
            }),
            notify: Notify::new(),
        }
    }

    /// Insert a batch of work items with PAR2 items ahead of data items.
    /// Cross-batch order is preserved: PAR2 items from this batch go after
    /// any existing items, then data items.
    pub fn submit_items(&self, mut items: Vec<WorkItem>) {
        items.sort_by_key(|item| par2_sort_key(&item.filename));
        let had_items = !items.is_empty();
        {
            let mut q = self.inner.lock();
            q.items.reserve(items.len());
            for item in items {
                q.items.push_back(item);
            }
        }
        if had_items {
            self.notify.notify_waiters();
        }
    }

    /// Push a single item back onto the front (used when a worker is
    /// returning an item because its job is paused or its server was just
    /// tried for this item).
    fn push_front(&self, item: WorkItem) {
        self.inner.lock().items.push_front(item);
        self.notify.notify_waiters();
    }

    /// Push a single item to the back (used after handle_article_not_available
    /// when another server can still try it).
    fn push_back(&self, item: WorkItem) {
        self.inner.lock().items.push_back(item);
        self.notify.notify_waiters();
    }

    /// `(workable, total)` for `server_id`. `workable` is the number of
    /// items in the queue that are eligible for a worker on `server_id`;
    /// `total` is the queue length. Used by the supervisor's starvation
    /// diagnostic — if `workable == 0` while `total > 0`, the server has
    /// items it can't service yet (either already tried, or waiting on a
    /// higher-priority server).
    ///
    /// An item is "workable" for this server when:
    /// - `server_id` is NOT in `item.tried_servers`, AND
    /// - no healthy higher-priority server still needs to try it (i.e. every
    ///   server in `higher_priority_servers` is already in `item.tried_servers`).
    ///
    /// Pass an empty slice when the caller has no strictly-higher-priority
    /// peers (priority 0, single-server setups, or all peers circuit-broken).
    pub(crate) fn workable_count_for(
        &self,
        server_id: &str,
        higher_priority_servers: &[String],
    ) -> (usize, usize) {
        let q = self.inner.lock();
        let total = q.items.len();
        let workable = q
            .items
            .iter()
            .filter(|i| !i.tried_servers.iter().any(|s| s == server_id))
            .filter(|i| {
                higher_priority_servers
                    .iter()
                    .all(|hp| i.tried_servers.contains(hp))
            })
            .count();
        (workable, total)
    }

    /// Pop the next item that can be processed by a worker on `server_id`.
    ///
    /// Prefers an eligible item from a job other than the one this server last
    /// served, then falls back to any eligible item. It also enforces server
    /// priority: items where any healthy higher-priority server has not yet
    /// tried the article are skipped for the backup server.
    ///
    /// `higher_priority_servers` is a caller-prepared list of server IDs with
    /// strictly higher priority (lower priority number) than the caller, filtered
    /// to only enabled + healthy servers. See `run_worker_pipelined` and
    /// `run_worker_serial` for the canonical computation. Empty slice disables
    /// the priority gate (priority-0 servers, single-server setups, or all
    /// higher-priority peers circuit-broken → backup can take over).
    ///
    /// Returns `None` if the queue is empty or if every item is either already
    /// tried here or pending a higher-priority server.
    fn pop_workable(
        &self,
        server_id: &str,
        higher_priority_servers: &[String],
    ) -> Option<WorkItem> {
        let mut state = self.inner.lock();
        let last_served = state.last_served.get(server_id).cloned();
        let is_eligible = |item: &WorkItem| {
            !item.tried_servers.iter().any(|s| s == server_id)
                && higher_priority_servers
                    .iter()
                    .all(|server| item.tried_servers.contains(server))
        };

        let selected = last_served
            .as_deref()
            .and_then(|last_job| {
                state
                    .items
                    .iter()
                    .position(|item| is_eligible(item) && item.job_id != last_job)
            })
            .or_else(|| state.items.iter().position(is_eligible));

        if let Some(index) = selected {
            let item = state.items.remove(index)?;
            state
                .last_served
                .insert(server_id.to_string(), item.job_id.clone());
            Some(item)
        } else {
            None
        }
    }

    /// Remove all items belonging to `job_id`. Used on cancel_job / remove_job.
    fn drain_job(&self, job_id: &str) -> Vec<WorkItem> {
        let mut q = self.inner.lock();
        let mut kept = VecDeque::with_capacity(q.items.len());
        let mut drained = Vec::new();
        while let Some(item) = q.items.pop_front() {
            if item.job_id == job_id {
                drained.push(item);
            } else {
                kept.push_back(item);
            }
        }
        q.items = kept;
        drained
    }

    /// Remove and return every queued item matching `predicate`.
    /// Remove and return the items matching `predicate`, leaving the rest in
    /// place and in order.
    fn take_where(&self, mut predicate: impl FnMut(&WorkItem) -> bool) -> Vec<WorkItem> {
        let mut q = self.inner.lock();
        let mut taken = Vec::new();
        let mut index = 0;
        while index < q.items.len() {
            if predicate(&q.items[index]) {
                taken.push(q.items.remove(index).expect("index < len"));
            } else {
                index += 1;
            }
        }
        taken
    }

    pub fn len(&self) -> usize {
        self.inner.lock().items.len()
    }
}

impl Default for SharedWorkQueue {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Worker pool
// ---------------------------------------------------------------------------

struct ActiveWorker {
    shutdown: Arc<AtomicBool>,
    /// Worker heartbeat: monotonic millis since pool creation
    /// (`WorkerPool::created_at`). Updated by the worker on every
    /// "definitive progress" event (successful decode or definitive
    /// failure). The supervisor reads this to detect zombie workers.
    last_progress: Arc<AtomicU64>,
    /// `ServerConfig` this worker was spawned with. The worker owns its own
    /// copy and never re-reads it, so a later edit to priority or connection
    /// parameters (host, port, TLS, credentials) is invisible until the
    /// worker is recycled. `reconcile_servers` compares this snapshot.
    spawned_with: ServerConfig,
    handle: JoinHandle<()>,
}

/// Fields a running worker captured at spawn and will not notice until it is
/// recycled. `connections` and `enabled` are handled separately (shrink/grow
/// and retire), so they are not part of this comparison.
fn connection_identity_changed(spawned: &ServerConfig, current: &ServerConfig) -> bool {
    spawned.priority != current.priority
        || spawned.host != current.host
        || spawned.port != current.port
        || spawned.ssl != current.ssl
        || spawned.ssl_verify != current.ssl_verify
        || spawned.username != current.username
        || spawned.password != current.password
        || spawned.trusted_fingerprint != current.trusted_fingerprint
        || spawned.proxy_url != current.proxy_url
}

/// Long-lived worker pool that services all active download jobs.
pub struct WorkerPool {
    work_queue: Arc<SharedWorkQueue>,
    job_contexts: JobContextMap,
    pub(crate) servers: Arc<Mutex<Vec<ServerConfig>>>,
    server_health: ServerHealthMap,
    bandwidth: Arc<BandwidthLimiter>,
    conn_tracker: Arc<ConnectionTracker>,
    stall_timeout: Option<Duration>,
    /// Reference epoch for worker heartbeats. All `last_progress` values
    /// store millis elapsed since this instant.
    created_at: Instant,
    /// Idle threshold above which the supervisor evicts a worker.
    /// Mutable so the harness (and runtime config reload) can adjust it
    /// without recreating the pool.
    max_worker_idle: Mutex<Duration>,
    /// How often idle workers probe their socket for a provider-side close.
    idle_keepalive: Mutex<Duration>,
    /// How long an idle worker keeps its socket and budget permit.
    idle_release: Mutex<Duration>,
    /// Per-server "last starvation log" timestamp; rate-limits the
    /// "no workable items" diagnostic to once per minute per server.
    starvation_log: Mutex<HashMap<String, Instant>>,
    /// Lifetime count of worker evictions performed by the heartbeat
    /// watchdog. Useful for tests and observability — a non-zero value
    /// means at least one worker stalled long enough to be reclaimed.
    evictions: AtomicU64,
    /// `par2.defer_volumes`: hold recovery volumes until damage shows.
    defer_par2_volumes: AtomicBool,
    workers: Mutex<HashMap<String, Vec<ActiveWorker>>>,
    shutdown: Arc<AtomicBool>,
    supervisor_handle: Mutex<Option<JoinHandle<()>>>,
}

impl WorkerPool {
    pub fn new(
        servers: Arc<Mutex<Vec<ServerConfig>>>,
        bandwidth: Arc<BandwidthLimiter>,
        conn_tracker: Arc<ConnectionTracker>,
        stall_timeout_secs: u64,
    ) -> Arc<Self> {
        let stall_timeout = if stall_timeout_secs > 0 {
            Some(Duration::from_secs(stall_timeout_secs))
        } else {
            None
        };
        Arc::new(Self {
            work_queue: Arc::new(SharedWorkQueue::new()),
            job_contexts: Arc::new(Mutex::new(HashMap::new())),
            servers,
            server_health: Arc::new(Mutex::new(HashMap::new())),
            bandwidth,
            conn_tracker,
            stall_timeout,
            created_at: Instant::now(),
            max_worker_idle: Mutex::new(DEFAULT_MAX_WORKER_IDLE),
            idle_keepalive: Mutex::new(DEFAULT_IDLE_KEEPALIVE),
            idle_release: Mutex::new(DEFAULT_IDLE_RELEASE),
            starvation_log: Mutex::new(HashMap::new()),
            evictions: AtomicU64::new(0),
            defer_par2_volumes: AtomicBool::new(true),
            workers: Mutex::new(HashMap::new()),
            shutdown: Arc::new(AtomicBool::new(false)),
            supervisor_handle: Mutex::new(None),
        })
    }

    /// Set `par2.defer_volumes` for jobs submitted from now on.
    pub fn set_defer_par2_volumes(&self, on: bool) {
        self.defer_par2_volumes.store(on, Ordering::Relaxed);
    }

    /// Override the worker idle eviction threshold. Tests use this to make
    /// the supervisor's heartbeat check converge in seconds.
    pub fn set_max_worker_idle(&self, d: Duration) {
        *self.max_worker_idle.lock() = d;
    }

    /// Read current worker idle eviction threshold.
    pub fn max_worker_idle(&self) -> Duration {
        *self.max_worker_idle.lock()
    }

    /// Override how often idle workers probe their socket. Tests shrink this
    /// so a provider-side close is noticed in milliseconds.
    pub fn set_idle_keepalive(&self, d: Duration) {
        *self.idle_keepalive.lock() = d;
    }

    /// Read the current idle keepalive interval.
    pub fn idle_keepalive(&self) -> Duration {
        *self.idle_keepalive.lock()
    }

    /// Override how long an idle worker keeps its socket and budget permit
    /// before closing it.
    pub fn set_idle_release(&self, d: Duration) {
        *self.idle_release.lock() = d;
    }

    /// Read the current idle release delay.
    pub fn idle_release(&self) -> Duration {
        *self.idle_release.lock()
    }

    /// Millis elapsed since the pool was constructed. Used as the
    /// monotonic clock for `ActiveWorker::last_progress`.
    fn elapsed_ms(&self) -> u64 {
        self.created_at.elapsed().as_millis() as u64
    }

    /// Reference `Instant` used as the epoch for `last_progress` / heartbeat
    /// timestamps. Exposed so the NNTP layer can share the same clock when
    /// ticking its socket-liveness heartbeat — values then compare directly
    /// against `self.elapsed_ms()` in the supervisor's idle-worker check.
    fn created_at(&self) -> Instant {
        self.created_at
    }

    /// Collect server IDs with strictly higher priority (lower priority number)
    /// than `my_priority`, restricted to enabled + healthy (non-circuit-broken)
    /// servers. `my_server_id` is excluded (a server never blocks itself).
    ///
    /// Used by worker loops and the supervisor to drive the priority gate in
    /// [`SharedWorkQueue::pop_workable`] and [`SharedWorkQueue::workable_count_for`].
    /// See `sabnzbd/nzb/article.py::get_article` for the reference behaviour.
    fn higher_priority_servers(&self, my_priority: u8, my_server_id: &str) -> Vec<String> {
        let servers = self.servers.lock();
        let health = self.server_health.lock();
        servers
            .iter()
            .filter(|s| s.enabled && s.priority < my_priority && s.id != my_server_id)
            .filter(|s| health.get(&s.id).is_none_or(|h| h.is_available()))
            .map(|s| s.id.clone())
            .collect()
    }

    /// Return a queue-facing message when no enabled provider can currently
    /// accept work. A circuit-broken provider is deliberately retried later,
    /// so this is a waiting condition rather than an article failure.
    fn provider_unavailable_message(&self) -> Option<&'static str> {
        let servers = self.servers.lock();
        let enabled: Vec<_> = servers.iter().filter(|server| server.enabled).collect();
        if enabled.is_empty() {
            return Some("Waiting for providers: no enabled NNTP servers are configured.");
        }

        let health = self.server_health.lock();
        enabled
            .iter()
            .all(|server| health.get(&server.id).is_some_and(|entry| !entry.is_available()))
            .then_some(
                "Waiting for providers: every enabled server is temporarily unavailable. rustnzb will retry automatically.",
            )
    }

    fn report_provider_outage(&self) {
        let Some(message) = self.provider_unavailable_message() else {
            return;
        };
        let contexts: Vec<_> = self.job_contexts.lock().values().cloned().collect();
        for ctx in contexts {
            ctx.report_waiting_for_providers(message);
        }
    }

    fn report_providers_available(&self) {
        let contexts: Vec<_> = self.job_contexts.lock().values().cloned().collect();
        for ctx in contexts {
            ctx.report_providers_available();
        }
    }

    /// Lifetime count of worker evictions performed by the heartbeat
    /// watchdog. Increases by 1 each time the supervisor reclaims a stalled
    /// worker. Test harnesses use this as a positive signal that the Phase
    /// 5 idle watchdog actually fired.
    pub fn eviction_count(&self) -> u64 {
        self.evictions.load(Ordering::Relaxed)
    }

    /// Spawn workers for all currently enabled servers and start the
    /// supervisor task. Call once at queue-manager startup.
    pub fn start(self: &Arc<Self>) {
        self.reconcile_servers();

        let this = Arc::clone(self);
        let handle = tokio::spawn(async move {
            this.supervisor_loop().await;
        });
        *self.supervisor_handle.lock() = Some(handle);
    }

    /// Create or tear down workers to match the current server list.
    ///
    /// For each enabled server, ensures exactly `server.connections` workers
    /// exist. Extra workers (from a shrunk limit or disabled server) have
    /// their per-worker shutdown flag flipped so they exit gracefully after
    /// the current article.
    ///
    /// A worker keeps the `ServerConfig` it was spawned with. When that
    /// snapshot's priority or connection parameters (host, port, TLS,
    /// credentials, proxy, certificate pin) no longer match the live config,
    /// every worker for that server is recycled so the new values take
    /// effect. An identical config does not recycle.
    pub fn reconcile_servers(self: &Arc<Self>) {
        if self.shutdown.load(Ordering::Relaxed) {
            return;
        }

        let servers_snapshot: Vec<ServerConfig> = self.servers.lock().clone();
        let mut workers = self.workers.lock();

        // First pass: retire workers for servers that are gone, disabled, or
        // whose priority / connection parameters changed under them.
        let mut retire: Vec<String> = Vec::new();
        for (key, list) in workers.iter() {
            let current = servers_snapshot.iter().find(|s| s.enabled && &s.id == key);
            let stale = current.is_none_or(|server| {
                list.iter()
                    .any(|w| connection_identity_changed(&w.spawned_with, server))
            });
            if stale {
                retire.push(key.clone());
            }
        }
        for key in retire {
            if let Some(list) = workers.remove(&key) {
                for w in list {
                    w.shutdown.store(true, Ordering::Relaxed);
                    // Don't await — workers check shutdown on next loop
                    // iteration and exit within ~WORKER_IDLE_POLL.
                    drop(w.handle);
                }
            }
        }

        // Second pass: spawn or shrink to match target count per enabled server.
        for server in &servers_snapshot {
            if !server.enabled {
                continue;
            }
            let target = (server.connections as usize).min(500);
            let entry = workers.entry(server.id.clone()).or_default();

            // Shrink: signal extras to exit.
            while entry.len() > target {
                if let Some(w) = entry.pop() {
                    w.shutdown.store(true, Ordering::Relaxed);
                    drop(w.handle);
                }
            }

            // Grow: spawn new workers with stagger.
            let current = entry.len();
            for conn_idx in current..target {
                let worker_shutdown = Arc::new(AtomicBool::new(false));
                // Initialize heartbeat to *now* so the worker has a full
                // grace period before its first eviction check.
                let last_progress = Arc::new(AtomicU64::new(self.elapsed_ms()));
                let pool = Arc::clone(self);
                let server_clone = server.clone();
                let ws_clone = Arc::clone(&worker_shutdown);
                let lp_clone = Arc::clone(&last_progress);
                let handle = tokio::spawn(async move {
                    pool_worker(pool, server_clone, conn_idx, ws_clone, lp_clone).await;
                });
                entry.push(ActiveWorker {
                    shutdown: worker_shutdown,
                    last_progress,
                    spawned_with: server.clone(),
                    handle,
                });
            }
        }
    }

    /// Register a new job context and submit its unfinished articles to the
    /// shared queue. Called by QueueManager::launch_download.
    pub(crate) fn submit_job(self: &Arc<Self>, ctx: Arc<JobContext>, items: Vec<WorkItem>) {
        let job_id = ctx.job_id.clone();
        if items.is_empty() {
            // Nothing to do — emit completion immediately.
            ctx.emit_terminal();
            return;
        }
        self.job_contexts.lock().insert(job_id.clone(), ctx.clone());
        let items =
            if self.defer_par2_volumes.load(Ordering::Relaxed) && !ctx.par2_volumes.is_empty() {
                let total = items.len();
                let now = ctx
                    .held
                    .hold(items, &ctx.par2_volumes, self.work_queue.clone());
                ctx.articles_remaining
                    .fetch_sub(total - now.len(), Ordering::Relaxed);
                now
            } else {
                items
            };
        if items.is_empty() {
            // Only held volumes are left (a resumed job): decide on them now.
            if !ctx.release_held(true) {
                ctx.emit_terminal();
            }
            return;
        }
        self.work_queue.submit_items(items);
        self.report_provider_outage();
        debug!(job_id = %job_id, queue_len = self.work_queue.len(), "Job submitted to worker pool");
    }

    /// Unregister a normally completed job and close its assembler files.
    ///
    /// This must only be called after `JobFinished` is received, which means
    /// every article has reached a definitive result and no worker can write
    /// another segment for this job. Abort and cancellation paths unregister
    /// their contexts separately because they may still have in-flight work.
    pub(crate) fn release_completed_job(&self, job_id: &str) {
        let ctx = self.job_contexts.lock().remove(job_id);
        if let Some(ctx) = ctx {
            // Workers can briefly retain an Arc<JobContext> after resolving
            // the final article. Clear the assembler explicitly so those
            // transient references do not keep every output file open during
            // post-processing.
            ctx.assembler.clear_job(job_id);
        }
    }

    /// Pause a job: workers stop pulling its items, and any item currently
    /// being held while paused is returned to the queue.
    pub fn pause_job(&self, job_id: &str) {
        if let Some(ctx) = self.job_contexts.lock().get(job_id) {
            ctx.pause_clock();
            ctx.paused.store(true, Ordering::Relaxed);
        }
    }

    /// Resume a paused job.
    pub fn resume_job(&self, job_id: &str) {
        if let Some(ctx) = self.job_contexts.lock().get(job_id) {
            ctx.resume_clock();
            ctx.paused.store(false, Ordering::Relaxed);
            // Wake any workers that were idle waiting for work.
            self.work_queue.notify.notify_waiters();
        }
    }

    /// Abort a job with a reason. Drains queued items, sets the abort flag,
    /// and emits JobAborted via the job's progress channel.
    pub fn abort_job(&self, job_id: &str, reason: String) -> bool {
        let ctx = self.job_contexts.lock().get(job_id).cloned();
        let Some(ctx) = ctx else {
            return false;
        };
        {
            let mut abort_reason = ctx.abort_reason.lock();
            if abort_reason.is_some() {
                return false;
            }
            *abort_reason = Some(reason);
        }
        ctx.cancelled.store(true, Ordering::Relaxed);
        let drained = self.work_queue.drain_job(job_id);
        // Decrement the remaining counter for drained items so the terminal
        // callback fires if nothing is in-flight for this job.
        for _ in drained {
            ctx.resolve_one();
        }
        // In-flight items still own the remaining count. They will resolve
        // (without writing) as their responses arrive or after a connection
        // failure requeues them. Only the final resolution emits JobAborted.
        if ctx.articles_remaining.load(Ordering::Relaxed) == 0 {
            ctx.emit_terminal();
        }
        true
    }

    /// Cancel a job silently (no JobFinished / JobAborted emission).
    /// Used by `remove_job` when the user deletes a job from the queue —
    /// the progress receiver is about to be dropped anyway.
    pub fn cancel_job(&self, job_id: &str) {
        let ctx = self.job_contexts.lock().remove(job_id);
        let Some(ctx) = ctx else {
            return;
        };
        ctx.cancelled.store(true, Ordering::Relaxed);
        let _ = self.work_queue.drain_job(job_id);
    }

    /// Per-tick checks that maintain pool health: idle-worker eviction,
    /// dead-worker reaping, worker reconciliation, and starvation diagnostics.
    ///
    /// Circuit-broken providers retain their queued articles. Workers retry
    /// after the provider cooldown instead of converting a transient outage
    /// into a persistent per-job pause.
    async fn supervisor_loop(self: Arc<Self>) {
        let mut ticker = tokio::time::interval(SUPERVISOR_INTERVAL);
        loop {
            ticker.tick().await;
            if self.shutdown.load(Ordering::Relaxed) {
                break;
            }

            // ---------- 1. Heartbeat eviction ----------
            // Compute idle threshold + current epoch outside the workers lock.
            let now_ms = self.elapsed_ms();
            let max_idle_ms = self.max_worker_idle().as_millis() as u64;

            // Pre-compute per-server "has workable items now" so idle backup
            // workers aren't evicted when there's legitimately nothing for
            // them to do (waiting on higher-priority servers to fail first).
            // Computed once per tick, outside the workers lock, to avoid
            // lock-ordering hazards. Matches SABnzbd's idle-thread model
            // (sabnzbd/downloader.py — idle threads stay connected).
            let server_priorities: Vec<(String, u8)> = {
                let srv = self.servers.lock();
                srv.iter()
                    .filter(|s| s.enabled)
                    .map(|s| (s.id.clone(), s.priority))
                    .collect()
            };
            let has_workable: HashMap<String, bool> = server_priorities
                .iter()
                .map(|(sid, prio)| {
                    let hp = self.higher_priority_servers(*prio, sid);
                    let (workable, _) = self.work_queue.workable_count_for(sid, &hp);
                    (sid.clone(), workable > 0)
                })
                .collect();

            {
                let workers = self.workers.lock();
                for (server_id, list) in workers.iter() {
                    for (idx, w) in list.iter().enumerate() {
                        if w.shutdown.load(Ordering::Relaxed) {
                            continue;
                        }
                        let last = w.last_progress.load(Ordering::Relaxed);
                        let idle = now_ms.saturating_sub(last);
                        if idle > max_idle_ms {
                            // Bug 2 fix: don't evict a worker whose server
                            // has no workable items. It's idle because it's
                            // waiting for its primary/higher-priority peers
                            // to fail articles — not because it's zombied.
                            // If the server isn't in the map at all (e.g.
                            // disabled mid-tick), default to true (evict).
                            if !has_workable.get(server_id).copied().unwrap_or(true) {
                                continue;
                            }
                            warn!(
                                server = %server_id,
                                worker_idx = idx,
                                idle_ms = idle,
                                max_idle_ms,
                                "Idle-worker watchdog: evicting stalled worker"
                            );
                            w.shutdown.store(true, Ordering::Relaxed);
                            self.evictions.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
            }

            // ---------- 2. Reap finished workers and respawn ----------
            {
                let mut workers = self.workers.lock();
                for list in workers.values_mut() {
                    list.retain(|w| !w.handle.is_finished());
                }
            }
            // Reconcile fills any gaps left by reaped workers.
            self.reconcile_servers();

            // ---------- 3. Resolve articles no longer waiting on a required server ----------
            self.resolve_items_not_waiting_on_required_providers();

            // ---------- 4. Starvation diagnostic ----------
            // For each enabled server: if the queue has items but none are
            // workable for this server, log once per minute. "Not workable"
            // can mean either (a) every item has already been tried here, or
            // (b) every item is still waiting on a higher-priority server
            // (backup server legitimately idle — not a bug).
            let now_instant = Instant::now();
            for (sid, prio) in &server_priorities {
                let hp = self.higher_priority_servers(*prio, sid);
                let (workable, total) = self.work_queue.workable_count_for(sid, &hp);
                if workable == 0 && total > 0 {
                    let mut log = self.starvation_log.lock();
                    let should_log = log
                        .get(sid)
                        .map(|t| now_instant.duration_since(*t) >= Duration::from_secs(60))
                        .unwrap_or(true);
                    if should_log {
                        log.insert(sid.clone(), now_instant);
                        let reason = if hp.is_empty() {
                            "every item has been tried here already"
                        } else {
                            "every item has been tried here, or is waiting for a higher-priority server"
                        };
                        info!(
                            server = %sid,
                            total_items = total,
                            higher_priority_servers = hp.len(),
                            "Queue has items but none are workable for this server ({reason})"
                        );
                    }
                }
            }
        }
    }

    /// Fail queued articles that every required provider has already
    /// answered definitively.
    ///
    /// An item is normally resolved by the worker that records the last
    /// outcome. When an optional server becomes unavailable after the other
    /// servers have answered, the item is left in the queue waiting for that
    /// server, whose workers no longer pull work. Since the optional server is
    /// no longer required, resolve such items here.
    fn resolve_items_not_waiting_on_required_providers(&self) {
        // Nothing can become resolvable unless an optional server is down:
        // with every enabled server required, an item resolves only when a
        // worker records the last server's outcome. Skip the queue walk.
        {
            let servers = self.servers.lock();
            let health = self.server_health.lock();
            let optional_down = servers.iter().any(|server| {
                server.enabled
                    && server.optional
                    && health.get(&server.id).is_some_and(|h| !h.is_available())
            });
            if !optional_down {
                return;
            }
        }
        let taken = {
            let servers = self.servers.lock();
            let health = self.server_health.lock();
            self.work_queue.take_where(|item| {
                !item.provider_outcomes.is_empty()
                    && all_enabled_providers_definitive(&servers, &health, &item.provider_outcomes)
            })
        };
        for item in taken {
            let ctx = self.job_contexts.lock().get(&item.job_id).cloned();
            let Some(ctx) = ctx else {
                continue;
            };
            if ctx.cancelled.load(Ordering::Relaxed) {
                ctx.resolve_one();
                continue;
            }
            let last_server = item.tried_servers.last().cloned().unwrap_or_default();
            let last_kind = item
                .provider_outcomes
                .get(&last_server)
                .copied()
                .unwrap_or(crate::article_failure::ArticleFailureKind::NotFound);
            fail_article_everywhere(
                &item,
                &last_server,
                &ctx,
                "supervisor",
                last_kind,
                "Article unavailable on every required server (optional servers unavailable)",
            );
        }
    }

    /// Shut down all workers gracefully. In-flight articles finish first.
    pub async fn shutdown(self: &Arc<Self>) {
        self.shutdown.store(true, Ordering::Relaxed);
        let handles: Vec<JoinHandle<()>> = {
            let mut workers = self.workers.lock();
            let mut out = Vec::new();
            for (_id, list) in workers.drain() {
                for w in list {
                    w.shutdown.store(true, Ordering::Relaxed);
                    out.push(w.handle);
                }
            }
            out
        };
        // Notify workers so any parked on notify.notified() wake up.
        self.work_queue.notify.notify_waiters();

        await_worker_shutdown(handles, Duration::from_secs(10)).await;

        if let Some(h) = self.supervisor_handle.lock().take() {
            h.abort();
        }
    }

    pub fn conn_tracker(&self) -> &Arc<ConnectionTracker> {
        &self.conn_tracker
    }

    /// Whether this job still has an active context in the pool.
    pub fn has_job(&self, job_id: &str) -> bool {
        self.job_contexts.lock().contains_key(job_id)
    }

    /// Active (unpaused) download time of a registered job, in seconds.
    pub fn job_active_secs(&self, job_id: &str) -> Option<f64> {
        self.job_contexts
            .lock()
            .get(job_id)
            .map(|ctx| ctx.active_elapsed().as_secs_f64())
    }

    /// NZB file ids of `job_id`'s PAR2 recovery volumes still held back
    /// (`par2.defer_volumes`), or `None` when the job is not registered.
    pub fn held_par2_files(&self, job_id: &str) -> Option<Vec<String>> {
        self.job_contexts
            .lock()
            .get(job_id)
            .map(|ctx| ctx.held.held_file_ids())
    }
}

async fn await_worker_shutdown(mut handles: Vec<JoinHandle<()>>, timeout: Duration) {
    let deadline = tokio::time::Instant::now() + timeout;
    for handle in &mut handles {
        if tokio::time::timeout_at(deadline, &mut *handle)
            .await
            .is_err()
        {
            break;
        }
    }
    for handle in handles {
        if !handle.is_finished() {
            handle.abort();
        }
    }
}

// ---------------------------------------------------------------------------
// Worker body
// ---------------------------------------------------------------------------

/// Single pool worker. Owns an NNTP connection to `primary_server` and pulls
/// items from the shared queue until `worker_shutdown` is flipped (server
/// reconciled away, limit shrunk) or the pool shuts down.
///
/// The worker acquires its connection slot (a download permit from the
/// per-server budget) **before** the first connect attempt and holds the
/// slot for its entire lifetime. It holds the slot's budget permit whenever
/// it has a socket, across every reconnect, which bounds concurrent
/// connections to `server.connections` by construction. With nothing to
/// fetch it closes the socket and gives the permit back (see
/// [`next_work_item`]), then takes a permit again before reconnecting.
async fn pool_worker(
    pool: Arc<WorkerPool>,
    primary_server: ServerConfig,
    conn_idx: usize,
    worker_shutdown: Arc<AtomicBool>,
    last_progress: Arc<AtomicU64>,
) {
    let worker_id = format!("{}#{}", primary_server.id, conn_idx);

    // Stagger worker startup to avoid thundering herd of connections.
    if conn_idx > 0 {
        let stagger = WORKER_RAMP_DELAY * conn_idx as u32;
        tokio::time::sleep(stagger).await;
    }

    let should_exit = |worker_shutdown: &Arc<AtomicBool>, pool: &Arc<WorkerPool>| {
        worker_shutdown.load(Ordering::Relaxed) || pool.shutdown.load(Ordering::Relaxed)
    };

    // Acquire the slot up-front. If the server isn't registered or its limit
    // is zero, the worker has nothing to do — exit.
    let acquired = wait_unless_exiting(
        pool.conn_tracker.acquire(&primary_server.id),
        &pool,
        &worker_shutdown,
        &last_progress,
    )
    .await;
    let Some(acquired) = acquired else {
        return;
    };
    let mut conn_slot = match acquired {
        Some(slot) => slot,
        None => {
            info!(
                worker = %worker_id,
                server = %primary_server.name,
                "No connection slot available (server removed or limit=0); worker exiting"
            );
            return;
        }
    };

    'reconnect: loop {
        if should_exit(&worker_shutdown, &pool) {
            return;
        }

        // If the limit was shrunk under us, the slot will have been
        // replaced. Drop our (now-orphaned) slot and exit cleanly.
        match pool.conn_tracker.slot_status(&conn_slot) {
            SlotStatus::Current => {}
            SlotStatus::PoolReplaced => {
                info!(
                    worker = %worker_id,
                    server = %primary_server.name,
                    reason = "pool_replaced",
                    "Connection slot is stale (connection limit changed); worker exiting"
                );
                return;
            }
            SlotStatus::ServerRemoved => {
                info!(
                    worker = %worker_id,
                    server = %primary_server.name,
                    reason = "server_removed",
                    "Connection slot is stale (server removed from tracker); worker exiting"
                );
                return;
            }
        }

        // Check circuit breaker before connecting. Compute an owned bool
        // so we don't hold the MutexGuard across an await point.
        let circuit_broken = {
            let health = pool.server_health.lock();
            health
                .get(&primary_server.id)
                .is_some_and(|h| !h.is_available())
        };
        if circuit_broken {
            pool.report_provider_outage();
            tokio::time::sleep(WORKER_IDLE_POLL).await;
            continue 'reconnect;
        }

        // An idle worker gave its permit back; take one again before
        // opening a socket.
        if !conn_slot.has_permit()
            && wait_unless_exiting(
                conn_slot.reacquire_permit(),
                &pool,
                &worker_shutdown,
                &last_progress,
            )
            .await
            .is_none()
        {
            return;
        }

        info!(
            worker = %worker_id,
            server = %primary_server.name,
            host = %primary_server.host,
            port = primary_server.port,
            ssl = primary_server.ssl,
            conn_idx,
            "Worker starting — connecting to primary server"
        );

        let mut conn = NntpConnection::new(worker_id.clone());
        // Attach socket-liveness heartbeat BEFORE connect so every byte
        // received — from the welcome banner onward — counts as progress.
        // This is the fix for false-eviction of slow-but-working workers:
        // previously `last_progress` only advanced on full article decode,
        // so a worker receiving a 50-second article looked idle for the
        // entire fetch. Now any line read from the socket keeps it alive.
        // Matches SABnzbd's `nw.timeout` model (newswrapper.py:315).
        conn.set_io_heartbeat(last_progress.clone(), pool.created_at());
        let connect_started = Instant::now();
        let connect_span = tracing::info_span!(
            "nntp_connect",
            server = %primary_server.name,
            worker = %worker_id,
        );
        if let Err(e) = tracing::Instrument::instrument(
            connect_with_retry(
                &mut conn,
                &primary_server,
                &worker_id,
                &pool.server_health,
                &pool.servers,
            ),
            connect_span,
        )
        .await
        {
            warn!(
                worker = %worker_id,
                server = %primary_server.name,
                host = %primary_server.host,
                "Worker FAILED to connect after all retries: {e}"
            );
            if should_exit(&worker_shutdown, &pool) {
                return;
            }
            pool.report_provider_outage();
            tokio::time::sleep(RECONNECT_DELAY).await;
            continue 'reconnect;
        }
        crate::telemetry::record_stage(
            "connect",
            Some(&primary_server.name),
            connect_started.elapsed().as_secs_f64(),
        );
        pool.report_providers_available();
        conn_slot.mark_open();
        let pipe_depth = primary_server.pipelining.max(1);
        let active_conns = pool.conn_tracker.total();
        info!(
            worker = %worker_id,
            server = %primary_server.name,
            host = %primary_server.host,
            pipelining = pipe_depth,
            total_nntp_connections = active_conns,
            "Worker connected and ready"
        );

        // NOTE: do not tick heartbeat here. Reconnects are *not* progress
        // — counting them masks the zombie cycle (worker reconnects forever
        // without ever decoding an article). The heartbeat is initialised
        // to spawn time in `reconcile_servers`, which gives every worker a
        // full grace period to first-connect and process its first article.
        let reconnect_needed = if pipe_depth <= 1 {
            run_worker_serial(
                &pool,
                &primary_server,
                &worker_id,
                &worker_shutdown,
                &mut conn,
                &mut conn_slot,
                &last_progress,
            )
            .await
        } else {
            run_worker_pipelined(
                &pool,
                &primary_server,
                &worker_id,
                pipe_depth,
                &worker_shutdown,
                &mut conn,
                &mut conn_slot,
                &last_progress,
            )
            .await
        };

        if matches!(reconnect_needed, WorkerExit::PeerClosed) {
            conn.mark_peer_closed();
        }
        let _ = conn.quit().await;
        conn_slot.mark_closed();
        match reconnect_needed {
            WorkerExit::Reconnect => {
                // Loop back to the top and reconnect — slot is preserved.
                continue 'reconnect;
            }
            WorkerExit::PeerClosed | WorkerExit::Idle => {
                // The socket is closed (by the provider, or by us with
                // nothing to fetch). Stay disconnected, without a budget
                // permit, until there is work, so an empty queue holds no
                // sockets and none of the server's allowance.
                if matches!(reconnect_needed, WorkerExit::PeerClosed) {
                    info!(
                        worker = %worker_id,
                        server = %primary_server.name,
                        "Idle connection closed by the server; reconnecting when work arrives"
                    );
                } else {
                    debug!(
                        worker = %worker_id,
                        server = %primary_server.name,
                        "Idle worker closed its connection; reconnecting when work arrives"
                    );
                }
                conn_slot.release_permit();
                if !wait_for_workable(&pool, &primary_server, &worker_shutdown).await {
                    return;
                }
                // Waiting for work is not a stall: give the reconnect a full
                // grace period before the watchdog looks at this worker.
                last_progress.store(pool.elapsed_ms(), Ordering::Relaxed);
                continue 'reconnect;
            }
            WorkerExit::Exit => {
                // Slot drops here when conn_slot goes out of scope.
                return;
            }
        }
    }
}

enum WorkerExit {
    /// Exit the worker function entirely (server retired or pool shutdown).
    Exit,
    /// Reconnect and keep pulling work (transient connection loss).
    Reconnect,
    /// The idle socket was closed by the server. Reconnect only once there
    /// is work for this server.
    PeerClosed,
    /// Nothing to fetch: close the socket and give the budget permit back.
    /// Reconnect only once there is work for this server.
    Idle,
}

/// Outcome of [`next_work_item`].
enum NextWork {
    Item(WorkItem, Arc<JobContext>),
    /// The worker should exit (shutdown or server retirement).
    Exit,
    /// The idle keepalive found the socket closed.
    PeerClosed,
    /// Idle long enough, or another tier could take the budget permit:
    /// close the socket and give the permit back.
    Release,
}

/// Probe an idle connection with a cheap `STAT` of a message-id that cannot
/// exist. A 430 (or 223) reply means the session is alive; anything else,
/// including a timeout, means the socket is gone or unusable.
async fn idle_socket_alive(conn: &mut NntpConnection) -> bool {
    if !conn.is_connected() {
        return false;
    }
    if !conn.capabilities().have_stat {
        // No cheap command to probe with; assume alive and let the next
        // fetch discover a close, as before.
        return true;
    }
    matches!(
        tokio::time::timeout(
            IDLE_KEEPALIVE_TIMEOUT,
            conn.stat_article(IDLE_KEEPALIVE_MESSAGE_ID)
        )
        .await,
        Ok(Ok(_) | Err(NntpError::ArticleNotFound(_)))
    )
}

/// Drive `fut` (a wait for a budget permit) until it completes, or return
/// `None` once the worker should exit. Waiting for the budget is not a
/// stall, so the heartbeat is kept fresh meanwhile and the watchdog leaves
/// the worker alone.
async fn wait_unless_exiting<F: std::future::Future>(
    fut: F,
    pool: &Arc<WorkerPool>,
    worker_shutdown: &Arc<AtomicBool>,
    last_progress: &Arc<AtomicU64>,
) -> Option<F::Output> {
    tokio::pin!(fut);
    loop {
        tokio::select! {
            out = &mut fut => return Some(out),
            _ = tokio::time::sleep(WORKER_IDLE_POLL) => {
                if worker_shutdown.load(Ordering::Relaxed) || pool.shutdown.load(Ordering::Relaxed) {
                    return None;
                }
                last_progress.store(pool.elapsed_ms(), Ordering::Relaxed);
            }
        }
    }
}

/// Wait without a socket until `server` has workable items. Returns false if
/// the worker should exit instead.
async fn wait_for_workable(
    pool: &Arc<WorkerPool>,
    server: &ServerConfig,
    worker_shutdown: &Arc<AtomicBool>,
) -> bool {
    loop {
        if worker_shutdown.load(Ordering::Relaxed) || pool.shutdown.load(Ordering::Relaxed) {
            return false;
        }
        let hp = pool.higher_priority_servers(server.priority, &server.id);
        if pool.work_queue.workable_count_for(&server.id, &hp).0 > 0 {
            return true;
        }
        let notified = pool.work_queue.notify.notified();
        tokio::select! {
            _ = notified => {}
            _ = tokio::time::sleep(WORKER_IDLE_POLL) => {}
        }
    }
}

/// Wait for work, with early exit on shutdown / server retirement.
/// Returns `Some(item, ctx)` when a processable item is available, or `None`
/// if the worker should exit.
///
/// `higher_priority_servers` gates which items this server is allowed to take
/// — see `pop_workable` for the priority model.
///
/// While there is nothing to fetch the worker holds a socket and a share of
/// `budget`. It gives both up ([`NextWork::Release`]) when a waiter at
/// another tier could take its permit, or once it has been idle for the
/// pool's idle-release delay. Until then the keepalive probe notices a socket
/// the provider closed.
async fn next_work_item(
    pool: &Arc<WorkerPool>,
    server_id: &str,
    higher_priority_servers: &[String],
    worker_shutdown: &Arc<AtomicBool>,
    conn: &mut NntpConnection,
    budget: &Arc<ServerBudget>,
) -> NextWork {
    let mut last_probe = Instant::now();
    let idle_since = Instant::now();
    loop {
        if worker_shutdown.load(Ordering::Relaxed) || pool.shutdown.load(Ordering::Relaxed) {
            return NextWork::Exit;
        }

        if let Some(item) = pool
            .work_queue
            .pop_workable(server_id, higher_priority_servers)
        {
            // Look up the job context. If the job is gone or cancelled, drop
            // the item and keep going.
            let ctx = pool.job_contexts.lock().get(&item.job_id).cloned();
            let Some(ctx) = ctx else {
                continue;
            };
            if ctx.cancelled.load(Ordering::Relaxed) {
                ctx.resolve_one();
                continue;
            }
            // Respect per-job pause: return the item and wait.
            if ctx.paused.load(Ordering::Relaxed) {
                pool.work_queue.push_back(item);
                tokio::time::sleep(WORKER_IDLE_POLL).await;
                continue;
            }
            return NextWork::Item(item, ctx);
        }

        // Idle: give the socket and its permit up when they are wanted
        // elsewhere or have sat unused long enough.
        if budget.wanted_elsewhere(BudgetTier::Download)
            || idle_since.elapsed() >= pool.idle_release()
        {
            return NextWork::Release;
        }

        // Idle: check the provider has not closed the socket under us.
        if last_probe.elapsed() >= pool.idle_keepalive() {
            if !idle_socket_alive(conn).await {
                return NextWork::PeerClosed;
            }
            last_probe = Instant::now();
        }

        // Queue empty (or nothing workable for this server) — wait with a
        // timeout so we still notice shutdown and new work alike.
        let notified = pool.work_queue.notify.notified();
        tokio::select! {
            _ = notified => {}
            _ = budget.contended() => {}
            _ = tokio::time::sleep(WORKER_IDLE_POLL) => {}
        }
    }
}

async fn run_worker_serial(
    pool: &Arc<WorkerPool>,
    primary_server: &ServerConfig,
    worker_id: &str,
    worker_shutdown: &Arc<AtomicBool>,
    conn: &mut NntpConnection,
    conn_slot: &mut ConnectionSlot,
    last_progress: &Arc<AtomicU64>,
) -> WorkerExit {
    let mut consecutive_errors: u32 = 0;
    let budget = Arc::clone(conn_slot.budget());

    loop {
        // Server runtime checks.
        let server_disabled = pool
            .servers
            .lock()
            .iter()
            .find(|s| s.id == primary_server.id)
            .is_none_or(|s| !s.enabled);
        if server_disabled {
            info!(
                worker = %worker_id,
                server = %primary_server.name,
                "Server disabled, worker exiting"
            );
            return WorkerExit::Exit;
        }
        {
            let health = pool.server_health.lock();
            if let Some(h) = health.get(&primary_server.id)
                && !h.is_available()
            {
                info!(
                    worker = %worker_id,
                    server = %primary_server.name,
                    reason = h.reason.as_deref().unwrap_or("unknown"),
                    "Server circuit-broken, worker reconnecting after cooldown"
                );
                return WorkerExit::Reconnect;
            }
        }

        // Snapshot of healthy servers with strictly higher priority (lower
        // priority number). Used as the priority gate in pop_workable:
        // items waiting for a higher-priority server won't be dispatched
        // to this worker. Recomputed each loop iteration so runtime
        // priority / health changes are picked up. See SABnzbd
        // `Article.get_article` for the reference behaviour.
        let higher_priority_servers =
            pool.higher_priority_servers(primary_server.priority, &primary_server.id);

        let (mut item, ctx) = match next_work_item(
            pool,
            &primary_server.id,
            &higher_priority_servers,
            worker_shutdown,
            conn,
            &budget,
        )
        .await
        {
            NextWork::Item(item, ctx) => (item, ctx),
            NextWork::Exit => return WorkerExit::Exit,
            NextWork::PeerClosed => return WorkerExit::PeerClosed,
            NextWork::Release => return WorkerExit::Idle,
        };

        let result = {
            let _activity = conn_slot.activity();
            let fetch_fut = fetch_article_with_retry(
                conn,
                &item,
                &ctx.assembler,
                &ctx.cancelled,
                Some(&ctx.pinned_file_sizes),
                primary_server,
                worker_id,
            );
            if let Some(timeout) = pool.stall_timeout {
                match tokio::time::timeout(timeout, fetch_fut).await {
                    Ok(r) => r,
                    Err(_) => {
                        warn!(
                            worker = %worker_id,
                            server = %primary_server.name,
                            article = %item.message_id,
                            "Connection stalled — no response within {}s, reconnecting",
                            timeout.as_secs()
                        );
                        send_outcome(&ctx, &item, transport(primary_server));
                        if retry_or_fail_over(
                            item,
                            primary_server,
                            pool,
                            &ctx,
                            worker_id,
                            crate::article_failure::ArticleFailureKind::Timeout,
                            "connection stalled",
                        ) {
                            last_progress.store(pool.elapsed_ms(), Ordering::Relaxed);
                        }
                        return WorkerExit::Reconnect;
                    }
                }
            } else {
                fetch_fut.await
            }
        };

        match result {
            Ok(process_result) => {
                consecutive_errors = 0;
                ctx.total_decode_us
                    .fetch_add(process_result.decode_us, Ordering::Relaxed);
                ctx.total_assemble_us
                    .fetch_add(process_result.assemble_us, Ordering::Relaxed);
                ctx.total_articles_decoded.fetch_add(1, Ordering::Relaxed);
                if let Some(ref yname) = process_result.yenc_filename {
                    ctx.yenc_names
                        .lock()
                        .entry(item.file_id.clone())
                        .or_insert_with(|| crate::util::normalize_nfc(yname));
                }
                if let Some(n) = std::num::NonZeroU32::new(process_result.decoded_bytes as u32) {
                    let _ = pool.bandwidth.acquire_download(n).await;
                }
                ctx.slices
                    .record_write(
                        &item.file_id,
                        process_result.write_offset,
                        process_result.decoded_bytes,
                        process_result.file_complete,
                    )
                    .await;
                send_outcome(&ctx, &item, nzb_core::Outcome::Present);
                try_send_progress(
                    &ctx.progress_tx,
                    &item.job_id,
                    ProgressUpdate::ArticleComplete {
                        job_id: item.job_id.clone(),
                        file_id: item.file_id.clone(),
                        segment_number: item.segment_number,
                        decoded_bytes: process_result.decoded_bytes,
                        file_complete: process_result.file_complete,
                        server_id: Some(primary_server.id.clone()),
                        yenc_filename: process_result.yenc_filename.clone(),
                    },
                );
                ctx.resolve_one();
                // Heartbeat: definitive forward progress.
                last_progress.store(pool.elapsed_ms(), Ordering::Relaxed);
            }
            Err(ArticleError::ArticleNotFound) => {
                send_outcome(&ctx, &item, refused(primary_server));
                if handle_article_not_available(
                    &mut item,
                    primary_server,
                    &pool.servers,
                    &pool.server_health,
                    &ctx,
                    &pool.work_queue,
                    worker_id,
                    crate::article_failure::ArticleFailureKind::NotFound,
                    "Article not found on any server",
                ) {
                    last_progress.store(pool.elapsed_ms(), Ordering::Relaxed);
                }
            }
            Err(ArticleError::ConnectionLost(msg)) => {
                send_outcome(&ctx, &item, transport(primary_server));
                increment_counter("nntp.transient_reconnects");
                consecutive_errors += 1;
                warn!(
                    worker = %worker_id,
                    server = %primary_server.name,
                    host = %primary_server.host,
                    consecutive_errors,
                    max_reconnects = MAX_RECONNECT_ATTEMPTS,
                    article = %item.message_id,
                    "Connection lost: {msg}"
                );
                pool.work_queue.push_front(item);
                if consecutive_errors > MAX_RECONNECT_ATTEMPTS {
                    warn!(
                        worker = %worker_id,
                        server = %primary_server.name,
                        host = %primary_server.host,
                        consecutive_errors,
                        "Too many consecutive errors — worker reconnecting"
                    );
                    return WorkerExit::Reconnect;
                }
                return WorkerExit::Reconnect;
            }
            Err(ArticleError::ProviderUnavailable { kind, message }) => {
                send_outcome(&ctx, &item, transport(primary_server));
                increment_counter("nntp.transient_reconnects");
                let is_auth = matches!(
                    kind,
                    crate::article_failure::ArticleFailureKind::AuthFailed
                        | crate::article_failure::ArticleFailureKind::PermissionDenied
                );
                warn!(
                    job_id = %item.job_id,
                    file_id = %item.file_id,
                    segment_number = item.segment_number,
                    message_id = %item.message_id,
                    server_id = %primary_server.id,
                    worker_id = %worker_id,
                    original_failure_kind = kind.as_str(),
                    "Provider unavailable during article fetch: {message}"
                );
                pool.server_health
                    .lock()
                    .entry(primary_server.id.clone())
                    .or_default()
                    .record_failure(is_auth, &message);
                pool.report_provider_outage();
                pool.work_queue.push_front(item);
                return WorkerExit::Reconnect;
            }
            Err(ArticleError::ServerError { kind, message }) => {
                send_outcome(&ctx, &item, transport(primary_server));
                if retry_or_fail_over(item, primary_server, pool, &ctx, worker_id, kind, &message) {
                    last_progress.store(pool.elapsed_ms(), Ordering::Relaxed);
                }
                if conn.state != ConnectionState::Ready {
                    return WorkerExit::Reconnect;
                }
            }
            Err(ArticleError::DecodeError(msg)) => {
                // A body that fails to decode is not evidence the article is
                // missing (WI-145): it is inconclusive, never a refusal.
                send_outcome(&ctx, &item, transport(primary_server));
                if handle_article_not_available(
                    &mut item,
                    primary_server,
                    &pool.servers,
                    &pool.server_health,
                    &ctx,
                    &pool.work_queue,
                    worker_id,
                    crate::article_failure::ArticleFailureKind::DecodeError,
                    &format!("Decode error: {msg}"),
                ) {
                    last_progress.store(pool.elapsed_ms(), Ordering::Relaxed);
                }
            }
            Err(ArticleError::Cancelled) => {
                // Mirror the pipelined path: an aborted job's in-flight
                // article resolves without being written or reported.
                ctx.resolve_one();
            }
            Err(ArticleError::AssemblyError(msg)) => {
                // The server served a decodable body; only the local write failed.
                send_outcome(&ctx, &item, nzb_core::Outcome::Present);
                ctx.slices.record_failure(&item.file_id, false);
                error!(job_id = %item.job_id, file_id = %item.file_id, segment_number = item.segment_number, message_id = %item.message_id, server_id = %primary_server.id, worker_id = %worker_id, original_failure_kind = "decode_error", terminal_failure_kind = "decode_error", "Assembly error: {msg}");
                try_send_progress(
                    &ctx.progress_tx,
                    &item.job_id,
                    ProgressUpdate::ArticleFailed {
                        job_id: item.job_id.clone(),
                        file_id: item.file_id.clone(),
                        segment_number: item.segment_number,
                        failure: crate::article_failure::ArticleFailure::decode_error(
                            &primary_server.id,
                            format!("Assembly error: {msg}"),
                        ),
                    },
                );
                ctx.articles_failed.fetch_add(1, Ordering::Relaxed);
                ctx.resolve_one();
                last_progress.store(pool.elapsed_ms(), Ordering::Relaxed);
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_worker_pipelined(
    pool: &Arc<WorkerPool>,
    primary_server: &ServerConfig,
    worker_id: &str,
    pipe_depth: u8,
    worker_shutdown: &Arc<AtomicBool>,
    conn: &mut NntpConnection,
    conn_slot: &mut ConnectionSlot,
    last_progress: &Arc<AtomicU64>,
) -> WorkerExit {
    let mut pipeline = Pipeline::new(pipe_depth);
    let budget = Arc::clone(conn_slot.budget());
    let mut in_flight_items: HashMap<u64, WorkItem> = HashMap::new();
    let mut next_tag: u64 = 0;
    let mut consecutive_errors: u32 = 0;

    // Perf metrics
    let mut perf_articles: u64 = 0;
    let mut perf_bytes: u64 = 0;
    let mut perf_queue_lock_us: u64 = 0;
    let mut perf_receive_us: u64 = 0;
    let mut perf_decode_us: u64 = 0;
    let mut perf_assemble_us: u64 = 0;
    let mut perf_bandwidth_us: u64 = 0;
    let mut perf_yield_us: u64 = 0;
    let mut perf_flush_us: u64 = 0;
    let mut perf_last_log = Instant::now();
    const PERF_LOG_INTERVAL: Duration = Duration::from_secs(10);

    loop {
        if worker_shutdown.load(Ordering::Relaxed) || pool.shutdown.load(Ordering::Relaxed) {
            requeue_all(&mut in_flight_items, &pool.work_queue);
            return WorkerExit::Exit;
        }

        // Server runtime checks.
        let server_disabled = pool
            .servers
            .lock()
            .iter()
            .find(|s| s.id == primary_server.id)
            .is_none_or(|s| !s.enabled);
        if server_disabled {
            info!(
                worker = %worker_id,
                server = %primary_server.name,
                "Server disabled, worker exiting"
            );
            requeue_all(&mut in_flight_items, &pool.work_queue);
            return WorkerExit::Exit;
        }
        {
            let health = pool.server_health.lock();
            if let Some(h) = health.get(&primary_server.id)
                && !h.is_available()
            {
                info!(
                    worker = %worker_id,
                    server = %primary_server.name,
                    reason = h.reason.as_deref().unwrap_or("unknown"),
                    "Server circuit-broken, worker reconnecting after cooldown"
                );
                requeue_all(&mut in_flight_items, &pool.work_queue);
                return WorkerExit::Reconnect;
            }
        }

        // Snapshot of healthy servers with strictly higher priority. Gates
        // which items the pipeline-fill and wait-for-work paths are allowed
        // to take (see `pop_workable`). Recomputed each loop iteration so
        // runtime priority / health changes are picked up.
        let higher_priority_servers =
            pool.higher_priority_servers(primary_server.priority, &primary_server.id);

        // Fill the pipeline.
        while pipeline.pending_count() + pipeline.in_flight_count() < pipe_depth as usize {
            let lock_t = Instant::now();
            let item = pool
                .work_queue
                .pop_workable(&primary_server.id, &higher_priority_servers);
            perf_queue_lock_us += lock_t.elapsed().as_micros() as u64;
            let Some(item) = item else {
                break;
            };
            // Look up ctx to respect pause/cancel.
            let ctx = pool.job_contexts.lock().get(&item.job_id).cloned();
            let Some(ctx) = ctx else {
                continue;
            };
            if ctx.cancelled.load(Ordering::Relaxed) {
                ctx.resolve_one();
                continue;
            }
            if ctx.paused.load(Ordering::Relaxed) {
                pool.work_queue.push_back(item);
                break;
            }
            let tag = next_tag;
            next_tag += 1;
            pipeline.submit(item.message_id.clone(), tag);
            in_flight_items.insert(tag, item);
        }

        // If nothing is queued and nothing in flight, wait for work.
        if pipeline.is_empty() && in_flight_items.is_empty() {
            let (first_item, ctx) = match next_work_item(
                pool,
                &primary_server.id,
                &higher_priority_servers,
                worker_shutdown,
                conn,
                &budget,
            )
            .await
            {
                NextWork::Item(item, ctx) => (item, ctx),
                NextWork::Exit => return WorkerExit::Exit,
                NextWork::PeerClosed => return WorkerExit::PeerClosed,
                NextWork::Release => return WorkerExit::Idle,
            };
            let _ = ctx; // ctx is validated in next_work_item
            let tag = next_tag;
            next_tag += 1;
            pipeline.submit(first_item.message_id.clone(), tag);
            in_flight_items.insert(tag, first_item);
        }

        let _activity = conn_slot.activity();
        let flush_t = Instant::now();
        if let Err(e) = pipeline.flush_sends(conn).await {
            warn!(
                worker = %worker_id,
                server = %primary_server.name,
                host = %primary_server.host,
                error = %e,
                in_flight = in_flight_items.len(),
                "Pipeline send error — re-queuing all in-flight items"
            );
            requeue_all(&mut in_flight_items, &pool.work_queue);
            consecutive_errors += 1;
            if consecutive_errors > MAX_RECONNECT_ATTEMPTS {
                warn!(
                    worker = %worker_id,
                    server = %primary_server.name,
                    consecutive_errors,
                    "Too many pipeline errors — worker reconnecting"
                );
                return WorkerExit::Reconnect;
            }
            tokio::time::sleep(RECONNECT_DELAY).await;
            return WorkerExit::Reconnect;
        }
        perf_flush_us += flush_t.elapsed().as_micros() as u64;

        // Read one response.
        let recv_t = Instant::now();
        trace!(
            worker = %worker_id,
            in_flight = in_flight_items.len(),
            stall_timeout_secs = pool.stall_timeout.map(|d| d.as_secs()).unwrap_or(0),
            "Pipeline: awaiting response"
        );
        let result = if let Some(timeout) = pool.stall_timeout {
            match tokio::time::timeout(timeout, pipeline.receive_one(conn)).await {
                Ok(r) => r,
                Err(_) => {
                    let elapsed_ms = recv_t.elapsed().as_millis();
                    warn!(
                        worker = %worker_id,
                        server = %primary_server.name,
                        elapsed_ms,
                        in_flight = in_flight_items.len(),
                        "Connection stalled — no response within {}s, reconnecting",
                        timeout.as_secs()
                    );
                    // Every request still in flight went unanswered.
                    for item in in_flight_items.values() {
                        let ctx = pool.job_contexts.lock().get(&item.job_id).cloned();
                        if let Some(ctx) = ctx {
                            send_outcome(&ctx, item, transport(primary_server));
                        }
                    }
                    requeue_all(&mut in_flight_items, &pool.work_queue);
                    return WorkerExit::Reconnect;
                }
            }
        } else {
            pipeline.receive_one(conn).await
        };
        perf_receive_us += recv_t.elapsed().as_micros() as u64;

        match result {
            Ok(Some(pipe_result)) => {
                let Some(mut item) = in_flight_items.remove(&pipe_result.request.tag) else {
                    continue;
                };
                // Look up the ctx for this item's job (may have been cancelled).
                let ctx = pool.job_contexts.lock().get(&item.job_id).cloned();
                let Some(ctx) = ctx else {
                    continue;
                };
                if ctx.cancelled.load(Ordering::Relaxed) {
                    ctx.resolve_one();
                    continue;
                }

                match pipe_result.result {
                    Ok(response) => {
                        consecutive_errors = 0;
                        let raw_data = response.data.unwrap_or_default();
                        let yield_t = Instant::now();
                        tokio::task::yield_now().await;
                        perf_yield_us += yield_t.elapsed().as_micros() as u64;
                        let (decode_result, raw_data) = decode_and_assemble_offloaded(
                            &item,
                            raw_data,
                            &ctx.assembler,
                            Some(&ctx.pinned_file_sizes),
                        )
                        .await;
                        // Return the buffer to the connection's pool so the
                        // next article's fetch reuses it instead of
                        // allocating fresh.
                        conn.release_body_buffer(raw_data);
                        match decode_result {
                            Ok(process_result) => {
                                perf_decode_us += process_result.decode_us;
                                perf_assemble_us += process_result.assemble_us;
                                perf_bytes += process_result.decoded_bytes;
                                perf_articles += 1;
                                ctx.total_decode_us
                                    .fetch_add(process_result.decode_us, Ordering::Relaxed);
                                ctx.total_assemble_us
                                    .fetch_add(process_result.assemble_us, Ordering::Relaxed);
                                ctx.total_articles_decoded.fetch_add(1, Ordering::Relaxed);
                                if let Some(ref yname) = process_result.yenc_filename {
                                    ctx.yenc_names
                                        .lock()
                                        .entry(item.file_id.clone())
                                        .or_insert_with(|| crate::util::normalize_nfc(yname));
                                }
                                let bw_t = Instant::now();
                                if let Some(n) =
                                    std::num::NonZeroU32::new(process_result.decoded_bytes as u32)
                                {
                                    let _ = pool.bandwidth.acquire_download(n).await;
                                }
                                perf_bandwidth_us += bw_t.elapsed().as_micros() as u64;
                                ctx.slices
                                    .record_write(
                                        &item.file_id,
                                        process_result.write_offset,
                                        process_result.decoded_bytes,
                                        process_result.file_complete,
                                    )
                                    .await;
                                send_outcome(&ctx, &item, nzb_core::Outcome::Present);
                                try_send_progress(
                                    &ctx.progress_tx,
                                    &item.job_id,
                                    ProgressUpdate::ArticleComplete {
                                        job_id: item.job_id.clone(),
                                        file_id: item.file_id.clone(),
                                        segment_number: item.segment_number,
                                        decoded_bytes: process_result.decoded_bytes,
                                        file_complete: process_result.file_complete,
                                        server_id: Some(primary_server.id.clone()),
                                        yenc_filename: process_result.yenc_filename.clone(),
                                    },
                                );
                                ctx.resolve_one();
                                // Heartbeat: definitive forward progress.
                                last_progress.store(pool.elapsed_ms(), Ordering::Relaxed);

                                if perf_last_log.elapsed() >= PERF_LOG_INTERVAL {
                                    let elapsed = perf_last_log.elapsed().as_secs_f64();
                                    let mbps = perf_bytes as f64 / elapsed / (1024.0 * 1024.0);
                                    info!(
                                        worker = %worker_id,
                                        articles = perf_articles,
                                        throughput_mbps = format!("{mbps:.1}"),
                                        recv_ms = perf_receive_us / 1000,
                                        decode_ms = perf_decode_us / 1000,
                                        assemble_ms = perf_assemble_us / 1000,
                                        queue_lock_ms = perf_queue_lock_us / 1000,
                                        flush_ms = perf_flush_us / 1000,
                                        yield_ms = perf_yield_us / 1000,
                                        bw_wait_ms = perf_bandwidth_us / 1000,
                                        "Worker perf summary"
                                    );
                                    crate::telemetry::record_server_window(
                                        &primary_server.name,
                                        perf_bytes,
                                        perf_articles,
                                        perf_receive_us as f64 / 1_000_000.0,
                                    );
                                    perf_articles = 0;
                                    perf_bytes = 0;
                                    perf_queue_lock_us = 0;
                                    perf_receive_us = 0;
                                    perf_decode_us = 0;
                                    perf_assemble_us = 0;
                                    perf_bandwidth_us = 0;
                                    perf_yield_us = 0;
                                    perf_flush_us = 0;
                                    perf_last_log = Instant::now();
                                }
                            }
                            Err(ArticleError::DecodeError(msg)) => {
                                // Inconclusive, never a refusal (WI-145).
                                send_outcome(&ctx, &item, transport(primary_server));
                                if handle_article_not_available(
                                    &mut item,
                                    primary_server,
                                    &pool.servers,
                                    &pool.server_health,
                                    &ctx,
                                    &pool.work_queue,
                                    worker_id,
                                    crate::article_failure::ArticleFailureKind::DecodeError,
                                    &format!("Decode error: {msg}"),
                                ) {
                                    last_progress.store(pool.elapsed_ms(), Ordering::Relaxed);
                                }
                            }
                            Err(ArticleError::AssemblyError(msg)) => {
                                send_outcome(&ctx, &item, nzb_core::Outcome::Present);
                                ctx.slices.record_failure(&item.file_id, false);
                                error!(job_id = %item.job_id, file_id = %item.file_id, segment_number = item.segment_number, message_id = %item.message_id, server_id = %primary_server.id, worker_id = %worker_id, original_failure_kind = "decode_error", terminal_failure_kind = "decode_error", "Assembly error: {msg}");
                                try_send_progress(
                                    &ctx.progress_tx,
                                    &item.job_id,
                                    ProgressUpdate::ArticleFailed {
                                        job_id: item.job_id.clone(),
                                        file_id: item.file_id.clone(),
                                        segment_number: item.segment_number,
                                        failure:
                                            crate::article_failure::ArticleFailure::decode_error(
                                                &primary_server.id,
                                                format!("Assembly error: {msg}"),
                                            ),
                                    },
                                );
                                ctx.articles_failed.fetch_add(1, Ordering::Relaxed);
                                ctx.resolve_one();
                                last_progress.store(pool.elapsed_ms(), Ordering::Relaxed);
                            }
                            Err(_) => {}
                        }
                    }
                    Err(
                        NntpError::ArticleNotFound(_)
                        | NntpError::NoSuchGroup(_)
                        | NntpError::NoArticleSelected(_),
                    ) => {
                        // 430 and 411/412/420/423 are article-level "not
                        // here" answers; the pipelined connection stays usable.
                        send_outcome(&ctx, &item, refused(primary_server));
                        if handle_article_not_available(
                            &mut item,
                            primary_server,
                            &pool.servers,
                            &pool.server_health,
                            &ctx,
                            &pool.work_queue,
                            worker_id,
                            crate::article_failure::ArticleFailureKind::NotFound,
                            "Article not found on any server",
                        ) {
                            last_progress.store(pool.elapsed_ms(), Ordering::Relaxed);
                        }
                    }
                    Err(NntpError::Connection(_) | NntpError::Io(_)) => {
                        send_outcome(&ctx, &item, transport(primary_server));
                        increment_counter("nntp.transient_reconnects");
                        warn!(
                            job_id = %item.job_id,
                            file_id = %item.file_id,
                            segment_number = item.segment_number,
                            worker = %worker_id,
                            server = %primary_server.name,
                            host = %primary_server.host,
                            article = %item.message_id,
                            in_flight = in_flight_items.len(),
                            consecutive_errors,
                            "Pipeline: connection lost during receive — re-queuing all"
                        );
                        pool.work_queue.push_front(item);
                        requeue_all(&mut in_flight_items, &pool.work_queue);
                        consecutive_errors += 1;
                        if consecutive_errors > MAX_RECONNECT_ATTEMPTS {
                            return WorkerExit::Reconnect;
                        }
                        tokio::time::sleep(RECONNECT_DELAY).await;
                        return WorkerExit::Reconnect;
                    }
                    Err(e) => {
                        send_outcome(&ctx, &item, transport(primary_server));
                        increment_counter("nntp.transient_reconnects");
                        let failure = crate::article_failure::ArticleFailure::from_nntp(
                            &e,
                            &primary_server.id,
                        );
                        warn!(
                            job_id = %item.job_id,
                            file_id = %item.file_id,
                            segment_number = item.segment_number,
                            message_id = %item.message_id,
                            server_id = %primary_server.id,
                            worker_id = %worker_id,
                            original_failure_kind = failure.kind.as_str(),
                            attempt = item.tries_on_current + 1,
                            "Transient pipeline error — re-queuing in-flight work and reconnecting: {e}"
                        );
                        // Only provider-level failures count against the
                        // circuit breaker, as in the serial path. Article-level
                        // answers (protocol errors such as 451, timeouts) are
                        // capped per server by the outage-try limit below;
                        // counting them let a few bad articles pause the whole
                        // server.
                        if counts_against_circuit_breaker(&e) {
                            let is_auth = matches!(
                                failure.kind,
                                crate::article_failure::ArticleFailureKind::AuthFailed
                                    | crate::article_failure::ArticleFailureKind::PermissionDenied
                            );
                            pool.server_health
                                .lock()
                                .entry(primary_server.id.clone())
                                .or_default()
                                .record_failure(is_auth, &e.to_string());
                            pool.report_provider_outage();
                        }
                        // A server-wide outage (502, 480, 403) says nothing
                        // about the article, so it is re-queued without
                        // spending a try: the circuit breaker decides when the
                        // server is unusable. A timeout or a dropped
                        // connection on a server that is otherwise serving
                        // articles may be this one article, so those are
                        // capped — otherwise one poison article is retried at
                        // the head of the queue forever.
                        let server_wide = matches!(
                            failure.kind,
                            crate::article_failure::ArticleFailureKind::ServerDown
                                | crate::article_failure::ArticleFailureKind::AuthFailed
                                | crate::article_failure::ArticleFailureKind::PermissionDenied
                        );
                        let article_outage = matches!(
                            failure.kind,
                            crate::article_failure::ArticleFailureKind::Timeout
                                | crate::article_failure::ArticleFailureKind::ConnectionClosed
                                | crate::article_failure::ArticleFailureKind::Other
                        );
                        if server_wide {
                            pool.work_queue.push_front(item);
                        } else if article_outage {
                            item.outage_tries += 1;
                            if item.outage_tries < MAX_OUTAGE_TRIES_PER_SERVER {
                                pool.work_queue.push_front(item);
                            } else if retry_or_fail_over(
                                item,
                                primary_server,
                                pool,
                                &ctx,
                                worker_id,
                                failure.kind,
                                &e.to_string(),
                            ) {
                                last_progress.store(pool.elapsed_ms(), Ordering::Relaxed);
                            }
                        } else if retry_or_fail_over(
                            item,
                            primary_server,
                            pool,
                            &ctx,
                            worker_id,
                            failure.kind,
                            &e.to_string(),
                        ) {
                            last_progress.store(pool.elapsed_ms(), Ordering::Relaxed);
                        }
                        requeue_all(&mut in_flight_items, &pool.work_queue);
                        return WorkerExit::Reconnect;
                    }
                }
            }
            Ok(None) => {
                // No in-flight requests — loop will fill more.
            }
            Err(e) => {
                warn!(
                    worker = %worker_id,
                    server = %primary_server.name,
                    host = %primary_server.host,
                    error = %e,
                    in_flight = in_flight_items.len(),
                    consecutive_errors,
                    "Pipeline receive error"
                );
                requeue_all(&mut in_flight_items, &pool.work_queue);
                consecutive_errors += 1;
                if consecutive_errors > MAX_RECONNECT_ATTEMPTS {
                    return WorkerExit::Reconnect;
                }
                tokio::time::sleep(RECONNECT_DELAY).await;
                return WorkerExit::Reconnect;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Connection with retry
// ---------------------------------------------------------------------------

async fn connect_with_retry(
    conn: &mut NntpConnection,
    server: &ServerConfig,
    worker_id: &str,
    server_health: &ServerHealthMap,
    all_servers: &Arc<Mutex<Vec<ServerConfig>>>,
) -> Result<(), String> {
    for attempt in 1..=MAX_RECONNECT_ATTEMPTS {
        {
            let health = server_health.lock();
            if let Some(h) = health.get(&server.id)
                && !h.is_available()
            {
                return Err(format!(
                    "Server circuit-broken: {}",
                    h.reason.as_deref().unwrap_or("unknown")
                ));
            }
        }

        let current_config = all_servers
            .lock()
            .iter()
            .find(|s| s.id == server.id)
            .cloned()
            .unwrap_or_else(|| server.clone());

        info!(
            worker = %worker_id,
            server = %current_config.name,
            host = %current_config.host,
            port = current_config.port,
            attempt,
            max_attempts = MAX_RECONNECT_ATTEMPTS,
            "Connect attempt starting"
        );
        match conn.connect(&current_config).await {
            Ok(()) => {
                info!(
                    worker = %worker_id,
                    server = %current_config.name,
                    host = %current_config.host,
                    attempt,
                    "Connect attempt succeeded"
                );
                server_health
                    .lock()
                    .entry(server.id.clone())
                    .or_default()
                    .record_success();
                return Ok(());
            }
            Err(e) => {
                let is_auth = matches!(e, NntpError::Auth(_) | NntpError::ServiceUnavailable(_));
                {
                    let mut health = server_health.lock();
                    let entry = health.entry(server.id.clone()).or_default();
                    entry.record_failure(is_auth, &e.to_string());
                    if !entry.is_available() {
                        warn!(
                            worker = %worker_id,
                            server = %current_config.name,
                            host = %current_config.host,
                            error = %e,
                            cooldown_secs = if is_auth { AUTH_FAILURE_COOLDOWN.as_secs() } else { TRANSIENT_FAILURE_COOLDOWN.as_secs() },
                            "Server circuit-broken — stopping all connection attempts"
                        );
                        return Err(format!("Server circuit-broken: {e}"));
                    }
                }

                warn!(
                    worker = %worker_id,
                    server = %current_config.name,
                    host = %current_config.host,
                    attempt,
                    max_attempts = MAX_RECONNECT_ATTEMPTS,
                    error = %e,
                    is_auth,
                    "Connect attempt FAILED: {e}"
                );

                if is_auth {
                    return Err(format!("Auth/permission failure: {e}"));
                }

                if attempt < MAX_RECONNECT_ATTEMPTS {
                    info!(
                        worker = %worker_id,
                        server = %current_config.name,
                        delay_secs = RECONNECT_DELAY.as_secs(),
                        "Waiting before retry"
                    );
                    tokio::time::sleep(RECONNECT_DELAY).await;
                    *conn = NntpConnection::new(worker_id.to_string());
                } else {
                    return Err(format!(
                        "All {MAX_RECONNECT_ATTEMPTS} connect attempts failed: {e}"
                    ));
                }
            }
        }
    }
    Err("Connect retry loop exited unexpectedly".into())
}

// ---------------------------------------------------------------------------
// Helpers: re-queue, not-available routing, par2 sort key
// ---------------------------------------------------------------------------

/// Handle an article that's not available on this server (not found, decode
/// error, etc.): mark the server as tried and either requeue or mark failed.
///
/// `kind` lets the failure be classified — if every server has been tried
/// and the failure was per-server (NotFound, ServerDown, …), we promote it
/// to a definitive `NotFound` for the hopeless tracker. DecodeError stays
/// classified as DecodeError because it's typically not server-specific.
#[allow(clippy::too_many_arguments)]
fn handle_article_not_available(
    item: &mut WorkItem,
    primary_server: &ServerConfig,
    all_servers: &Arc<Mutex<Vec<ServerConfig>>>,
    server_health: &ServerHealthMap,
    ctx: &Arc<JobContext>,
    work_queue: &Arc<SharedWorkQueue>,
    worker_id: &str,
    kind: crate::article_failure::ArticleFailureKind,
    error_msg: &str,
) -> bool {
    item.provider_outcomes
        .insert(primary_server.id.clone(), kind);
    if !item.tried_servers.contains(&primary_server.id) {
        item.tried_servers.push(primary_server.id.clone());
    }
    item.tries_on_current = 0;
    item.outage_tries = 0;

    let all_definitive = all_enabled_providers_definitive(
        &all_servers.lock(),
        &server_health.lock(),
        &item.provider_outcomes,
    );

    debug!(
        article = %item.message_id,
        server = %primary_server.id,
        kind = kind.as_str(),
        tried_count = item.tried_servers.len(),
        all_definitive,
        "Article returned error on this server"
    );

    let all_refused = item
        .provider_outcomes
        .values()
        .all(|k| *k == crate::article_failure::ArticleFailureKind::NotFound);
    if all_definitive && all_refused && item.confirm_rounds < CONFIRM_ASKS_PER_SERVER {
        // Every server refused within one fast round. Ask them all again
        // after the busy-provider guard, through the normal queue, so the
        // article stays unresolved and the re-ask uses the pool's own
        // connections and permits.
        item.confirm_rounds += 1;
        item.provider_outcomes.clear();
        item.tried_servers.clear();
        let item = item.clone();
        let work_queue = Arc::clone(work_queue);
        tokio::spawn(async move {
            tokio::time::sleep(CONFIRM_ROUND_DELAY).await;
            work_queue.push_front(item);
        });
        return false;
    }

    // (debug log immediately below was added for observability)
    if all_definitive {
        fail_article_everywhere(item, &primary_server.id, ctx, worker_id, kind, error_msg);
        true
    } else {
        // push_FRONT (not push_back): put the failed item at the front of the
        // queue so the next eligible server picks it up IMMEDIATELY instead
        // of queueing behind thousands of fresh items. Same-server workers
        // rotate the item back via pop_workable's existing skip-and-push_back
        // logic. This transforms a retention-dead NZB from "11+ minutes per
        // article's full-server cascade" into "fractions of a second per
        // cascade" — the dominant failure mode for hung downloads.
        work_queue.push_front(item.clone());
        false
    }
}

/// Handle a non-connection error for `item` on `primary_server`: retry it on
/// the same server up to [`MAX_TRIES_PER_SERVER`] times, then record the
/// error as this server's definitive outcome so the article fails over to
/// the next server (or fails once every server has an outcome). Without the
/// cap such errors were re-queued for the same server forever.
///
/// Returns `true` when the article was resolved as failed.
fn retry_or_fail_over(
    mut item: WorkItem,
    primary_server: &ServerConfig,
    pool: &Arc<WorkerPool>,
    ctx: &Arc<JobContext>,
    worker_id: &str,
    kind: crate::article_failure::ArticleFailureKind,
    message: &str,
) -> bool {
    item.tries_on_current += 1;
    if item.tries_on_current < MAX_TRIES_PER_SERVER {
        pool.work_queue.push_front(item);
        return false;
    }
    handle_article_not_available(
        &mut item,
        primary_server,
        &pool.servers,
        &pool.server_health,
        ctx,
        &pool.work_queue,
        worker_id,
        kind,
        &format!(
            "Article failed on every server; last error after {MAX_TRIES_PER_SERVER} attempts: {message}"
        ),
    )
}

/// Resolve an article as failed: every required provider has given a
/// definitive outcome. `server_id` is the provider that answered last.
fn fail_article_everywhere(
    item: &WorkItem,
    server_id: &str,
    ctx: &Arc<JobContext>,
    worker_id: &str,
    kind: crate::article_failure::ArticleFailureKind,
    error_msg: &str,
) {
    let mut provider_outcomes = item
        .provider_outcomes
        .iter()
        .map(|(server, outcome)| format!("{server}={}", outcome.as_str()))
        .collect::<Vec<_>>();
    provider_outcomes.sort_unstable();
    let outcomes = provider_outcomes.join(",");
    let all_not_found = item
        .provider_outcomes
        .values()
        .all(|outcome| *outcome == crate::article_failure::ArticleFailureKind::NotFound);
    let final_kind = if all_not_found {
        crate::article_failure::ArticleFailureKind::NotFound
    } else {
        crate::article_failure::ArticleFailureKind::DecodeError
    };
    warn!(
        job_id = %item.job_id,
        file_id = %item.file_id,
        segment_number = item.segment_number,
        message_id = %item.message_id,
        server_id = %server_id,
        worker_id = %worker_id,
        original_failure_kind = kind.as_str(),
        terminal_failure_kind = final_kind.as_str(),
        attempt = item.tried_servers.len(),
        provider_outcomes = %outcomes,
        "{error_msg}"
    );
    let final_failure = if final_kind == crate::article_failure::ArticleFailureKind::DecodeError {
        crate::article_failure::ArticleFailure::decode_error(
            server_id,
            format!("{error_msg}; provider outcomes: {outcomes}"),
        )
    } else {
        increment_counter("articles.explicit_global_absence");
        crate::article_failure::ArticleFailure::not_found_anywhere(server_id, outcomes)
    };
    ctx.slices.record_failure(
        &item.file_id,
        final_kind == crate::article_failure::ArticleFailureKind::NotFound,
    );
    try_send_progress(
        &ctx.progress_tx,
        &item.job_id,
        ProgressUpdate::ArticleFailed {
            job_id: item.job_id.clone(),
            file_id: item.file_id.clone(),
            segment_number: item.segment_number,
            failure: final_failure,
        },
    );
    ctx.articles_failed.fetch_add(1, Ordering::Relaxed);
    ctx.resolve_one();
}

/// Whether every provider whose answer is required has given a definitive
/// outcome for an article.
///
/// Every enabled server is required, except an `optional` server that is
/// currently unavailable (circuit open, e.g. it cannot be reached). A
/// required server that is down still blocks the decision: an outage is not
/// evidence that the article is missing. An optional server is a best-effort
/// extra, so it must not hold articles hostage while it is down.
fn all_enabled_providers_definitive(
    servers: &[ServerConfig],
    health: &HashMap<String, ServerHealth>,
    outcomes: &HashMap<String, crate::article_failure::ArticleFailureKind>,
) -> bool {
    servers
        .iter()
        .filter(|server| server.enabled)
        .filter(|server| {
            !(server.optional && health.get(&server.id).is_some_and(|h| !h.is_available()))
        })
        .all(|server| outcomes.contains_key(&server.id))
}

/// Whether a fetch error says the provider itself is unhealthy, and so should
/// count against its circuit breaker: connection/TLS, auth, permission, 502
/// and timeouts. A server that accepts the connection and then black-holes
/// the read is as unhealthy as one that refuses it. Article-level answers
/// never count.
fn counts_against_circuit_breaker(e: &NntpError) -> bool {
    matches!(
        e,
        NntpError::Connection(_)
            | NntpError::Io(_)
            | NntpError::Tls(_)
            | NntpError::Auth(_)
            | NntpError::AuthRequired(_)
            | NntpError::PermissionDenied(_)
            | NntpError::ServiceUnavailable(_)
            | NntpError::Timeout(_)
    )
}

/// Re-queue all in-flight items back to the work queue (on connection loss).
fn requeue_all(in_flight: &mut HashMap<u64, WorkItem>, work_queue: &Arc<SharedWorkQueue>) {
    let items: Vec<WorkItem> = in_flight.drain().map(|(_, item)| item).collect();
    for item in items {
        work_queue.push_front(item);
    }
}

/// The job's recovery volumes as NZB file id → `(first exponent, blocks)`,
/// or none when deferring them is unsafe: no PAR2 index to build a slice map
/// from, or a volume whose exponent or block count the name does not give.
fn deferrable_volumes(job: &NzbJob) -> HashMap<String, (u32, u32)> {
    if !job
        .files
        .iter()
        .any(|f| crate::slice_tracker::is_par2_index(&f.filename))
    {
        return HashMap::new();
    }
    let mut volumes = HashMap::new();
    for file in &job.files {
        if par2_sort_key(&file.filename) != 1 {
            continue;
        }
        match (file.par2_vol, file.par2_blocks) {
            (Some(vol), Some(blocks)) => {
                volumes.insert(file.id.clone(), (vol, blocks));
            }
            _ => return HashMap::new(),
        }
    }
    volumes
}

/// Sort key for work-queue prioritisation of PAR2 files. Index files (0)
/// first, then volume files (1), then data files (2).
fn par2_sort_key(filename: &str) -> u8 {
    let lower = filename.to_lowercase();
    if lower.ends_with(".par2") {
        if lower.contains(".vol") { 1 } else { 0 }
    } else {
        2
    }
}

pub fn has_known_extension(name: &str) -> bool {
    let lower = name.to_lowercase();
    if let Some(dot_pos) = lower.rfind('.') {
        let ext = &lower[dot_pos + 1..];
        matches!(
            ext,
            "rar"
                | "r00"
                | "r01"
                | "r02"
                | "r03"
                | "r04"
                | "r05"
                | "zip"
                | "7z"
                | "gz"
                | "bz2"
                | "xz"
                | "tar"
                | "mkv"
                | "mp4"
                | "avi"
                | "wmv"
                | "ts"
                | "m4v"
                | "mov"
                | "mpg"
                | "mpeg"
                | "mp3"
                | "flac"
                | "ogg"
                | "m4a"
                | "aac"
                | "wav"
                | "srt"
                | "sub"
                | "idx"
                | "ass"
                | "ssa"
                | "sup"
                | "nfo"
                | "jpg"
                | "jpeg"
                | "png"
                | "gif"
                | "bmp"
                | "par2"
                | "001"
                | "002"
                | "003"
                | "004"
                | "005"
        )
    } else {
        false
    }
}

// ---------------------------------------------------------------------------
// Public helper used by queue_manager: build work items + context for a job
// ---------------------------------------------------------------------------

/// Build the WorkItems for a job's unfinished articles and an initialised
/// JobContext. Called by QueueManager before [`WorkerPool::submit_job`].
pub(crate) fn build_job_submission(
    job: &NzbJob,
    progress_tx: mpsc::Sender<ProgressUpdate>,
    enabled_server_ids: &[String],
) -> (Arc<JobContext>, Vec<WorkItem>) {
    let assembler = Arc::new(FileAssembler::new());
    for file in &job.files {
        let output_path = job.work_dir.join(&file.filename);
        let completed_segments: Vec<u32> = file
            .articles
            .iter()
            .filter(|article| article.downloaded)
            .map(|article| article.segment_number)
            .collect();
        // Register the declared segment numbers rather than assuming 1..=N:
        // an NZB may skip or repeat numbers.
        let segment_numbers: Vec<u32> = file
            .articles
            .iter()
            .map(|article| article.segment_number)
            .collect();
        // Only trust the NZB size when every segment reported one. The parser
        // turns a missing or unparsable `bytes` attribute into 0, so a file
        // whose segments omit it sums to 0 (or to an understatement) and
        // every article would then be rejected as past the declared size.
        // With no bound, the write check falls back to the article's size=.
        let declared_bytes = file
            .articles
            .iter()
            .all(|article| article.bytes > 0)
            .then_some(file.bytes);
        if let Err(e) = assembler.register_file_with_segment_numbers(
            &job.id,
            &file.id,
            output_path,
            &segment_numbers,
            &completed_segments,
            declared_bytes,
        ) {
            error!(file = %file.filename, "Failed to register file for assembly: {e}");
        }
    }

    // `tried_servers` on a persisted article is only ever written when the
    // article reached a terminal `ArticleFailed`. That is only still terminal
    // if every server enabled now was one of them: a server added or
    // re-enabled since the failure has never been asked, so the article must
    // be fetched again rather than skipped.
    let is_previously_failed = |article: &nzb_nntp::Article| {
        !article.downloaded
            && !enabled_server_ids.is_empty()
            && enabled_server_ids
                .iter()
                .all(|server| article.tried_servers.iter().any(|tried| tried == server))
    };
    let previously_failed = job
        .files
        .iter()
        .flat_map(|file| file.articles.iter())
        .filter(|article| is_previously_failed(article))
        .count();

    let work_items: Vec<WorkItem> = job
        .files
        .iter()
        .flat_map(|file| {
            file.articles
                .iter()
                .filter(|article| !article.downloaded && !is_previously_failed(article))
                .map(move |article| WorkItem {
                    job_id: job.id.clone(),
                    file_id: file.id.clone(),
                    filename: file.filename.clone(),
                    message_id: article.message_id.clone(),
                    segment_number: article.segment_number,
                    tried_servers: article.tried_servers.clone(),
                    provider_outcomes: HashMap::new(),
                    tries_on_current: 0,
                    outage_tries: 0,
                    confirm_rounds: 0,
                })
        })
        .collect();

    let total_remaining = work_items.len();
    let ctx = Arc::new(JobContext::new(
        job,
        assembler,
        progress_tx,
        total_remaining,
    ));
    ctx.articles_failed
        .store(previously_failed, Ordering::Relaxed);
    (ctx, work_items)
}

// ---------------------------------------------------------------------------
// Article fetch with per-server retry
// ---------------------------------------------------------------------------

async fn fetch_article_with_retry(
    conn: &mut NntpConnection,
    item: &WorkItem,
    assembler: &Arc<FileAssembler>,
    cancelled: &AtomicBool,
    pinned_file_sizes: Option<&PinnedSizes>,
    _server: &ServerConfig,
    worker_id: &str,
) -> Result<ProcessResult, ArticleError> {
    // Retries of article-level errors are counted per item by the caller
    // (`WorkItem::tries_on_current`), so each call makes a single attempt.
    let attempt = item.tries_on_current + 1;
    let fetch_start = Instant::now();
    match conn.fetch_article(&item.message_id).await {
        Ok(response) => {
            let fetch_us = fetch_start.elapsed().as_micros();
            let raw_data = response.data.unwrap_or_default();
            debug!(
                worker = %worker_id,
                article = %item.message_id,
                raw_bytes = raw_data.len(),
                fetch_us,
                "NNTP fetch complete"
            );
            // The job may have been aborted while this fetch was in
            // flight. Like the pipelined path, drop the response rather
            // than writing it into an aborted job's files.
            if cancelled.load(Ordering::Relaxed) {
                conn.release_body_buffer(raw_data);
                return Err(ArticleError::Cancelled);
            }
            let (result, raw_data) =
                decode_and_assemble_offloaded(item, raw_data, assembler, pinned_file_sizes).await;
            // Return the buffer to the connection's pool so the next
            // article's fetch reuses it instead of allocating fresh.
            conn.release_body_buffer(raw_data);
            result
        }
        Err(NntpError::ArticleNotFound(_)) => {
            debug!(
                worker = %worker_id,
                article = %item.message_id,
                "Article not found (430) — will try next server"
            );
            Err(ArticleError::ArticleNotFound)
        }
        Err(e @ (NntpError::NoSuchGroup(_) | NntpError::NoArticleSelected(_))) => {
            // 411/412/420/423: an article-level "not here" answer. Retrying on
            // this server cannot change it.
            debug!(
                worker = %worker_id,
                article = %item.message_id,
                error = %e,
                "Article unavailable on this server — will try next server"
            );
            Err(ArticleError::ArticleNotFound)
        }
        Err(e @ (NntpError::Connection(_) | NntpError::Io(_))) => {
            warn!(
                worker = %worker_id,
                article = %item.message_id,
                attempt,
                error = %e,
                conn_state = ?conn.state,
                "Connection/IO error during fetch — connection lost"
            );
            Err(ArticleError::ConnectionLost(format!(
                "Connection error on attempt {attempt}: {e}"
            )))
        }
        Err(e @ NntpError::Tls(_)) => {
            warn!(
                worker = %worker_id,
                article = %item.message_id,
                attempt,
                error = %e,
                "TLS error during fetch — connection lost"
            );
            Err(ArticleError::ConnectionLost(format!("TLS error: {e}")))
        }
        Err(e @ NntpError::ServiceUnavailable(_)) => {
            warn!(
                worker = %worker_id,
                article = %item.message_id,
                attempt,
                error = %e,
                "Service unavailable (502) during article fetch — likely rate limited or blocked"
            );
            Err(ArticleError::ProviderUnavailable {
                kind: crate::article_failure::ArticleFailureKind::ServerDown,
                message: e.to_string(),
            })
        }
        Err(e @ (NntpError::AuthRequired(_) | NntpError::Auth(_))) => {
            warn!(
                worker = %worker_id,
                article = %item.message_id,
                attempt,
                error = %e,
                "Auth required (480) during article fetch — session expired or rate limited"
            );
            Err(ArticleError::ProviderUnavailable {
                kind: crate::article_failure::ArticleFailureKind::AuthFailed,
                message: e.to_string(),
            })
        }
        Err(e @ NntpError::PermissionDenied(_)) => Err(ArticleError::ProviderUnavailable {
            kind: crate::article_failure::ArticleFailureKind::PermissionDenied,
            message: e.to_string(),
        }),
        Err(e) => {
            // Protocol / unexpected response / timeout and the like. The caller
            // retries up to MAX_TRIES_PER_SERVER times on this server, then
            // records a definitive outcome so the article fails over.
            warn!(
                worker = %worker_id,
                article = %item.message_id,
                attempt,
                max_tries = MAX_TRIES_PER_SERVER,
                error = %e,
                "Article fetch error on this server"
            );
            Err(ArticleError::ServerError {
                kind: crate::article_failure::ArticleFailure::from_nntp(&e, "").kind,
                message: e.to_string(),
            })
        }
    }
}

// ---------------------------------------------------------------------------
// Article processing
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct ProcessResult {
    decoded_bytes: u64,
    /// File offset the decoded bytes were written at.
    write_offset: u64,
    file_complete: bool,
    yenc_filename: Option<String>,
    decode_us: u64,
    assemble_us: u64,
}

#[derive(Debug, thiserror::Error)]
enum ArticleError {
    #[error("Article not found on server")]
    ArticleNotFound,
    #[error("Connection lost: {0}")]
    ConnectionLost(String),
    /// A non-connection error for this article on this server (protocol
    /// error, unexpected response, timeout). Retried a bounded number of
    /// times per server, then treated as a definitive outcome.
    #[error("Server error ({kind:?}): {message}")]
    ServerError {
        kind: crate::article_failure::ArticleFailureKind,
        message: String,
    },
    #[error("Provider unavailable ({kind:?}): {message}")]
    ProviderUnavailable {
        kind: crate::article_failure::ArticleFailureKind,
        message: String,
    },
    #[error("Decode error: {0}")]
    DecodeError(String),
    #[error("Assembly error: {0}")]
    AssemblyError(String),
    /// The job was cancelled while the fetch was in flight; the article was
    /// not written.
    #[error("Job cancelled")]
    Cancelled,
}

/// `(first, min, max, logged)`: the first `=ybegin size=` recorded for one
/// file, the range seen since, and whether the one disagreement line for it
/// has been emitted.
type YencSizeRange = (u64, u64, u64, bool);

/// Choose the file offset a decoded article is written at, rejecting headers
/// that would place it outside the file.
///
/// The offset comes from `=ypart begin=` and is used directly as the `pwrite`
/// position. The bound is the size the NZB declared for the file
/// (`declared_bytes`), not the `size=` the article itself claims: a forged
/// `size=2e12 begin=1e12` would otherwise create a terabyte sparse file.
/// `pinned_size` is the fallback bound, and only when the NZB declared
/// nothing. It is the first `size=` seen for the file, not this article's:
/// a later segment whose `size=` differs is still written, as long as it
/// fits that first size.
/// A multi-segment file with no bound at all — no NZB size and no `size=` —
/// is rejected, as is a `part_end` that does not describe exactly the decoded
/// bytes. A missing `=ypart` on a later segment would overwrite the start of
/// the file and is rejected too.
fn validated_write_offset(
    segment_number: u32,
    decoded: &nzb_decode::YencDecodeResult,
    data_len: u64,
    declared_bytes: Option<u64>,
    pinned_size: Option<u64>,
) -> Result<u64, String> {
    let part_begin = decoded.part_begin;
    let part_end = decoded.part_end;
    // `size=` is not checked against `pinned_size`. Obfuscated reposts name a
    // different original file on every segment, so the two disagree even when
    // the bytes are right; the write bound below is what rejects a forged size.
    let bound = match declared_bytes.or(pinned_size) {
        Some(size) => Some(size),
        None if segment_number <= 1 => None,
        None => {
            return Err(format!(
                "no declared file size for multi-segment file (segment {segment_number})"
            ));
        }
    };
    let begin = match part_begin {
        Some(begin) => begin,
        // A single-part article (no =ypart) can only be the first segment.
        None if segment_number <= 1 => 0,
        None => {
            return Err(format!(
                "no =ypart header on segment {segment_number} of a multi-part file"
            ));
        }
    };
    if let Some(size) = bound {
        if size == 0 {
            return Err("declared file size is 0".to_string());
        }
        if begin >= size {
            return Err(format!(
                "=ypart begin {begin} is past the declared file size {size}"
            ));
        }
        match begin.checked_add(data_len) {
            Some(end) if end <= size => {
                if let Some(part_end) = part_end
                    && part_end != end
                {
                    return Err(format!(
                        "=ypart end {part_end} does not match the decoded bytes ending at {end}"
                    ));
                }
            }
            _ => {
                return Err(format!(
                    "part range begin={begin} len={data_len} exceeds declared file size {size}"
                ));
            }
        }
    }
    Ok(begin)
}

/// Per-file `=ybegin size=` ranges seen so far, shared with the decode tasks.
type PinnedSizes = Arc<Mutex<HashMap<String, YencSizeRange>>>;

/// Run [`decode_and_assemble`] on the blocking pool (WI-1847).
///
/// yEnc decode is CPU-bound and the assembler's positional write can stall in
/// disk writeback for tens of milliseconds at line rate. Running either inline
/// parks a tokio worker, and the HTTP handlers share those workers, so API
/// latency climbed with download speed. `spawn_blocking` hands the work to a
/// pool thread and leaves the worker free; unlike `block_in_place` it does not
/// force the runtime to migrate the worker's other tasks on every article.
///
/// The body buffer is handed back alongside the result so the caller can
/// return it to the connection's pool.
async fn decode_and_assemble_offloaded(
    item: &WorkItem,
    raw_data: Vec<u8>,
    assembler: &Arc<FileAssembler>,
    pinned_file_sizes: Option<&PinnedSizes>,
) -> (Result<ProcessResult, ArticleError>, Vec<u8>) {
    let task_item = item.clone();
    let assembler = Arc::clone(assembler);
    let pinned = pinned_file_sizes.cloned();
    let joined = tokio::task::spawn_blocking(move || {
        let result = decode_and_assemble(&task_item, &raw_data, &assembler, pinned.as_deref());
        (result, raw_data)
    })
    .await;
    match joined {
        Ok(out) => out,
        Err(e) => (
            Err(ArticleError::AssemblyError(format!(
                "decode task failed for {} seg {}: {e}",
                item.filename, item.segment_number
            ))),
            Vec::new(),
        ),
    }
}

#[cfg(test)]
thread_local! {
    /// Decodes run on the current thread, so tests can tell inline from offloaded.
    static DECODES_ON_THREAD: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

fn decode_and_assemble(
    item: &WorkItem,
    raw_data: &[u8],
    assembler: &FileAssembler,
    pinned_file_sizes: Option<&Mutex<HashMap<String, YencSizeRange>>>,
) -> Result<ProcessResult, ArticleError> {
    #[cfg(test)]
    DECODES_ON_THREAD.with(|n| n.set(n.get() + 1));
    let decode_start = Instant::now();
    let decoded = decode_yenc(raw_data).map_err(|e| {
        ArticleError::DecodeError(format!(
            "yEnc decode failed for {} seg {}: {e}",
            item.filename, item.segment_number
        ))
    })?;
    let decode_us = decode_start.elapsed().as_micros();

    // Remember the `=ybegin size=` values seen for this file, but only a real
    // one: an article that omits `size=` must not become the record. Obfuscated
    // reposts put a different original file's `size=` on every segment of one
    // NZB `<file>`, so the values scatter both above and below whatever arrived
    // first; that is not the NZB size (which is the sum of encoded segment
    // `bytes`) and it is not corruption. The range is logged once per file, the
    // first time a segment widens it. The NZB size is what bounds the write.
    // Without one, the first `size=` recorded is the only bound the write has,
    // and a later segment is written only when it fits that first size.
    let declared = assembler.declared_bytes(&item.job_id, &item.file_id);
    let pinned_size = match (pinned_file_sizes, decoded.file_size) {
        (Some(sizes), Some(size)) => {
            let mut sizes = sizes.lock();
            match sizes.entry(item.file_id.clone()) {
                std::collections::hash_map::Entry::Occupied(mut existing) => {
                    let (first, min, max, logged) = *existing.get();
                    let widened = size < min || size > max;
                    if widened && !logged {
                        // Once per file. Arrival order is random, so logging on
                        // every widening would be about 2·ln(n) lines per file.
                        info!(
                            file_id = %item.file_id,
                            segment_number = item.segment_number,
                            size_min = min.min(size),
                            size_max = max.max(size),
                            "=ybegin size= differs between segments of one NZB file; bytes are written, bounded by the NZB size or the first size= seen"
                        );
                    }
                    if widened || !logged {
                        existing.insert((first, min.min(size), max.max(size), logged || widened));
                    }
                    Some(first)
                }
                std::collections::hash_map::Entry::Vacant(slot) => {
                    slot.insert((size, size, size, false));
                    Some(size)
                }
            }
        }
        (Some(sizes), None) => sizes.lock().get(&item.file_id).map(|(first, ..)| *first),
        (None, size) => size,
    };
    // With an NZB size the record above is only a log. Without one, the first
    // `size=` seen is the only bound the write has.
    let pinned_bound = if declared.is_some() {
        None
    } else {
        pinned_size
    };

    let decoded_len = decoded.data.len() as u64;
    let data_begin = validated_write_offset(
        item.segment_number,
        &decoded,
        decoded_len,
        declared,
        pinned_bound,
    )
    .map_err(|reason| {
        ArticleError::DecodeError(format!(
            "Corrupt yEnc article for {} seg {}: {reason}",
            item.filename, item.segment_number
        ))
    })?;
    let yenc_filename = decoded.filename;

    let assemble_start = Instant::now();
    let file_complete = assembler
        .assemble_article(
            &item.job_id,
            &item.file_id,
            item.segment_number,
            data_begin,
            &decoded.data,
        )
        .map_err(|e| {
            ArticleError::AssemblyError(format!(
                "Assembly failed for {} seg {}: {e}",
                item.filename, item.segment_number
            ))
        })?;
    let assemble_us = assemble_start.elapsed().as_micros();

    debug!(
        file = %item.filename,
        segment = item.segment_number,
        raw_bytes = raw_data.len(),
        decoded_bytes = decoded_len,
        decode_us,
        assemble_us,
        "Article decode+assemble timing"
    );

    Ok(ProcessResult {
        decoded_bytes: decoded_len,
        write_offset: data_begin,
        file_complete,
        yenc_filename,
        decode_us: decode_us as u64,
        assemble_us: assemble_us as u64,
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn worker_pool_without_servers() -> Arc<WorkerPool> {
        WorkerPool::new(
            Arc::new(Mutex::new(Vec::new())),
            Arc::new(BandwidthLimiter::new(Default::default())),
            Arc::new(ConnectionTracker::new()),
            0,
        )
    }

    #[tokio::test(start_paused = true)]
    async fn worker_shutdown_uses_one_shared_deadline() {
        let handles = (0..3)
            .map(|_| tokio::spawn(std::future::pending()))
            .collect();
        let shutdown = tokio::spawn(await_worker_shutdown(handles, Duration::from_secs(10)));
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(9)).await;
        tokio::task::yield_now().await;
        assert!(!shutdown.is_finished());
        tokio::time::advance(Duration::from_secs(2)).await;
        shutdown.await.unwrap();
    }

    fn test_job(job_id: &str, root: &std::path::Path) -> NzbJob {
        NzbJob {
            slice_map: None,
            held_par2_bytes: 0,
            id: job_id.to_string(),
            name: job_id.to_string(),
            category: "Default".to_string(),
            status: nzb_core::models::JobStatus::Downloading,
            priority: nzb_core::models::Priority::Normal,
            total_bytes: 1,
            downloaded_bytes: 0,
            file_count: 1,
            files_completed: 0,
            article_count: 1,
            articles_downloaded: 0,
            articles_failed: 0,
            added_at: chrono::Utc::now(),
            completed_at: None,
            work_dir: root.to_path_buf(),
            output_dir: root.join("complete"),
            password: None,
            error_message: None,
            speed_bps: 0,
            pp_override: None,
            delete_archives: None,
            server_stats: Vec::new(),
            files: Vec::new(),
        }
    }

    fn insert_test_context(pool: &WorkerPool, job: &NzbJob, assembler: Arc<FileAssembler>) {
        let (progress_tx, _progress_rx) = mpsc::channel(1);
        let ctx = Arc::new(JobContext::new(job, assembler, progress_tx, 1));
        pool.job_contexts.lock().insert(job.id.clone(), ctx);
    }

    #[test]
    fn submission_preserves_declared_segment_numbers() {
        let temp = tempfile::tempdir().unwrap();
        let mut job = test_job("segment-identity", temp.path());
        let article = |message_id: &str, segment_number: u32, bytes: u64| nzb_nntp::Article {
            message_id: message_id.to_string(),
            segment_number,
            bytes,
            downloaded: false,
            data_begin: None,
            data_size: None,
            crc32: None,
            tried_servers: Vec::new(),
            tries: 0,
        };
        job.files.push(nzb_core::models::NzbFile {
            id: "file-1".into(),
            filename: "out-of-order.bin".into(),
            bytes: 1_000,
            bytes_downloaded: 0,
            is_par2: false,
            par2_setname: None,
            par2_vol: None,
            par2_blocks: None,
            assembled: false,
            groups: Vec::new(),
            articles: vec![article("third", 3, 300), article("first", 1, 100)],
        });
        let (tx, _rx) = mpsc::channel(4);

        let (_ctx, items) = build_job_submission(&job, tx, &["srv1".into()]);

        assert_eq!(
            items
                .iter()
                .map(|item| (item.message_id.as_str(), item.segment_number))
                .collect::<Vec<_>>(),
            vec![("third", 3), ("first", 1)]
        );
    }

    #[test]
    fn file_without_segment_bytes_still_assembles() {
        let temp = tempfile::tempdir().unwrap();
        let mut job = test_job("no-bytes", temp.path());
        let article = |message_id: &str, segment_number: u32| nzb_nntp::Article {
            message_id: message_id.to_string(),
            segment_number,
            bytes: 0,
            downloaded: false,
            data_begin: None,
            data_size: None,
            crc32: None,
            tried_servers: Vec::new(),
            tries: 0,
        };
        job.files.push(nzb_core::models::NzbFile {
            id: "file-1".into(),
            filename: "out.bin".into(),
            bytes: 0,
            bytes_downloaded: 0,
            is_par2: false,
            par2_setname: None,
            par2_vol: None,
            par2_blocks: None,
            assembled: false,
            groups: Vec::new(),
            articles: vec![article("seg1", 1), article("seg2", 2)],
        });
        let (tx, _rx) = mpsc::channel(4);
        let (ctx, _items) = build_job_submission(&job, tx, &["srv1".into()]);
        // A missing bytes= attribute parses as 0, so there is no NZB bound.
        assert_eq!(ctx.assembler.declared_bytes(&job.id, "file-1"), None);

        let (seg1, _) = yenc_simd::encode_article(b"AAAA", "out.bin", 1, 2, 0, 8);
        let (seg2, _) = yenc_simd::encode_article(b"BBBB", "out.bin", 2, 2, 4, 8);
        let item = |segment_number| {
            let mut item = make_item(&job.id, "msg@test", "out.bin");
            item.file_id = "file-1".into();
            item.segment_number = segment_number;
            item
        };
        decode_and_assemble(&item(1), &seg1, &ctx.assembler, None).unwrap();
        let second = decode_and_assemble(&item(2), &seg2, &ctx.assembler, None).unwrap();

        assert!(second.file_complete);
        assert_eq!(
            std::fs::read(temp.path().join("out.bin")).unwrap(),
            b"AAAABBBB"
        );
    }

    #[test]
    fn submission_with_segment_gap_completes_when_all_declared_segments_written() {
        let temp = tempfile::tempdir().unwrap();
        let mut job = test_job("segment-gap", temp.path());
        let article = |message_id: &str, segment_number: u32| nzb_nntp::Article {
            message_id: message_id.to_string(),
            segment_number,
            bytes: 4,
            downloaded: false,
            data_begin: None,
            data_size: None,
            crc32: None,
            tried_servers: Vec::new(),
            tries: 0,
        };
        job.files.push(nzb_core::models::NzbFile {
            id: "file-1".into(),
            filename: "gap.bin".into(),
            bytes: 16,
            bytes_downloaded: 0,
            is_par2: false,
            par2_setname: None,
            par2_vol: None,
            par2_blocks: None,
            assembled: false,
            groups: Vec::new(),
            // Segment 3 is absent from the NZB.
            articles: vec![article("s1", 1), article("s2", 2), article("s4", 4)],
        });
        let (tx, _rx) = mpsc::channel(4);
        let (ctx, _items) = build_job_submission(&job, tx, &["srv1".into()]);

        let asm = &ctx.assembler;
        assert!(
            !asm.assemble_article(&job.id, "file-1", 1, 0, b"AAAA")
                .unwrap()
        );
        assert!(
            !asm.assemble_article(&job.id, "file-1", 2, 4, b"BBBB")
                .unwrap()
        );
        // Segment 4 must be accepted and must complete the file.
        assert!(
            asm.assemble_article(&job.id, "file-1", 4, 12, b"DDDD")
                .unwrap()
        );
        assert!(asm.is_file_complete(&job.id, "file-1"));
        assert_eq!(asm.get_file_progress(&job.id, "file-1"), (3, 3));
    }

    #[test]
    fn submission_does_not_requeue_previously_failed_articles() {
        let temp = tempfile::tempdir().unwrap();
        let mut job = test_job("previously-failed", temp.path());
        let article = |message_id: &str, segment_number: u32, tried: &[&str]| nzb_nntp::Article {
            message_id: message_id.to_string(),
            segment_number,
            bytes: 100,
            downloaded: false,
            data_begin: None,
            data_size: None,
            crc32: None,
            tried_servers: tried.iter().map(|s| s.to_string()).collect(),
            tries: tried.len() as u32,
        };
        job.files.push(nzb_core::models::NzbFile {
            id: "file-1".into(),
            filename: "partial.bin".into(),
            bytes: 200,
            bytes_downloaded: 0,
            is_par2: false,
            par2_setname: None,
            par2_vol: None,
            par2_blocks: None,
            assembled: false,
            groups: Vec::new(),
            articles: vec![article("pending", 1, &[]), article("failed", 2, &["srv1"])],
        });
        let (tx, _rx) = mpsc::channel(4);

        let (ctx, items) = build_job_submission(&job, tx, &["srv1".into()]);

        assert_eq!(
            items
                .iter()
                .map(|item| item.message_id.as_str())
                .collect::<Vec<_>>(),
            vec!["pending"]
        );
        assert_eq!(ctx.articles_remaining.load(Ordering::Relaxed), 1);
        assert_eq!(ctx.articles_failed.load(Ordering::Relaxed), 1);

        // A server enabled since the failure has never been asked, so the
        // article is fetched again instead of being skipped.
        let (tx, _rx) = mpsc::channel(4);
        let (ctx, items) = build_job_submission(&job, tx, &["srv1".into(), "srv2".into()]);
        assert_eq!(items.len(), 2);
        assert_eq!(ctx.articles_failed.load(Ordering::Relaxed), 0);
    }

    fn decode_test_item(job_id: &str, segment_number: u32) -> WorkItem {
        let mut item = make_item(job_id, "msg@test", "out.bin");
        item.segment_number = segment_number;
        item
    }

    #[tokio::test]
    async fn offloaded_decode_runs_off_the_async_thread_and_returns_the_buffer() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("out.bin");
        let assembler = Arc::new(FileAssembler::new());
        assembler
            .register_file("j1", "f1", path.clone(), 1)
            .unwrap();
        let (raw, _) = yenc_simd::encode_article(b"payload", "out.bin", 1, 1, 0, 7);
        let pinned: PinnedSizes = Arc::new(Mutex::new(HashMap::new()));

        let before = DECODES_ON_THREAD.with(|n| n.get());
        let (result, buffer) = decode_and_assemble_offloaded(
            &decode_test_item("j1", 1),
            raw.clone(),
            &assembler,
            Some(&pinned),
        )
        .await;

        // The decode and write ran on a blocking-pool thread, not this one.
        assert_eq!(DECODES_ON_THREAD.with(|n| n.get()), before);
        let result = result.unwrap();
        assert!(result.file_complete);
        assert_eq!(result.decoded_bytes, 7);
        assert_eq!(std::fs::read(&path).unwrap(), b"payload");
        assert_eq!(buffer, raw, "the body buffer comes back for reuse");
        assert!(pinned.lock().contains_key("f1"));
    }

    #[tokio::test]
    async fn offloaded_decode_reports_decode_errors() {
        let temp = tempfile::tempdir().unwrap();
        let assembler = Arc::new(FileAssembler::new());
        assembler
            .register_file("j1", "f1", temp.path().join("out.bin"), 1)
            .unwrap();
        let (result, buffer) = decode_and_assemble_offloaded(
            &decode_test_item("j1", 1),
            b"not yenc".to_vec(),
            &assembler,
            None,
        )
        .await;
        assert!(
            matches!(result, Err(ArticleError::DecodeError(_))),
            "{result:?}"
        );
        assert_eq!(buffer, b"not yenc");
    }

    /// WI-1847: a stalled write must not starve the async runtime.
    ///
    /// At line rate the assembler's positional write sits in disk writeback for
    /// tens of milliseconds. When that write ran inline on a tokio worker, the
    /// HTTP handlers sharing the worker pool stopped being scheduled, and API
    /// latency climbed with download speed. This holds several writes stalled at
    /// once — more than the runtime has worker threads — and asserts unrelated
    /// async work still makes progress, then that the writes finish correctly
    /// once the disk recovers.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stalled_file_writes_do_not_block_the_async_runtime() {
        const STALLED_WRITES: usize = 4;

        let temp = tempfile::tempdir().unwrap();
        let assembler = Arc::new(FileAssembler::new());
        let gate = Arc::new(nzb_decode::WriteStall::new());
        gate.hold();
        assembler.stall_writes(Arc::clone(&gate));

        // If writes ever move back inline, they would park the runtime's own
        // workers and this test would hang instead of failing. The watchdog
        // releases the gate after a generous deadline so the harness always
        // gets a verdict; the assertion below then reports the regression.
        let (finished_tx, finished_rx) = std::sync::mpsc::channel::<()>();
        let (watchdog_tx, watchdog_rx) = std::sync::mpsc::channel::<()>();
        let watchdog_gate = Arc::clone(&gate);
        let watchdog = std::thread::spawn(move || {
            if finished_rx.recv_timeout(Duration::from_secs(10)).is_err() {
                watchdog_gate.release();
                let _ = watchdog_tx.send(());
            }
        });

        let mut expected = Vec::new();
        let mut handles = Vec::new();
        for i in 0..STALLED_WRITES {
            let (file_id, path) = (format!("f{i}"), temp.path().join(format!("out{i}.bin")));
            assembler
                .register_file("j1", &file_id, path.clone(), 1)
                .unwrap();
            let (raw, _) = yenc_simd::encode_article(b"payload", "out.bin", 1, 1, 0, 7);
            expected.push(path);
            let mut item = decode_test_item("j1", 1);
            item.file_id = file_id;
            let assembler = Arc::clone(&assembler);
            handles.push(tokio::spawn(async move {
                let (result, _) = decode_and_assemble_offloaded(&item, raw, &assembler, None).await;
                result
            }));
        }

        // Wait on the runtime itself (not on a wall clock) until every write is
        // parked in the assembler, which is the state that used to hold a worker.
        let mut parked = 0;
        for _ in 0..1_000 {
            parked = gate.stalled_writes();
            if parked as usize == STALLED_WRITES {
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        assert_eq!(parked as usize, STALLED_WRITES, "writes did not park");

        // With the writes still stalled, the runtime must keep running other
        // tasks — this is the progress the API and GUI handlers need.
        let ticks = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let ticker = {
            let ticks = Arc::clone(&ticks);
            tokio::spawn(async move {
                for _ in 0..50 {
                    tokio::time::sleep(Duration::from_millis(1)).await;
                    ticks.fetch_add(1, Ordering::SeqCst);
                }
            })
        };
        ticker.await.unwrap();
        assert_eq!(ticks.load(Ordering::SeqCst), 50);
        assert_eq!(
            gate.stalled_writes() as usize,
            STALLED_WRITES,
            "the writes did not stay stalled while unrelated async work ran; \
             if they are inline on the runtime, the API handlers starve"
        );
        assert!(
            watchdog_rx.try_recv().is_err(),
            "the runtime made no progress for 10s while writes were stalled — \
             writes are blocking the async workers again"
        );

        // The disk recovers: every write completes normally and the files are
        // byte-for-byte what the articles carried.
        gate.release();
        let _ = finished_tx.send(());
        watchdog.join().unwrap();
        for handle in handles {
            let result = handle.await.unwrap().expect("stalled write should finish");
            assert!(result.file_complete);
            assert_eq!(result.decoded_bytes, 7);
        }
        for path in expected {
            assert_eq!(std::fs::read(path).unwrap(), b"payload");
        }
    }

    #[test]
    fn decode_rejects_ypart_offset_beyond_declared_file_size() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("out.bin");
        let assembler = FileAssembler::new();
        assembler
            .register_file("j1", "f1", path.clone(), 2)
            .unwrap();

        // Part 2 of an 8-byte file claims to start at ~1 TB.
        let (raw, _) = yenc_simd::encode_article(b"BBBB", "out.bin", 2, 2, 1_000_000_000_000, 8);
        let result = decode_and_assemble(&decode_test_item("j1", 2), &raw, &assembler, None);

        assert!(
            matches!(result, Err(ArticleError::DecodeError(_))),
            "expected corrupt-article error, got {result:?}"
        );
        assert!(std::fs::metadata(&path).unwrap().len() <= 8);
        assert_eq!(assembler.get_file_progress("j1", "f1"), (0, 2));
    }

    #[test]
    fn decode_rejects_forged_size_past_nzb_declared_bytes() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("out.bin");
        let assembler = FileAssembler::new();
        // The NZB says the file is 8 bytes. The article claims 2 TB and asks
        // to be written at 1 TB, which the article's own size= would allow.
        assembler
            .register_file_with_segment_numbers("j1", "f1", path.clone(), &[1, 2], &[], Some(8))
            .unwrap();

        let (raw, _) = yenc_simd::encode_article(
            b"BBBB",
            "out.bin",
            2,
            2,
            1_000_000_000_000,
            2_000_000_000_000,
        );
        let result = decode_and_assemble(&decode_test_item("j1", 2), &raw, &assembler, None);

        assert!(
            matches!(result, Err(ArticleError::DecodeError(_))),
            "forged size= must not win over the NZB size, got {result:?}"
        );
        assert!(std::fs::metadata(&path).unwrap().len() <= 8);
        assert_eq!(assembler.get_file_progress("j1", "f1"), (0, 2));
    }

    #[test]
    fn decode_rejects_missing_size_on_multi_segment_file() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("out.bin");
        let assembler = FileAssembler::new();
        // No NZB size, and the article carries no size= either.
        assembler
            .register_file("j1", "f1", path.clone(), 2)
            .unwrap();

        let (mut raw, _) = yenc_simd::encode_article(b"BBBB", "out.bin", 2, 2, 4, 8);
        let header_end = raw.windows(2).position(|w| w == b"\r\n").unwrap();
        raw.splice(
            0..header_end,
            b"=ybegin part=2 line=128 name=out.bin".iter().copied(),
        );
        let result = decode_and_assemble(&decode_test_item("j1", 2), &raw, &assembler, None);

        assert!(
            matches!(result, Err(ArticleError::DecodeError(_))),
            "a multi-segment file with no bound at all must be rejected, got {result:?}"
        );
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 0);
        assert_eq!(assembler.get_file_progress("j1", "f1"), (0, 2));
    }

    #[test]
    fn decode_accepts_yenc_size_below_encoded_nzb_byte_sum() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("out.bin");
        let assembler = FileAssembler::new();
        // The NZB `bytes` attributes count the encoded article, so their sum
        // runs a little over the decoded file `=ybegin size=` names. Both
        // segments must be written in full, not rejected or truncated at the
        // NZB sum, and the difference must not be treated as corruption.
        assembler
            .register_file_with_segment_numbers("j1", "f1", path.clone(), &[1, 2], &[], Some(9))
            .unwrap();

        let (seg1, _) = yenc_simd::encode_article(b"AAAA", "out.bin", 1, 2, 0, 8);
        let (seg2, _) = yenc_simd::encode_article(b"BBBB", "out.bin", 2, 2, 4, 8);
        decode_and_assemble(&decode_test_item("j1", 1), &seg1, &assembler, None).unwrap();
        decode_and_assemble(&decode_test_item("j1", 2), &seg2, &assembler, None).unwrap();

        assert_eq!(std::fs::read(&path).unwrap(), b"AAAABBBB");
        assert_eq!(assembler.get_file_progress("j1", "f1"), (2, 2));
    }

    #[test]
    fn decode_writes_segments_whose_yenc_sizes_differ() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("out.bin");
        let assembler = FileAssembler::new();
        // The NZB size bounds the write. The `=ybegin size=` on each segment
        // names the original file that segment was cut from, and obfuscated
        // reposts make those differ. Both segments are still written in full.
        assembler
            .register_file_with_segment_numbers("j1", "f1", path.clone(), &[1, 2], &[], Some(100))
            .unwrap();

        let (mut seg1, _) = yenc_simd::encode_article(b"AAAA", "out.bin", 1, 2, 0, 8);
        let (mut seg2, _) = yenc_simd::encode_article(b"BBBB", "out.bin", 2, 2, 4, 8);
        let rewrite_size = |raw: &mut Vec<u8>, from: &str, to: &str| {
            let header_end = raw.windows(2).position(|w| w == b"\r\n").unwrap();
            let header = String::from_utf8(raw[..header_end].to_vec()).unwrap();
            let header = header.replacen(from, to, 1);
            assert_ne!(
                header,
                String::from_utf8(raw[..header_end].to_vec()).unwrap()
            );
            raw.splice(0..header_end, header.bytes());
        };
        rewrite_size(&mut seg1, "size=8", "size=4000");
        rewrite_size(&mut seg2, "size=8", "size=9000");

        let pinned = Mutex::new(HashMap::new());
        decode_and_assemble(&decode_test_item("j1", 1), &seg1, &assembler, Some(&pinned)).unwrap();
        decode_and_assemble(&decode_test_item("j1", 2), &seg2, &assembler, Some(&pinned)).unwrap();

        assert_eq!(std::fs::read(&path).unwrap(), b"AAAABBBB");
        assert_eq!(assembler.get_file_progress("j1", "f1"), (2, 2));
        // The record keeps the range and does not grow a new entry per segment.
        assert_eq!(
            pinned.lock().get("f1").copied(),
            Some((4000, 4000, 9000, true))
        );
    }

    #[test]
    fn decode_bounds_undeclared_file_by_the_first_yenc_size() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("out.bin");
        let assembler = FileAssembler::new();
        // No NZB size: a segment omitted or garbled its `bytes` attribute.
        // The first `size=` seen is the write bound, so both segments of a
        // file whose headers agree are written.
        assembler
            .register_file("j1", "f1", path.clone(), 2)
            .unwrap();

        let (seg1, _) = yenc_simd::encode_article(b"AAAA", "out.bin", 1, 2, 0, 8);
        let (seg2, _) = yenc_simd::encode_article(b"BBBB", "out.bin", 2, 2, 4, 8);
        let pinned = Mutex::new(HashMap::new());
        decode_and_assemble(&decode_test_item("j1", 1), &seg1, &assembler, Some(&pinned)).unwrap();
        decode_and_assemble(&decode_test_item("j1", 2), &seg2, &assembler, Some(&pinned)).unwrap();

        assert_eq!(std::fs::read(&path).unwrap(), b"AAAABBBB");
        assert_eq!(assembler.get_file_progress("j1", "f1"), (2, 2));
        assert_eq!(pinned.lock().get("f1").copied(), Some((8, 8, 8, false)));
    }

    #[test]
    fn decode_rejects_undeclared_segment_past_the_first_yenc_size() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("out.bin");
        let assembler = FileAssembler::new();
        // No NZB size, and the segments disagree on `size=`. The first one
        // wins as the bound; a later segment whose own header places it past
        // that bound is rejected rather than growing the file to whatever the
        // header names.
        assembler
            .register_file("j1", "f1", path.clone(), 2)
            .unwrap();

        let (seg1, _) = yenc_simd::encode_article(b"AAAA", "out.bin", 1, 2, 0, 8);
        let (seg2, _) = yenc_simd::encode_article(b"BBBB", "out.bin", 2, 2, 8, 12);

        let pinned = Mutex::new(HashMap::new());
        decode_and_assemble(&decode_test_item("j1", 1), &seg1, &assembler, Some(&pinned)).unwrap();
        let result =
            decode_and_assemble(&decode_test_item("j1", 2), &seg2, &assembler, Some(&pinned));

        assert!(
            matches!(result, Err(ArticleError::DecodeError(_))),
            "a segment past the first size= must be rejected, got {result:?}"
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"AAAA");
        assert_eq!(assembler.get_file_progress("j1", "f1"), (1, 2));
        // The range is still recorded, and logged once, even though the write
        // was rejected.
        assert_eq!(pinned.lock().get("f1").copied(), Some((8, 8, 12, true)));
    }

    #[test]
    fn decode_keeps_the_first_yenc_size_as_the_bound_after_a_smaller_one() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("out.bin");
        let assembler = FileAssembler::new();
        // No NZB size. Segments arrive with size= 12, then 8, then 12. The
        // smaller size= of segment 2 widens the recorded range but must not
        // lower the bound, so segment 3 (bytes 8..12) still fits the first
        // size and is written, as it was before the range was recorded.
        assembler
            .register_file("j1", "f1", path.clone(), 3)
            .unwrap();

        let (seg1, _) = yenc_simd::encode_article(b"AAAA", "out.bin", 1, 3, 0, 12);
        let (seg2, _) = yenc_simd::encode_article(b"BBBB", "out.bin", 2, 3, 4, 8);
        let (seg3, _) = yenc_simd::encode_article(b"CCCC", "out.bin", 3, 3, 8, 12);

        let pinned = Mutex::new(HashMap::new());
        decode_and_assemble(&decode_test_item("j1", 1), &seg1, &assembler, Some(&pinned)).unwrap();
        decode_and_assemble(&decode_test_item("j1", 2), &seg2, &assembler, Some(&pinned)).unwrap();
        decode_and_assemble(&decode_test_item("j1", 3), &seg3, &assembler, Some(&pinned)).unwrap();

        assert_eq!(std::fs::read(&path).unwrap(), b"AAAABBBBCCCC");
        assert_eq!(assembler.get_file_progress("j1", "f1"), (3, 3));
        assert_eq!(pinned.lock().get("f1").copied(), Some((12, 8, 12, true)));
    }

    #[test]
    fn decode_rejects_ypart_range_overrunning_declared_file_size() {
        let temp = tempfile::tempdir().unwrap();
        let assembler = FileAssembler::new();
        assembler
            .register_file("j1", "f1", temp.path().join("out.bin"), 2)
            .unwrap();

        // Begins inside the file but runs 2 bytes past its declared end.
        let (raw, _) = yenc_simd::encode_article(b"BBBB", "out.bin", 2, 2, 6, 8);
        let result = decode_and_assemble(&decode_test_item("j1", 2), &raw, &assembler, None);

        assert!(matches!(result, Err(ArticleError::DecodeError(_))));
        assert_eq!(assembler.get_file_progress("j1", "f1"), (0, 2));
    }

    #[test]
    fn decode_rejects_missing_ypart_on_non_first_segment() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("out.bin");
        let assembler = FileAssembler::new();
        assembler
            .register_file("j1", "f1", path.clone(), 2)
            .unwrap();

        let (seg1, _) = yenc_simd::encode_article(b"AAAA", "out.bin", 1, 2, 0, 8);
        decode_and_assemble(&decode_test_item("j1", 1), &seg1, &assembler, None).unwrap();

        // Segment 2 arrives as a single-part article (no =ypart). Writing it
        // at offset 0 would overwrite segment 1.
        let (seg2, _) = yenc_simd::encode_article(b"ZZZZ", "out.bin", 1, 1, 0, 4);
        let result = decode_and_assemble(&decode_test_item("j1", 2), &seg2, &assembler, None);

        assert!(matches!(result, Err(ArticleError::DecodeError(_))));
        assert_eq!(std::fs::read(&path).unwrap(), b"AAAA");
        assert_eq!(assembler.get_file_progress("j1", "f1"), (1, 2));
    }

    #[test]
    fn decode_accepts_valid_parts_and_single_part_articles() {
        let temp = tempfile::tempdir().unwrap();
        let multi = temp.path().join("multi.bin");
        let single = temp.path().join("single.bin");
        let assembler = FileAssembler::new();
        assembler
            .register_file("j1", "f1", multi.clone(), 2)
            .unwrap();
        assembler
            .register_file("j1", "f2", single.clone(), 1)
            .unwrap();

        let (seg2, _) = yenc_simd::encode_article(b"BBBB", "multi.bin", 2, 2, 4, 8);
        let (seg1, _) = yenc_simd::encode_article(b"AAAA", "multi.bin", 1, 2, 0, 8);
        assert!(
            !decode_and_assemble(&decode_test_item("j1", 2), &seg2, &assembler, None)
                .unwrap()
                .file_complete
        );
        assert!(
            decode_and_assemble(&decode_test_item("j1", 1), &seg1, &assembler, None)
                .unwrap()
                .file_complete
        );
        assert_eq!(std::fs::read(&multi).unwrap(), b"AAAABBBB");

        let (whole, _) = yenc_simd::encode_article(b"ONE", "single.bin", 1, 1, 0, 3);
        let mut item = decode_test_item("j1", 1);
        item.file_id = "f2".into();
        assert!(
            decode_and_assemble(&item, &whole, &assembler, None)
                .unwrap()
                .file_complete
        );
        assert_eq!(std::fs::read(&single).unwrap(), b"ONE");
    }

    #[tokio::test]
    async fn serial_fetch_does_not_write_article_after_job_cancelled() {
        use nzb_nntp::testutil::{MockConfig, MockNntpServer, test_config};

        let (encoded, _) = yenc_simd::encode_article(b"payload", "out.bin", 1, 1, 0, 7);
        let mut articles = HashMap::new();
        articles.insert("msg@test".to_string(), encoded);
        let server = MockNntpServer::start(MockConfig {
            articles,
            ..MockConfig::default()
        })
        .await;
        let config = test_config(server.port());
        let mut conn = NntpConnection::new(config.id.clone());
        conn.connect(&config).await.unwrap();

        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("out.bin");
        let assembler = Arc::new(FileAssembler::new());
        assembler
            .register_file("j1", "f1", path.clone(), 1)
            .unwrap();
        let item = make_item("j1", "msg@test", "out.bin");

        // abort_job() flips the flag while the fetch is in flight.
        let cancelled = AtomicBool::new(true);
        let result = fetch_article_with_retry(
            &mut conn, &item, &assembler, &cancelled, None, &config, "w0",
        )
        .await;

        assert!(
            matches!(result, Err(ArticleError::Cancelled)),
            "expected Cancelled, got {result:?}"
        );
        assert_eq!(assembler.get_file_progress("j1", "f1"), (0, 1));
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 0);

        // Sanity: the same fetch succeeds and writes when not cancelled.
        let not_cancelled = AtomicBool::new(false);
        let result = fetch_article_with_retry(
            &mut conn,
            &item,
            &assembler,
            &not_cancelled,
            None,
            &config,
            "w0",
        )
        .await
        .unwrap();
        assert!(result.file_complete);
        assert_eq!(std::fs::read(&path).unwrap(), b"payload");
    }

    #[tokio::test]
    async fn serial_fetch_bounds_undeclared_file_by_the_first_yenc_size() {
        use nzb_nntp::testutil::{MockConfig, MockNntpServer, test_config};

        // Segment 2 claims an offset past the first segment's `size=`. The
        // pipelined path rejects it; the serial path must too.
        let (seg1, _) = yenc_simd::encode_article(b"AAAA", "out.bin", 1, 2, 0, 8);
        let (seg2, _) = yenc_simd::encode_article(b"BBBB", "out.bin", 2, 2, 8, 12);
        let mut articles = HashMap::new();
        articles.insert("seg1@test".to_string(), seg1);
        articles.insert("seg2@test".to_string(), seg2);
        let server = MockNntpServer::start(MockConfig {
            articles,
            ..MockConfig::default()
        })
        .await;
        let config = test_config(server.port());
        let mut conn = NntpConnection::new(config.id.clone());
        conn.connect(&config).await.unwrap();

        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("out.bin");
        let assembler = Arc::new(FileAssembler::new());
        assembler
            .register_file("j1", "f1", path.clone(), 2)
            .unwrap();
        let pinned = Arc::new(Mutex::new(HashMap::new()));
        let not_cancelled = AtomicBool::new(false);

        let item1 = make_item("j1", "seg1@test", "out.bin");
        fetch_article_with_retry(
            &mut conn,
            &item1,
            &assembler,
            &not_cancelled,
            Some(&pinned),
            &config,
            "w0",
        )
        .await
        .unwrap();

        let item2 = make_item("j1", "seg2@test", "out.bin");
        let result = fetch_article_with_retry(
            &mut conn,
            &item2,
            &assembler,
            &not_cancelled,
            Some(&pinned),
            &config,
            "w0",
        )
        .await;

        assert!(
            matches!(result, Err(ArticleError::DecodeError(_))),
            "a serial segment past the first size= must be rejected, got {result:?}"
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"AAAA");
        assert_eq!(assembler.get_file_progress("j1", "f1"), (1, 2));
    }

    #[cfg(target_os = "linux")]
    fn open_fd_count_under(root: &std::path::Path) -> usize {
        std::fs::read_dir("/proc/self/fd")
            .expect("read /proc/self/fd")
            .filter_map(Result::ok)
            .filter_map(|entry| std::fs::read_link(entry.path()).ok())
            .filter(|target| target.starts_with(root))
            .count()
    }

    #[test]
    fn has_known_extension_recognizes_archives() {
        assert!(has_known_extension("movie.rar"));
        assert!(has_known_extension("movie.part01.rar"));
        assert!(has_known_extension("file.zip"));
        assert!(has_known_extension("file.7z"));
        assert!(has_known_extension("archive.001"));
    }

    #[test]
    fn has_known_extension_recognizes_video() {
        assert!(has_known_extension("episode.mkv"));
        assert!(has_known_extension("movie.mp4"));
        assert!(has_known_extension("video.avi"));
        assert!(has_known_extension("clip.ts"));
    }

    #[test]
    fn has_known_extension_recognizes_par2() {
        assert!(has_known_extension("file.par2"));
        assert!(has_known_extension("file.vol00+01.par2"));
        assert!(has_known_extension("file.vol015-031.par2"));
    }

    #[test]
    fn has_known_extension_recognizes_misc() {
        assert!(has_known_extension("info.nfo"));
        assert!(has_known_extension("sub.srt"));
        assert!(has_known_extension("cover.jpg"));
        assert!(has_known_extension("song.flac"));
    }

    #[test]
    fn has_known_extension_rejects_obfuscated_hashes() {
        assert!(!has_known_extension("9b6a324d7560b87091685020371ba869"));
        assert!(!has_known_extension("1fG1GP7L2263LHXH213HTNIxZsX7l0cv44BZ"));
        assert!(!has_known_extension("DfKUx3bl7L6PSo6276WSaXSZ7"));
        assert!(!has_known_extension("Q77O1ZxL237vc241z77hFoLBxl"));
    }

    #[test]
    fn has_known_extension_rejects_unknown_extensions() {
        assert!(!has_known_extension("file.xyz123"));
        assert!(!has_known_extension("noext"));
        assert!(!has_known_extension(""));
    }

    #[test]
    fn has_known_extension_case_insensitive() {
        assert!(has_known_extension("file.RAR"));
        assert!(has_known_extension("file.MKV"));
        assert!(has_known_extension("file.Par2"));
        assert!(has_known_extension("file.MP4"));
    }

    fn make_item(job_id: &str, msg_id: &str, filename: &str) -> WorkItem {
        WorkItem {
            job_id: job_id.to_string(),
            file_id: "f1".to_string(),
            filename: filename.to_string(),
            message_id: msg_id.to_string(),
            segment_number: 1,
            tried_servers: Vec::new(),
            provider_outcomes: HashMap::new(),
            tries_on_current: 0,
            outage_tries: 0,
            confirm_rounds: 0,
        }
    }

    #[test]
    fn shared_queue_par2_first() {
        let q = SharedWorkQueue::new();
        q.submit_items(vec![
            make_item("j1", "a", "movie.rar"),
            make_item("j1", "b", "movie.par2"),
            make_item("j1", "c", "movie.vol00+01.par2"),
            make_item("j1", "d", "movie.r00"),
        ]);
        let first = q.pop_workable("srv1", &[]).unwrap();
        assert_eq!(first.filename, "movie.par2", "index file first");
        let second = q.pop_workable("srv1", &[]).unwrap();
        assert_eq!(second.filename, "movie.vol00+01.par2", "vol file second");
    }

    #[test]
    fn shared_queue_skips_tried_servers() {
        let q = SharedWorkQueue::new();
        let mut item = make_item("j1", "a", "file.rar");
        item.tried_servers.push("srv1".to_string());
        q.submit_items(vec![item, make_item("j1", "b", "other.rar")]);

        // srv1 should skip the first item and return the second.
        let picked = q.pop_workable("srv1", &[]).unwrap();
        assert_eq!(picked.message_id, "b");
    }

    #[test]
    fn shared_queue_round_robins_eligible_jobs_per_server() {
        let q = SharedWorkQueue::new();
        q.submit_items(vec![
            make_item("job-a", "a-1", "first.rar"),
            make_item("job-a", "a-2", "second.rar"),
            make_item("job-b", "b-1", "third.rar"),
        ]);

        assert_eq!(q.pop_workable("srv1", &[]).unwrap().job_id, "job-a");
        assert_eq!(q.pop_workable("srv1", &[]).unwrap().job_id, "job-b");
        assert_eq!(q.pop_workable("srv1", &[]).unwrap().job_id, "job-a");
    }

    #[tokio::test]
    async fn provider_outage_is_reported_once_then_cleared_on_recovery() {
        let temp = tempfile::tempdir().unwrap();
        let server = ServerConfig::new("srv1", "one.invalid");
        let pool = WorkerPool::new(
            Arc::new(Mutex::new(vec![server.clone()])),
            Arc::new(BandwidthLimiter::new(Default::default())),
            Arc::new(ConnectionTracker::new()),
            0,
        );
        let job = test_job("provider-outage", temp.path());
        let (progress_tx, mut progress_rx) = mpsc::channel(4);
        let ctx = Arc::new(JobContext::new(
            &job,
            Arc::new(FileAssembler::new()),
            progress_tx,
            1,
        ));
        pool.job_contexts.lock().insert(job.id.clone(), ctx);
        pool.server_health
            .lock()
            .entry(server.id.clone())
            .or_default()
            .record_failure(false, "connection refused");
        pool.server_health
            .lock()
            .entry(server.id.clone())
            .or_default()
            .record_failure(false, "connection refused");
        pool.server_health
            .lock()
            .entry(server.id.clone())
            .or_default()
            .record_failure(false, "connection refused");

        pool.report_provider_outage();
        pool.report_provider_outage();
        match progress_rx.recv().await.unwrap() {
            ProgressUpdate::WaitingForProviders { message, .. } => {
                assert!(message.contains("retry automatically"));
            }
            other => panic!("unexpected progress event: {other:?}"),
        }
        assert!(
            progress_rx.try_recv().is_err(),
            "outage event is deduplicated"
        );

        pool.server_health
            .lock()
            .get_mut(&server.id)
            .unwrap()
            .record_success();
        pool.report_providers_available();
        assert!(matches!(
            progress_rx.recv().await,
            Some(ProgressUpdate::ProvidersAvailable { .. })
        ));
    }

    #[test]
    fn unavailable_provider_does_not_prove_global_absence() {
        let servers = vec![
            ServerConfig::new("srv1", "one.invalid"),
            ServerConfig::new("srv2", "two.invalid"),
        ];
        let mut outcomes = HashMap::new();
        outcomes.insert(
            "srv1".to_string(),
            crate::article_failure::ArticleFailureKind::NotFound,
        );

        let health = HashMap::new();
        assert!(!all_enabled_providers_definitive(
            &servers, &health, &outcomes
        ));
        outcomes.insert(
            "srv2".to_string(),
            crate::article_failure::ArticleFailureKind::NotFound,
        );
        assert!(all_enabled_providers_definitive(
            &servers, &health, &outcomes
        ));
    }

    #[test]
    fn unavailable_optional_provider_is_not_required() {
        let mut optional = ServerConfig::new("fill", "fill.invalid");
        optional.optional = true;
        let servers = vec![ServerConfig::new("main", "main.invalid"), optional];
        let mut outcomes = HashMap::new();
        outcomes.insert(
            "main".to_string(),
            crate::article_failure::ArticleFailureKind::NotFound,
        );
        let mut health = HashMap::new();

        // A healthy optional server is still asked.
        assert!(!all_enabled_providers_definitive(
            &servers, &health, &outcomes
        ));

        let mut down = ServerHealth::new();
        down.record_failure(true, "502 on connect");
        health.insert("fill".to_string(), down);
        assert!(all_enabled_providers_definitive(
            &servers, &health, &outcomes
        ));

        // A required server that is down still blocks the decision.
        let mut main_down = ServerHealth::new();
        main_down.record_failure(true, "502 on connect");
        health.insert("main".to_string(), main_down);
        outcomes.clear();
        assert!(!all_enabled_providers_definitive(
            &servers, &health, &outcomes
        ));
    }

    #[tokio::test]
    async fn supervisor_sweep_fails_items_left_waiting_on_down_optional_server() {
        let temp = tempfile::tempdir().unwrap();
        let pool = worker_pool_without_servers();
        let mut optional = ServerConfig::new("fill", "fill.invalid");
        optional.optional = true;
        *pool.servers.lock() = vec![ServerConfig::new("main", "main.invalid"), optional];

        let job = test_job("sweep", temp.path());
        let (progress_tx, mut progress_rx) = mpsc::channel(8);
        let ctx = Arc::new(JobContext::new(
            &job,
            Arc::new(FileAssembler::new()),
            progress_tx,
            1,
        ));
        pool.job_contexts.lock().insert(job.id.clone(), ctx.clone());
        let mut item = make_item("sweep", "missing@test", "payload.bin");
        item.tried_servers.push("main".into());
        item.provider_outcomes.insert(
            "main".into(),
            crate::article_failure::ArticleFailureKind::NotFound,
        );
        pool.work_queue.push_front(item);

        // Optional server still healthy: the item keeps waiting for it.
        pool.resolve_items_not_waiting_on_required_providers();
        assert_eq!(pool.work_queue.len(), 1);

        // It goes down after the main server answered.
        let mut down = ServerHealth::new();
        down.record_failure(true, "502 on connect");
        pool.server_health.lock().insert("fill".into(), down);
        pool.resolve_items_not_waiting_on_required_providers();

        assert_eq!(pool.work_queue.len(), 0);
        assert_eq!(ctx.articles_failed.load(Ordering::Relaxed), 1);
        assert_eq!(ctx.articles_remaining.load(Ordering::Relaxed), 0);
        let mut saw_failure = false;
        while let Ok(update) = progress_rx.try_recv() {
            saw_failure |= matches!(update, ProgressUpdate::ArticleFailed { .. });
        }
        assert!(saw_failure);
    }

    #[tokio::test]
    async fn abort_has_one_owner_and_waits_for_in_flight_resolution() {
        let temp = tempfile::tempdir().unwrap();
        let pool = worker_pool_without_servers();
        let job = test_job("abort-drain", temp.path());
        let assembler = Arc::new(FileAssembler::new());
        let (progress_tx, mut progress_rx) = mpsc::channel(8);
        let ctx = Arc::new(JobContext::new(&job, assembler, progress_tx, 3));
        ctx.articles_failed.store(2, Ordering::Relaxed);
        pool.job_contexts
            .lock()
            .insert(job.id.clone(), Arc::clone(&ctx));
        pool.work_queue.submit_items(vec![
            make_item(&job.id, "queued-1", "file.rar"),
            make_item(&job.id, "queued-2", "file.rar"),
        ]);

        assert!(pool.abort_job(&job.id, "original reason".into()));
        assert!(!pool.abort_job(&job.id, "overwritten reason".into()));
        assert_eq!(ctx.articles_remaining.load(Ordering::Relaxed), 1);
        assert!(!ctx.finished.load(Ordering::Relaxed));
        assert!(progress_rx.try_recv().is_err());

        // Simulate the sole in-flight worker settling after its write handle
        // is no longer usable. Only now may the terminal event be published.
        ctx.resolve_one();
        match progress_rx.recv().await.unwrap() {
            ProgressUpdate::JobAborted {
                reason,
                articles_failed,
                download_time_secs,
                ..
            } => {
                assert_eq!(reason, "original reason");
                assert_eq!(articles_failed, 2);
                assert!(download_time_secs >= 0.0);
            }
            other => panic!("unexpected terminal event: {other:?}"),
        }
    }

    #[test]
    fn pop_workable_respects_priority() {
        // Fresh item (tried_servers empty). A backup-priority caller whose
        // higher_priority_servers list is non-empty must NOT get the item —
        // the primary server still needs a chance first.
        let q = SharedWorkQueue::new();
        q.submit_items(vec![make_item("j1", "a", "file.rar")]);

        let higher = vec!["srv_primary".to_string()];
        assert!(q.pop_workable("srv_backup", &higher).is_none());

        // Primary server (empty higher list) should still get it.
        let item = q.pop_workable("srv_primary", &[]).unwrap();
        assert_eq!(item.message_id, "a");
    }

    #[test]
    fn pop_workable_allows_backup_after_primary_tried() {
        // Once the primary has been added to tried_servers (because it
        // failed), the backup is allowed to take the item.
        let q = SharedWorkQueue::new();
        let mut item = make_item("j1", "a", "file.rar");
        item.tried_servers.push("srv_primary".to_string());
        q.submit_items(vec![item]);

        let higher = vec!["srv_primary".to_string()];
        let picked = q.pop_workable("srv_backup", &higher).unwrap();
        assert_eq!(picked.message_id, "a");
    }

    #[test]
    fn pop_workable_ignores_circuit_broken_higher_server() {
        // When the caller's `higher_priority_servers` list is empty because
        // the primary was filtered out as circuit-broken/disabled, the backup
        // gets items immediately — no waiting for the dead primary.
        let q = SharedWorkQueue::new();
        q.submit_items(vec![make_item("j1", "a", "file.rar")]);

        let higher: Vec<String> = vec![]; // primary filtered out by caller
        let item = q.pop_workable("srv_backup", &higher).unwrap();
        assert_eq!(item.message_id, "a");
    }

    #[test]
    fn workable_count_for_respects_priority() {
        let q = SharedWorkQueue::new();
        q.submit_items(vec![
            make_item("j1", "a", "a.rar"),
            make_item("j1", "b", "b.rar"),
        ]);

        // Backup: both items need primary first → workable=0.
        let higher = vec!["srv_primary".to_string()];
        let (workable, total) = q.workable_count_for("srv_backup", &higher);
        assert_eq!(workable, 0);
        assert_eq!(total, 2);

        // Primary: both items are workable.
        let (workable, total) = q.workable_count_for("srv_primary", &[]);
        assert_eq!(workable, 2);
        assert_eq!(total, 2);
    }

    #[test]
    fn shared_queue_drain_job_removes_only_target() {
        let q = SharedWorkQueue::new();
        q.submit_items(vec![
            make_item("j1", "a", "a.rar"),
            make_item("j2", "b", "b.rar"),
            make_item("j1", "c", "c.rar"),
        ]);
        let drained = q.drain_job("j1");
        assert_eq!(drained.len(), 2);
        assert_eq!(q.len(), 1);
        let remaining = q.pop_workable("srv1", &[]).unwrap();
        assert_eq!(remaining.job_id, "j2");
    }

    #[test]
    fn release_completed_job_drops_context_and_closes_assembler_files() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let assembler = Arc::new(FileAssembler::new());
        let job_id = "completed-job";
        assembler
            .register_file(job_id, "file-1", tempdir.path().join("file.rar"), 1)
            .expect("register file");
        assert_eq!(assembler.get_file_progress(job_id, "file-1"), (0, 1));

        let job = test_job(job_id, tempdir.path());
        let pool = worker_pool_without_servers();
        insert_test_context(&pool, &job, Arc::clone(&assembler));
        assert!(pool.has_job(job_id));

        pool.release_completed_job(job_id);

        assert!(!pool.has_job(job_id));
        assert_eq!(assembler.get_file_progress(job_id, "file-1"), (0, 0));
    }

    #[test]
    fn job_active_secs_reports_registered_jobs_only() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let job = test_job("active-secs", tempdir.path());
        let pool = worker_pool_without_servers();
        assert_eq!(pool.job_active_secs(&job.id), None);

        insert_test_context(&pool, &job, Arc::new(FileAssembler::new()));
        pool.pause_job(&job.id);
        let paused = pool.job_active_secs(&job.id).expect("registered job");
        assert!(paused >= 0.0);
        // A paused job's clock is stopped.
        assert_eq!(pool.job_active_secs(&job.id), Some(paused));

        pool.release_completed_job(&job.id);
        assert_eq!(pool.job_active_secs(&job.id), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn repeated_completed_jobs_do_not_accumulate_file_descriptors() {
        const JOBS: usize = 64;
        const FILES_PER_JOB: usize = 8;

        let tempdir = tempfile::tempdir().expect("tempdir");
        let pool = worker_pool_without_servers();
        assert_eq!(open_fd_count_under(tempdir.path()), 0);

        for job_index in 0..JOBS {
            let job_id = format!("job-{job_index}");
            let job_dir = tempdir.path().join(&job_id);
            let assembler = Arc::new(FileAssembler::new());
            for file_index in 0..FILES_PER_JOB {
                assembler
                    .register_file(
                        &job_id,
                        &format!("file-{file_index}"),
                        job_dir.join(format!("file-{file_index}.rar")),
                        1,
                    )
                    .expect("register file");
            }
            assert_eq!(open_fd_count_under(tempdir.path()), FILES_PER_JOB);

            let job = test_job(&job_id, &job_dir);
            insert_test_context(&pool, &job, assembler);
            pool.release_completed_job(&job_id);

            assert!(!pool.has_job(&job_id));
            assert_eq!(
                open_fd_count_under(tempdir.path()),
                0,
                "completed job {job_index} retained output file descriptors"
            );
        }

        assert!(pool.job_contexts.lock().is_empty());
    }

    // -----------------------------------------------------------------------
    // ConnectionTracker (budget-backed slots)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn connection_tracker_acquire_releases_slot_on_drop() {
        let t = ConnectionTracker::new();
        t.set_limit("srv1", "Server 1", 2);
        let s1 = t.acquire("srv1").await.unwrap();
        let s2 = t.acquire("srv1").await.unwrap();
        assert_eq!(t.total(), 2);
        // Synchronous release on drop.
        drop(s1);
        assert_eq!(t.total(), 1);
        drop(s2);
        assert_eq!(t.total(), 0);
    }

    #[tokio::test]
    async fn connection_tracker_blocks_at_limit() {
        let t = Arc::new(ConnectionTracker::new());
        t.set_limit("srv1", "Server 1", 1);
        let _held = t.acquire("srv1").await.unwrap();

        // Third acquire on a 1-slot pool must block. Wrap with a short
        // timeout — if it does NOT time out, the cap was breached.
        let t2 = Arc::clone(&t);
        let res = tokio::time::timeout(Duration::from_millis(150), async move {
            t2.acquire("srv1").await
        })
        .await;
        assert!(
            res.is_err(),
            "second acquire should block while limit is reached"
        );
    }

    #[tokio::test]
    async fn connection_tracker_grow_in_place_lets_more_acquire() {
        let t = ConnectionTracker::new();
        t.set_limit("srv1", "Server 1", 2);
        let _a = t.acquire("srv1").await.unwrap();
        let _b = t.acquire("srv1").await.unwrap();
        // No more capacity — but grow to 4 and we should be able to take 2
        // more without releasing the existing slots.
        t.set_limit("srv1", "Server 1", 4);
        let _c = t.acquire("srv1").await.unwrap();
        let _d = t.acquire("srv1").await.unwrap();
        assert_eq!(t.total(), 4);
    }

    #[tokio::test]
    async fn connection_tracker_shrink_marks_old_slots_stale() {
        let t = ConnectionTracker::new();
        t.set_limit("srv1", "Server 1", 4);
        let s = t.acquire("srv1").await.unwrap();
        assert!(t.slot_is_current(&s));

        // Shrink to 1 — the slot is replaced.
        t.set_limit("srv1", "Server 1", 1);
        assert!(
            !t.slot_is_current(&s),
            "after shrink, the previously-acquired slot must be marked stale"
        );

        // The new generation starts empty. The old holder is no longer
        // counted in `total()`, but it still holds the server's only
        // connection, so the new generation waits for it to wind down.
        assert_eq!(t.total(), 0);
        assert!(
            tokio::time::timeout(Duration::from_millis(100), t.acquire("srv1"))
                .await
                .is_err(),
            "the shrunk budget must count the stale slot until it drops"
        );
        drop(s);

        let new_slot = t.acquire("srv1").await.unwrap();
        assert!(t.slot_is_current(&new_slot));
        assert_eq!(t.total(), 1);
    }

    #[tokio::test]
    async fn connection_tracker_remove_server_marks_slot_stale() {
        let t = ConnectionTracker::new();
        t.set_limit("srv1", "Server 1", 2);
        let s = t.acquire("srv1").await.unwrap();
        assert!(t.slot_is_current(&s));

        t.remove_server("srv1");
        assert!(
            !t.slot_is_current(&s),
            "after remove_server, the slot must be marked stale"
        );
        assert_eq!(t.total(), 0);
    }

    #[tokio::test]
    async fn connection_tracker_snapshot_reflects_active_count() {
        let t = ConnectionTracker::new();
        t.set_limit("srv1", "Server 1", 3);
        t.set_limit("srv2", "Server 2", 5);

        let _a1 = t.acquire("srv1").await.unwrap();
        let _a2 = t.acquire("srv1").await.unwrap();
        let _b1 = t.acquire("srv2").await.unwrap();

        let mut snap = t.snapshot();
        snap.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(snap.len(), 2);
        assert_eq!(snap[0], ("srv1".into(), 2, 3));
        assert_eq!(snap[1], ("srv2".into(), 1, 5));
    }

    #[tokio::test]
    async fn active_snapshot_counts_only_article_activity() {
        let t = ConnectionTracker::new();
        t.set_limit("srv1", "Server 1", 3);

        let mut connected = t.acquire("srv1").await.unwrap();
        let _disconnected = t.acquire("srv1").await.unwrap();
        assert_eq!(t.connected_snapshot(), vec![("srv1".into(), 0, 3)]);

        connected.mark_active();
        assert_eq!(t.connected_snapshot(), vec![("srv1".into(), 1, 3)]);

        connected.mark_inactive();
        assert_eq!(t.connected_snapshot(), vec![("srv1".into(), 0, 3)]);
    }

    #[tokio::test]
    async fn activity_guard_clears_count_when_fetch_scope_ends() {
        let t = ConnectionTracker::new();
        t.set_limit("srv1", "Server 1", 1);
        let mut slot = t.acquire("srv1").await.unwrap();

        {
            let _activity = slot.activity();
            assert_eq!(t.connected_snapshot(), vec![("srv1".into(), 1, 1)]);
        }

        assert_eq!(t.connected_snapshot(), vec![("srv1".into(), 0, 1)]);
        assert_eq!(t.snapshot(), vec![("srv1".into(), 1, 1)]);
    }

    #[tokio::test]
    async fn open_snapshot_counts_open_sockets_not_held_slots() {
        let t = ConnectionTracker::new();
        t.set_limit("srv1", "Server 1", 3);
        let mut a = t.acquire("srv1").await.unwrap();
        let _idle_without_socket = t.acquire("srv1").await.unwrap();
        assert_eq!(t.open_snapshot(), vec![("srv1".into(), 0, 3)]);

        a.mark_open();
        a.mark_open();
        assert_eq!(t.open_snapshot(), vec![("srv1".into(), 1, 3)]);
        a.mark_closed();
        assert_eq!(t.open_snapshot(), vec![("srv1".into(), 0, 3)]);
        assert_eq!(t.snapshot(), vec![("srv1".into(), 2, 3)]);

        a.mark_open();
        drop(a);
        assert_eq!(t.open_snapshot(), vec![("srv1".into(), 0, 3)]);
    }

    fn dav_budgets(playback_reserve_pct: u8) -> Arc<BudgetRegistry> {
        Arc::new(BudgetRegistry::new(nzb_nntp::budget::BudgetPolicy {
            playback_reserve_pct,
            repair_reserve_pct: 0,
        }))
    }

    #[tokio::test]
    async fn download_and_dav_together_never_exceed_connections() {
        use nzb_nntp::ConnectionPool;
        use nzb_nntp::testutil::{MockConfig, MockNntpServer, test_config};

        let mock = MockNntpServer::start(MockConfig::default()).await;
        let config = test_config(mock.port()); // connections = 4
        let budgets = dav_budgets(25);
        let tracker = Arc::new(ConnectionTracker::with_budgets(Arc::clone(&budgets)));
        tracker.set_limit(&config.id, &config.name, config.connections as usize);
        let budget = budgets.get(&config.id).unwrap();
        let dav = Arc::new(ConnectionPool::with_budget(
            Arc::new(config.clone()),
            Arc::clone(&budget),
            BudgetTier::Playback,
        ));

        let peak = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let monitor = tokio::spawn({
            let (budget, peak, stop) = (Arc::clone(&budget), Arc::clone(&peak), Arc::clone(&stop));
            async move {
                while !stop.load(Ordering::Relaxed) {
                    peak.fetch_max(budget.held_total(), Ordering::Relaxed);
                    tokio::task::yield_now().await;
                }
            }
        });
        let downloads_done = Arc::new(AtomicUsize::new(0));
        let reads_done = Arc::new(AtomicUsize::new(0));
        let mut tasks = Vec::new();
        for _ in 0..6 {
            let (tracker, id, done) = (
                Arc::clone(&tracker),
                config.id.clone(),
                Arc::clone(&downloads_done),
            );
            tasks.push(tokio::spawn(async move {
                for _ in 0..20 {
                    let _slot = tracker.acquire(&id).await.unwrap();
                    tokio::task::yield_now().await;
                    done.fetch_add(1, Ordering::Relaxed);
                }
            }));
            let (dav, done) = (Arc::clone(&dav), Arc::clone(&reads_done));
            tasks.push(tokio::spawn(async move {
                for _ in 0..20 {
                    let conn = dav.acquire().await.unwrap();
                    tokio::task::yield_now().await;
                    dav.release(conn);
                    done.fetch_add(1, Ordering::Relaxed);
                }
            }));
        }
        for task in tasks {
            tokio::time::timeout(Duration::from_secs(20), task)
                .await
                .expect("a consumer starved")
                .unwrap();
        }
        stop.store(true, Ordering::Relaxed);
        monitor.await.unwrap();

        assert_eq!(downloads_done.load(Ordering::Relaxed), 120);
        assert_eq!(reads_done.load(Ordering::Relaxed), 120);
        assert!(peak.load(Ordering::Relaxed) <= 4, "budget exceeded");
        // The only permits left are the DAV pool's idle sockets.
        assert_eq!(budget.held_total(), dav.idle_count());
        assert!(budget.held(BudgetTier::Download) == 0);
    }

    #[tokio::test]
    async fn dav_playback_gets_a_connection_while_downloads_saturate() {
        use nzb_nntp::ConnectionPool;
        use nzb_nntp::testutil::{MockConfig, MockNntpServer, test_config};

        let mock = MockNntpServer::start(MockConfig::default()).await;
        let config = test_config(mock.port()); // connections = 4
        let budgets = dav_budgets(25);
        let tracker = Arc::new(ConnectionTracker::with_budgets(Arc::clone(&budgets)));
        tracker.set_limit(&config.id, &config.name, config.connections as usize);

        // The downloader takes everything it may: 3 of 4, one kept for
        // playback. A fourth worker queues.
        let mut slots = Vec::new();
        for _ in 0..3 {
            slots.push(tracker.acquire(&config.id).await.unwrap());
        }
        let queued = tokio::spawn({
            let (tracker, id) = (Arc::clone(&tracker), config.id.clone());
            async move { tracker.acquire(&id).await }
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!queued.is_finished(), "download took the playback reserve");

        let budget = budgets.get(&config.id).unwrap();
        let dav = ConnectionPool::with_budget(
            Arc::new(config),
            Arc::clone(&budget),
            BudgetTier::Playback,
        );
        let conn = tokio::time::timeout(Duration::from_secs(5), dav.acquire())
            .await
            .expect("playback starved behind the downloader")
            .unwrap();
        assert_eq!(budget.held_total(), 4);

        // Once playback is done and its socket closed, the queued download
        // gets the permit.
        dav.discard(conn);
        drop(slots.pop());
        let _slot = tokio::time::timeout(Duration::from_secs(5), queued)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(tracker.total(), 3);
    }

    #[test]
    fn only_provider_level_errors_count_against_circuit_breaker() {
        let s = || "x".to_string();
        for e in [
            NntpError::Connection(s()),
            NntpError::Tls(s()),
            NntpError::Auth(s()),
            NntpError::AuthRequired(s()),
            NntpError::PermissionDenied(s()),
            NntpError::ServiceUnavailable(s()),
            NntpError::Timeout(s()),
        ] {
            assert!(counts_against_circuit_breaker(&e), "{e}");
        }
        for e in [
            NntpError::ArticleNotFound(s()),
            NntpError::NoSuchGroup(s()),
            NntpError::NoArticleSelected(s()),
            NntpError::Protocol(s()),
        ] {
            assert!(!counts_against_circuit_breaker(&e), "{e}");
        }
    }

    fn pool_with(servers: Vec<ServerConfig>) -> Arc<WorkerPool> {
        let tracker = Arc::new(ConnectionTracker::new());
        for server in &servers {
            tracker.set_limit(&server.id, &server.name, server.connections as usize);
        }
        WorkerPool::new(
            Arc::new(Mutex::new(servers)),
            Arc::new(BandwidthLimiter::new(Default::default())),
            tracker,
            0,
        )
    }

    fn server(id: &str, priority: u8) -> ServerConfig {
        let mut server = ServerConfig::new(id, format!("{id}.example"));
        server.name = id.to_string();
        server.priority = priority;
        server.connections = 1;
        server.port = 563;
        server
    }

    fn spawned_endpoints(pool: &WorkerPool) -> HashMap<String, Vec<(String, u16)>> {
        pool.workers
            .lock()
            .iter()
            .map(|(id, list)| {
                (
                    id.clone(),
                    list.iter()
                        .map(|w| (w.spawned_with.host.clone(), w.spawned_with.port))
                        .collect(),
                )
            })
            .collect()
    }

    #[tokio::test]
    async fn raising_server_priority_keeps_downloads_flowing() {
        // Real workers against two mock servers that both carry the article.
        // A starts worse than B; the edit raises A above B. If A's workers
        // keep their spawn-time priority they still treat B as higher and
        // wait for B to try the item, while B's workers read A's new
        // priority from the live list and wait for A. Nothing pops, nothing
        // fails, and the download stalls.
        use nzb_nntp::testutil::{MockConfig, MockNntpServer, test_config};

        let (encoded, _) = yenc_simd::encode_article(b"payload", "out.bin", 1, 1, 0, 7);
        let mut articles = HashMap::new();
        articles.insert("msg@test".to_string(), encoded);
        let mock_a = MockNntpServer::start(MockConfig {
            articles: articles.clone(),
            ..MockConfig::default()
        })
        .await;
        let mock_b = MockNntpServer::start(MockConfig {
            articles,
            ..MockConfig::default()
        })
        .await;
        let mock_server = |id: &str, port: u16, priority: u8| {
            let mut s = test_config(port);
            s.id = id.to_string();
            s.name = id.to_string();
            s.connections = 1;
            s.priority = priority;
            s
        };
        let a = mock_server("a", mock_a.port(), 2);
        let b = mock_server("b", mock_b.port(), 1);

        let pool = pool_with(vec![a.clone(), b.clone()]);
        pool.reconcile_servers();

        let mut raised = a.clone();
        raised.priority = 0;
        *pool.servers.lock() = vec![raised, b];
        pool.reconcile_servers();

        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("out.bin");
        let assembler = Arc::new(FileAssembler::new());
        assembler
            .register_file("j1", "f1", path.clone(), 1)
            .unwrap();
        insert_test_context(&pool, &test_job("j1", temp.path()), Arc::clone(&assembler));
        pool.work_queue
            .submit_items(vec![make_item("j1", "msg@test", "out.bin")]);

        let deadline = Instant::now() + Duration::from_secs(10);
        while assembler.get_file_progress("j1", "f1") != (1, 1) {
            assert!(
                Instant::now() < deadline,
                "download stalled after raising A's priority: queue_len={}",
                pool.work_queue.len()
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert_eq!(std::fs::read(&path).unwrap(), b"payload");
        pool.shutdown().await;
    }

    #[tokio::test]
    async fn host_or_port_edit_recycles_workers_onto_new_config() {
        let a = server("a", 0);
        let pool = pool_with(vec![a.clone()]);
        pool.reconcile_servers();
        assert_eq!(
            spawned_endpoints(&pool)["a"],
            vec![("a.example".to_string(), 563)]
        );

        let mut moved = a.clone();
        moved.host = "news.example".to_string();
        moved.port = 119;
        *pool.servers.lock() = vec![moved];
        pool.reconcile_servers();

        assert_eq!(
            spawned_endpoints(&pool)["a"],
            vec![("news.example".to_string(), 119)],
            "recycled workers must connect with the edited host and port"
        );
    }

    #[tokio::test]
    async fn identical_config_does_not_recycle_workers() {
        let a = server("a", 1);
        let pool = pool_with(vec![a.clone()]);
        pool.reconcile_servers();

        let before: Vec<_> = pool
            .workers
            .lock()
            .get("a")
            .unwrap()
            .iter()
            .map(|w| w.handle.id())
            .collect();

        *pool.servers.lock() = vec![a];
        pool.reconcile_servers();

        let after: Vec<_> = pool
            .workers
            .lock()
            .get("a")
            .unwrap()
            .iter()
            .map(|w| w.handle.id())
            .collect();
        assert_eq!(before, after, "a no-op update must keep the same workers");
    }
}
