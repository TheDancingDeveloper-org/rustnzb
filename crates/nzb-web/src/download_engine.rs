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
//! - A supervisor task detects "all enabled servers circuit-broken for a
//!   given job" and emits [`ProgressUpdate::NoServersAvailable`] so the user
//!   can fix config and resume, matching the prior per-engine behaviour.
//!
//! Retry logic (per article):
//! 1. Try the article on the current server up to [`MAX_TRIES_PER_SERVER`]
//!    times, reconnecting on transient errors.
//! 2. On `ArticleNotFound` (430) — requeue with the current server added to
//!    `tried_servers`; another worker on a different server picks it up.
//! 3. On connection loss — requeue and reconnect.
//! 4. On decode error — treated like "not available on this server", try
//!    another.
//! 5. When every enabled server is in `tried_servers` (or circuit-broken),
//!    the article is marked failed.
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

use crate::nzb_core::config::ServerConfig;
use crate::nzb_core::models::NzbJob;
use crate::nzb_core::nzb_nntp::Pipeline;
use crate::nzb_core::nzb_nntp::connection::NntpConnection;
use crate::nzb_core::nzb_nntp::error::NntpError;
use nzb_decode::FileAssembler;
use nzb_decode::yenc::decode_yenc;

use crate::bandwidth::BandwidthLimiter;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Max times to retry an article on the SAME server before trying the next.
const MAX_TRIES_PER_SERVER: u32 = 3;
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

// ---------------------------------------------------------------------------
// Global connection tracking — semaphore-backed permit pools
// ---------------------------------------------------------------------------

/// Tracks the per-server NNTP connection budget. Each server has a
/// `tokio::sync::Semaphore` whose initial permit count is the configured
/// `connections` limit. Workers acquire a permit *before* connecting and
/// hold it for their entire lifetime; the permit's `Drop` releases the slot
/// synchronously back to the pool. This makes over-allocation a type-level
/// impossibility — once `limit` permits are out, the next acquire awaits.
///
/// **Limit changes at runtime** are handled differently for grow vs shrink:
///
/// - **Grow** (e.g. 5 → 10): the existing semaphore is given the additional
///   permits via `add_permits`; existing slot holders are unaffected.
/// - **Shrink** (e.g. 10 → 5): the entire `ServerSlot` is replaced with a
///   fresh one. Old permit holders continue to reference the orphaned
///   semaphore via their `Arc`; their drops release back to it (a no-op
///   since nothing else points at it). Workers detect the replacement via
///   [`ConnectionTracker::slot_is_current`] and exit on the next iteration.
pub struct ConnectionTracker {
    pools: Mutex<HashMap<String, ServerSlot>>,
}

#[derive(Clone)]
struct ServerSlot {
    name: String,
    limit: usize,
    semaphore: Arc<tokio::sync::Semaphore>,
}

impl ConnectionTracker {
    pub fn new() -> Self {
        Self {
            pools: Mutex::new(HashMap::new()),
        }
    }

    /// Set or update the per-server connection limit.
    ///
    /// - First call for a `server_id`: creates a fresh semaphore with `limit` permits.
    /// - Subsequent call with the same `limit` and `server_name`: no-op.
    /// - Grow (`limit > current`): adds permits in place.
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
                slot.semaphore.add_permits(added);
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
                        semaphore: Arc::new(tokio::sync::Semaphore::new(limit)),
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
                        "Connection pool replaced (shrink or rename); old permits orphaned"
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
        let permit = Arc::clone(&server_slot.semaphore)
            .acquire_owned()
            .await
            .ok()?;
        Some(ConnectionSlot {
            server_id: server_id.to_string(),
            server_name: server_slot.name,
            semaphore_origin: server_slot.semaphore,
            _permit: permit,
        })
    }

    /// Returns true if `slot` was acquired from the *current* semaphore for
    /// its server. False if the limit was changed (semaphore replaced) or
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
                if Arc::ptr_eq(&server_slot.semaphore, &slot.semaphore_origin) {
                    SlotStatus::Current
                } else {
                    SlotStatus::PoolReplaced
                }
            }
            None => SlotStatus::ServerRemoved,
        }
    }

    /// `(server_id, active, limit)` triples for the live pool. `active` is
    /// derived from the semaphore's available permits and is always
    /// `<= limit` by construction.
    pub fn snapshot(&self) -> Vec<(String, usize, usize)> {
        let pools = self.pools.lock();
        pools
            .iter()
            .map(|(id, slot)| {
                let active = slot
                    .limit
                    .saturating_sub(slot.semaphore.available_permits());
                (id.clone(), active, slot.limit)
            })
            .collect()
    }

    /// Total currently-held permits across all servers in the live pool.
    /// Permits held against orphaned (replaced) semaphores are NOT counted —
    /// they'll go away as the holding workers exit.
    pub fn total(&self) -> usize {
        let pools = self.pools.lock();
        pools
            .values()
            .map(|s| s.limit.saturating_sub(s.semaphore.available_permits()))
            .sum()
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
    /// Slot is still attached to the current semaphore for its server.
    Current,
    /// The semaphore was replaced (set_limit shrunk or rebuilt the pool).
    /// Old permit holders should exit at their next safe checkpoint.
    PoolReplaced,
    /// The server has been removed entirely from the tracker.
    ServerRemoved,
}

/// RAII handle for one acquired NNTP connection slot. The underlying
/// semaphore permit is released synchronously on drop. Workers should hold
/// a `ConnectionSlot` for their entire lifetime — across reconnects, even
/// across temporary connect failures — and only drop it when exiting.
pub struct ConnectionSlot {
    server_id: String,
    server_name: String,
    /// The Arc identity of the semaphore the permit was issued by.
    /// Used by `ConnectionTracker::slot_is_current` to detect a stale slot
    /// after a `set_limit` shrink (which replaces the semaphore).
    semaphore_origin: Arc<tokio::sync::Semaphore>,
    _permit: tokio::sync::OwnedSemaphorePermit,
}

impl ConnectionSlot {
    pub fn server_id(&self) -> &str {
        &self.server_id
    }
    pub fn server_name(&self) -> &str {
        &self.server_name
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
    ArticleComplete {
        job_id: String,
        file_id: String,
        segment_number: u32,
        decoded_bytes: u64,
        file_complete: bool,
        server_id: Option<String>,
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
    JobFinished {
        job_id: String,
        success: bool,
        articles_failed: usize,
    },
    NoServersAvailable {
        job_id: String,
        reason: String,
    },
    JobAborted {
        job_id: String,
        reason: String,
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
    /// Number of attempts on the current server.
    pub(crate) tries_on_current: u32,
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
    /// Total bytes across all files (for perf summary throughput).
    pub total_bytes: u64,
    /// Ensures JobFinished/JobAborted is only emitted once.
    finished: AtomicBool,
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
            total_bytes: job.total_bytes,
            finished: AtomicBool::new(false),
        }
    }

    /// Decrement articles_remaining. If it reaches zero, run deobfuscation
    /// and emit the job-finished/aborted terminal update. Idempotent.
    fn resolve_one(&self) {
        let prev = self.articles_remaining.fetch_sub(1, Ordering::Relaxed);
        if prev != 1 {
            return;
        }
        self.emit_terminal();
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

        let abort_reason = self.abort_reason.lock().clone();
        if let Some(reason) = abort_reason {
            try_send_progress(
                &self.progress_tx,
                &self.job_id,
                ProgressUpdate::JobAborted {
                    job_id: self.job_id.clone(),
                    reason,
                },
            );
            return;
        }

        let failed = self.articles_failed.load(Ordering::Relaxed);
        try_send_progress(
            &self.progress_tx,
            &self.job_id,
            ProgressUpdate::JobFinished {
                job_id: self.job_id.clone(),
                success: failed == 0,
                articles_failed: failed,
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

/// Multi-job FIFO work queue with PAR2-first priority within each submission.
///
/// Items submitted via [`SharedWorkQueue::submit_items`] are inserted so that
/// PAR2 index and volume files land ahead of data files (matching the prior
/// per-job ordering), while data files land at the tail. Cross-job ordering
/// is FIFO by submission time, per the chosen FIFO priority model.
pub(crate) struct SharedWorkQueue {
    inner: Mutex<VecDeque<WorkItem>>,
    notify: Notify,
}

impl SharedWorkQueue {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(VecDeque::new()),
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
            q.reserve(items.len());
            for item in items {
                q.push_back(item);
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
        self.inner.lock().push_front(item);
        self.notify.notify_waiters();
    }

    /// Push a single item to the back (used after handle_article_not_available
    /// when another server can still try it).
    fn push_back(&self, item: WorkItem) {
        self.inner.lock().push_back(item);
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
        let total = q.len();
        let workable = q
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
    /// Skips items that have already tried `server_id`, rotating them to the
    /// back of the queue. Also enforces server priority: items where any
    /// healthy higher-priority server has not yet tried the article are
    /// rotated to the back so the primary server sees them first.
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
        let mut q = self.inner.lock();
        let len = q.len();
        for _ in 0..len {
            let item = q.pop_front()?;
            if item.tried_servers.iter().any(|s| s == server_id) {
                q.push_back(item);
                continue;
            }
            // Priority gate: rotate back if any higher-priority server still
            // needs to try this item. Matches SABnzbd's get_article() behaviour
            // (sabnzbd/nzb/article.py:149-170).
            if higher_priority_servers
                .iter()
                .any(|hp| !item.tried_servers.contains(hp))
            {
                q.push_back(item);
                continue;
            }
            return Some(item);
        }
        None
    }

    /// Remove all items belonging to `job_id`. Used on cancel_job / remove_job.
    fn drain_job(&self, job_id: &str) -> Vec<WorkItem> {
        let mut q = self.inner.lock();
        let mut kept = VecDeque::with_capacity(q.len());
        let mut drained = Vec::new();
        while let Some(item) = q.pop_front() {
            if item.job_id == job_id {
                drained.push(item);
            } else {
                kept.push_back(item);
            }
        }
        *q = kept;
        drained
    }

    pub fn len(&self) -> usize {
        self.inner.lock().len()
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
    handle: JoinHandle<()>,
}

/// Long-lived worker pool that services all active download jobs.
pub struct WorkerPool {
    work_queue: Arc<SharedWorkQueue>,
    job_contexts: JobContextMap,
    servers: Arc<Mutex<Vec<ServerConfig>>>,
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
    /// Per-server "last starvation log" timestamp; rate-limits the
    /// "no workable items" diagnostic to once per minute per server.
    starvation_log: Mutex<HashMap<String, Instant>>,
    /// Lifetime count of worker evictions performed by the heartbeat
    /// watchdog. Useful for tests and observability — a non-zero value
    /// means at least one worker stalled long enough to be reclaimed.
    evictions: AtomicU64,
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
            starvation_log: Mutex::new(HashMap::new()),
            evictions: AtomicU64::new(0),
            workers: Mutex::new(HashMap::new()),
            shutdown: Arc::new(AtomicBool::new(false)),
            supervisor_handle: Mutex::new(None),
        })
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
    pub fn reconcile_servers(self: &Arc<Self>) {
        if self.shutdown.load(Ordering::Relaxed) {
            return;
        }

        let servers_snapshot: Vec<ServerConfig> = self.servers.lock().clone();
        let mut workers = self.workers.lock();

        // First pass: retire workers for servers that are gone or disabled.
        let mut retire: Vec<String> = Vec::new();
        for key in workers.keys() {
            let still_active = servers_snapshot.iter().any(|s| s.enabled && &s.id == key);
            if !still_active {
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
        self.job_contexts.lock().insert(job_id.clone(), ctx);
        self.work_queue.submit_items(items);
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
            ctx.paused.store(true, Ordering::Relaxed);
        }
    }

    /// Resume a paused job.
    pub fn resume_job(&self, job_id: &str) {
        if let Some(ctx) = self.job_contexts.lock().get(job_id) {
            ctx.paused.store(false, Ordering::Relaxed);
            // Wake any workers that were idle waiting for work.
            self.work_queue.notify.notify_waiters();
        }
    }

    /// Abort a job with a reason. Drains queued items, sets the abort flag,
    /// and emits JobAborted via the job's progress channel.
    pub fn abort_job(&self, job_id: &str, reason: String) {
        let ctx = self.job_contexts.lock().get(job_id).cloned();
        let Some(ctx) = ctx else {
            return;
        };
        *ctx.abort_reason.lock() = Some(reason);
        ctx.cancelled.store(true, Ordering::Relaxed);
        let drained = self.work_queue.drain_job(job_id);
        // Decrement the remaining counter for drained items so the terminal
        // callback fires if nothing is in-flight for this job.
        for _ in drained {
            ctx.resolve_one();
        }
        ctx.emit_terminal();
        self.job_contexts.lock().remove(job_id);
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

    /// Emit NoServersAvailable for a stuck job and unregister it.
    fn mark_no_servers(&self, job_id: &str, reason: String) {
        let ctx = self.job_contexts.lock().remove(job_id);
        let Some(ctx) = ctx else {
            return;
        };
        ctx.paused.store(true, Ordering::Relaxed);
        try_send_progress(
            &ctx.progress_tx,
            &ctx.job_id,
            ProgressUpdate::NoServersAvailable {
                job_id: ctx.job_id.clone(),
                reason,
            },
        );
        // Remove pending work for this job so other jobs aren't blocked.
        let _ = self.work_queue.drain_job(job_id);
    }

    /// Supervisor loop: periodically detects jobs whose remaining articles
    /// cannot possibly be fetched (all enabled servers circuit-broken or
    /// Per-tick checks that maintain pool health: idle-worker eviction,
    /// dead-worker reaping, reconcile (respawn missing workers), starvation
    /// diagnostics, and the legacy "all servers broken" pause.
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
                for (_id, list) in workers.iter_mut() {
                    list.retain(|w| !w.handle.is_finished());
                }
            }
            // Reconcile fills any gaps left by reaped workers.
            self.reconcile_servers();

            // ---------- 3. Starvation diagnostic ----------
            // For each enabled server: if the queue has items but none are
            // workable for this server, log once per minute. "Not workable"
            // can mean either (a) every item has already been tried here, or
            // (b) every item is still waiting on a higher-priority server
            // (backup server legitimately idle — not a bug).
            let enabled_servers: Vec<String> =
                server_priorities.iter().map(|(id, _)| id.clone()).collect();
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

            // ---------- 4. Legacy "all servers broken" pause ----------
            if enabled_servers.is_empty() {
                continue;
            }
            let healthy_servers: Vec<String> = {
                let health = self.server_health.lock();
                enabled_servers
                    .iter()
                    .filter(|sid| health.get(sid.as_str()).is_none_or(|h| h.is_available()))
                    .cloned()
                    .collect()
            };
            let all_broken = healthy_servers.is_empty();

            let ctxs: Vec<Arc<JobContext>> = self.job_contexts.lock().values().cloned().collect();
            for ctx in ctxs {
                if ctx.articles_remaining.load(Ordering::Relaxed) == 0 {
                    continue;
                }
                if ctx.cancelled.load(Ordering::Relaxed) {
                    continue;
                }
                if all_broken {
                    let reason = {
                        let health = self.server_health.lock();
                        health
                            .values()
                            .filter_map(|h| h.reason.clone())
                            .next()
                            .unwrap_or_else(|| "All servers unavailable".into())
                    };
                    warn!(
                        job_id = %ctx.job_id,
                        remaining = ctx.articles_remaining.load(Ordering::Relaxed),
                        "All servers circuit-broken — pausing job for user intervention"
                    );
                    self.mark_no_servers(&ctx.job_id, reason);
                }
            }
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

        let timeout = Duration::from_secs(10);
        for h in handles {
            let _ = tokio::time::timeout(timeout, h).await;
        }

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
}

// ---------------------------------------------------------------------------
// Worker body
// ---------------------------------------------------------------------------

/// Single pool worker. Owns an NNTP connection to `primary_server` and pulls
/// items from the shared queue until `worker_shutdown` is flipped (server
/// reconciled away, limit shrunk) or the pool shuts down.
///
/// The worker acquires its connection slot (a semaphore permit from the
/// per-server pool) **before** the first connect attempt and holds it for
/// the entire lifetime of the worker, across every reconnect. This bounds
/// concurrent connections to `server.connections` by construction.
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
    let mut conn_slot = match pool.conn_tracker.acquire(&primary_server.id).await {
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

        // If the limit was shrunk under us, the semaphore will have been
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
            tokio::time::sleep(WORKER_IDLE_POLL).await;
            continue 'reconnect;
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
        if let Err(e) = connect_with_retry(
            &mut conn,
            &primary_server,
            &worker_id,
            &pool.server_health,
            &pool.servers,
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
            tokio::time::sleep(RECONNECT_DELAY).await;
            continue 'reconnect;
        }

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

        let _ = conn.quit().await;

        match reconnect_needed {
            WorkerExit::Reconnect => {
                // Loop back to the top and reconnect — slot is preserved.
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
}

/// Wait for work, with early exit on shutdown / server retirement.
/// Returns `Some(item, ctx)` when a processable item is available, or `None`
/// if the worker should exit.
///
/// `higher_priority_servers` gates which items this server is allowed to take
/// — see `pop_workable` for the priority model.
async fn next_work_item(
    pool: &Arc<WorkerPool>,
    server_id: &str,
    higher_priority_servers: &[String],
    worker_shutdown: &Arc<AtomicBool>,
) -> Option<(WorkItem, Arc<JobContext>)> {
    loop {
        if worker_shutdown.load(Ordering::Relaxed) || pool.shutdown.load(Ordering::Relaxed) {
            return None;
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
                continue;
            }
            // Respect per-job pause: return the item and wait.
            if ctx.paused.load(Ordering::Relaxed) {
                pool.work_queue.push_back(item);
                tokio::time::sleep(WORKER_IDLE_POLL).await;
                continue;
            }
            return Some((item, ctx));
        }

        // Queue empty (or nothing workable for this server) — wait with a
        // timeout so we still notice shutdown and new work alike.
        let notified = pool.work_queue.notify.notified();
        tokio::select! {
            _ = notified => {}
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
    _conn_slot: &mut ConnectionSlot,
    last_progress: &Arc<AtomicU64>,
) -> WorkerExit {
    let mut consecutive_errors: u32 = 0;

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

        let Some((mut item, ctx)) = next_work_item(
            pool,
            &primary_server.id,
            &higher_priority_servers,
            worker_shutdown,
        )
        .await
        else {
            return WorkerExit::Exit;
        };

        let fetch_fut =
            fetch_article_with_retry(conn, &item, &ctx.assembler, primary_server, worker_id);
        let result = if let Some(timeout) = pool.stall_timeout {
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
                    pool.work_queue.push_front(item);
                    return WorkerExit::Reconnect;
                }
            }
        } else {
            fetch_fut.await
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
                    },
                );
                ctx.resolve_one();
                // Heartbeat: definitive forward progress.
                last_progress.store(pool.elapsed_ms(), Ordering::Relaxed);
            }
            Err(ArticleError::ArticleNotFound) => {
                if handle_article_not_available(
                    &mut item,
                    primary_server,
                    &pool.servers,
                    &pool.server_health,
                    &ctx,
                    &pool.work_queue,
                    crate::article_failure::ArticleFailureKind::NotFound,
                    "Article not found on any server",
                ) {
                    last_progress.store(pool.elapsed_ms(), Ordering::Relaxed);
                }
            }
            Err(ArticleError::ConnectionLost(msg)) => {
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
            Err(ArticleError::DecodeError(msg)) => {
                if handle_article_not_available(
                    &mut item,
                    primary_server,
                    &pool.servers,
                    &pool.server_health,
                    &ctx,
                    &pool.work_queue,
                    crate::article_failure::ArticleFailureKind::DecodeError,
                    &format!("Decode error: {msg}"),
                ) {
                    last_progress.store(pool.elapsed_ms(), Ordering::Relaxed);
                }
            }
            Err(ArticleError::AssemblyError(msg)) => {
                error!(article = %item.message_id, "Assembly error: {msg}");
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
    _conn_slot: &mut ConnectionSlot,
    last_progress: &Arc<AtomicU64>,
) -> WorkerExit {
    let mut pipeline = Pipeline::new(pipe_depth);
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
            let Some((first_item, ctx)) = next_work_item(
                pool,
                &primary_server.id,
                &higher_priority_servers,
                worker_shutdown,
            )
            .await
            else {
                return WorkerExit::Exit;
            };
            let _ = ctx; // ctx is validated in next_work_item
            let tag = next_tag;
            next_tag += 1;
            pipeline.submit(first_item.message_id.clone(), tag);
            in_flight_items.insert(tag, first_item);
        }

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
                    continue;
                }

                match pipe_result.result {
                    Ok(response) => {
                        consecutive_errors = 0;
                        let raw_data = response.data.unwrap_or_default();
                        let yield_t = Instant::now();
                        tokio::task::yield_now().await;
                        perf_yield_us += yield_t.elapsed().as_micros() as u64;
                        match decode_and_assemble(&item, &raw_data, &ctx.assembler) {
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
                                if handle_article_not_available(
                                    &mut item,
                                    primary_server,
                                    &pool.servers,
                                    &pool.server_health,
                                    &ctx,
                                    &pool.work_queue,
                                    crate::article_failure::ArticleFailureKind::DecodeError,
                                    &format!("Decode error: {msg}"),
                                ) {
                                    last_progress.store(pool.elapsed_ms(), Ordering::Relaxed);
                                }
                            }
                            Err(ArticleError::AssemblyError(msg)) => {
                                error!(article = %item.message_id, "Assembly error: {msg}");
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
                    Err(NntpError::ArticleNotFound(_)) => {
                        if handle_article_not_available(
                            &mut item,
                            primary_server,
                            &pool.servers,
                            &pool.server_health,
                            &ctx,
                            &pool.work_queue,
                            crate::article_failure::ArticleFailureKind::NotFound,
                            "Article not found on any server",
                        ) {
                            last_progress.store(pool.elapsed_ms(), Ordering::Relaxed);
                        }
                    }
                    Err(NntpError::Connection(_) | NntpError::Io(_)) => {
                        warn!(
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
                        warn!(worker = %worker_id, article = %item.message_id, "Pipeline error: {e}");
                        let kind = crate::article_failure::ArticleFailure::from_nntp(
                            &e,
                            &primary_server.id,
                        )
                        .kind;
                        if handle_article_not_available(
                            &mut item,
                            primary_server,
                            &pool.servers,
                            &pool.server_health,
                            &ctx,
                            &pool.work_queue,
                            kind,
                            &format!("Pipeline error: {e}"),
                        ) {
                            last_progress.store(pool.elapsed_ms(), Ordering::Relaxed);
                        }
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
    kind: crate::article_failure::ArticleFailureKind,
    error_msg: &str,
) -> bool {
    item.tried_servers.push(primary_server.id.clone());
    item.tries_on_current = 0;

    let all_tried = {
        let servers = all_servers.lock();
        let health = server_health.lock();
        servers.iter().filter(|s| s.enabled).all(|s| {
            item.tried_servers.contains(&s.id)
                || health.get(&s.id).is_some_and(|h| !h.is_available())
        })
    };

    debug!(
        article = %item.message_id,
        server = %primary_server.id,
        kind = kind.as_str(),
        tried_count = item.tried_servers.len(),
        all_tried,
        "Article returned error on this server"
    );

    // (debug log immediately below was added for observability)
    if all_tried {
        warn!(article = %item.message_id, kind = kind.as_str(), "{error_msg}");
        // Promote a per-server NotFound to a definitive NotFound now that
        // every server has been exhausted. DecodeError keeps its kind.
        let final_failure = if kind == crate::article_failure::ArticleFailureKind::DecodeError {
            crate::article_failure::ArticleFailure::decode_error(
                &primary_server.id,
                error_msg.to_string(),
            )
        } else {
            crate::article_failure::ArticleFailure::not_found_anywhere(&primary_server.id)
        };
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

/// Re-queue all in-flight items back to the work queue (on connection loss).
fn requeue_all(in_flight: &mut HashMap<u64, WorkItem>, work_queue: &Arc<SharedWorkQueue>) {
    let items: Vec<WorkItem> = in_flight.drain().map(|(_, item)| item).collect();
    for item in items {
        work_queue.push_front(item);
    }
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

fn has_known_extension(name: &str) -> bool {
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
) -> (Arc<JobContext>, Vec<WorkItem>) {
    let assembler = Arc::new(FileAssembler::new());
    for file in &job.files {
        let output_path = job.work_dir.join(&file.filename);
        if let Err(e) =
            assembler.register_file(&job.id, &file.id, output_path, file.articles.len() as u32)
        {
            error!(file = %file.filename, "Failed to register file for assembly: {e}");
        }
    }

    let work_items: Vec<WorkItem> = job
        .files
        .iter()
        .flat_map(|file| {
            file.articles
                .iter()
                .enumerate()
                .filter(|(_, a)| !a.downloaded)
                .map(move |(idx, article)| WorkItem {
                    job_id: job.id.clone(),
                    file_id: file.id.clone(),
                    filename: file.filename.clone(),
                    message_id: article.message_id.clone(),
                    segment_number: (idx as u32) + 1,
                    tried_servers: Vec::new(),
                    tries_on_current: 0,
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
    (ctx, work_items)
}

// ---------------------------------------------------------------------------
// Article fetch with per-server retry
// ---------------------------------------------------------------------------

async fn fetch_article_with_retry(
    conn: &mut NntpConnection,
    item: &WorkItem,
    assembler: &FileAssembler,
    _server: &ServerConfig,
    worker_id: &str,
) -> Result<ProcessResult, ArticleError> {
    let mut last_error = None;

    for attempt in 1..=MAX_TRIES_PER_SERVER {
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
                return decode_and_assemble(item, &raw_data, assembler);
            }
            Err(NntpError::ArticleNotFound(_)) => {
                debug!(
                    worker = %worker_id,
                    article = %item.message_id,
                    "Article not found (430) — will try next server"
                );
                return Err(ArticleError::ArticleNotFound);
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
                return Err(ArticleError::ConnectionLost(format!(
                    "Connection error on attempt {attempt}: {e}"
                )));
            }
            Err(e @ NntpError::Tls(_)) => {
                warn!(
                    worker = %worker_id,
                    article = %item.message_id,
                    attempt,
                    error = %e,
                    "TLS error during fetch — connection lost"
                );
                return Err(ArticleError::ConnectionLost(format!("TLS error: {e}")));
            }
            Err(e @ NntpError::ServiceUnavailable(_)) => {
                warn!(
                    worker = %worker_id,
                    article = %item.message_id,
                    attempt,
                    error = %e,
                    "Service unavailable (502) during article fetch — likely rate limited or blocked"
                );
                return Err(ArticleError::ConnectionLost(format!(
                    "Service unavailable: {e}"
                )));
            }
            Err(e @ NntpError::AuthRequired(_)) => {
                warn!(
                    worker = %worker_id,
                    article = %item.message_id,
                    attempt,
                    error = %e,
                    "Auth required (480) during article fetch — session expired or rate limited"
                );
                return Err(ArticleError::ConnectionLost(format!(
                    "Auth required mid-session: {e}"
                )));
            }
            Err(e) => {
                last_error = Some(format!("{e}"));
                if attempt < MAX_TRIES_PER_SERVER {
                    warn!(
                        worker = %worker_id,
                        article = %item.message_id,
                        attempt,
                        max_tries = MAX_TRIES_PER_SERVER,
                        error = %e,
                        "Transient fetch error, retrying in 500ms"
                    );
                    tokio::time::sleep(Duration::from_millis(500)).await;
                } else {
                    warn!(
                        worker = %worker_id,
                        article = %item.message_id,
                        attempt,
                        error = %e,
                        "All retries on this server exhausted"
                    );
                }
            }
        }
    }

    Err(ArticleError::DecodeError(
        last_error.unwrap_or_else(|| "Unknown error after retries".into()),
    ))
}

// ---------------------------------------------------------------------------
// Article processing
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct ProcessResult {
    decoded_bytes: u64,
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
    #[error("Decode error: {0}")]
    DecodeError(String),
    #[error("Assembly error: {0}")]
    AssemblyError(String),
}

fn decode_and_assemble(
    item: &WorkItem,
    raw_data: &[u8],
    assembler: &FileAssembler,
) -> Result<ProcessResult, ArticleError> {
    let decode_start = Instant::now();
    let decoded = decode_yenc(raw_data).map_err(|e| {
        ArticleError::DecodeError(format!(
            "yEnc decode failed for {} seg {}: {e}",
            item.filename, item.segment_number
        ))
    })?;
    let decode_us = decode_start.elapsed().as_micros();

    let yenc_filename = decoded.filename;
    let data_begin = decoded.part_begin.unwrap_or(0);
    let decoded_len = decoded.data.len() as u64;

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

    fn test_job(job_id: &str, root: &std::path::Path) -> NzbJob {
        NzbJob {
            id: job_id.to_string(),
            name: job_id.to_string(),
            category: "Default".to_string(),
            status: crate::nzb_core::models::JobStatus::Downloading,
            priority: crate::nzb_core::models::Priority::Normal,
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
            server_stats: Vec::new(),
            files: Vec::new(),
        }
    }

    fn insert_test_context(pool: &WorkerPool, job: &NzbJob, assembler: Arc<FileAssembler>) {
        let (progress_tx, _progress_rx) = mpsc::channel(1);
        let ctx = Arc::new(JobContext::new(job, assembler, progress_tx, 1));
        pool.job_contexts.lock().insert(job.id.clone(), ctx);
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
            tries_on_current: 0,
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
    // ConnectionTracker (Phase 4 — semaphore-backed slots)
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

        // Shrink to 1 — old semaphore is replaced.
        t.set_limit("srv1", "Server 1", 1);
        assert!(
            !t.slot_is_current(&s),
            "after shrink, the previously-acquired slot must be marked stale"
        );

        // The new pool starts empty (1 permit available, 0 in use). Old
        // permit holder is no longer counted in `total()` because its
        // semaphore is orphaned.
        assert_eq!(t.total(), 0);

        // We can still acquire from the new pool.
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
}
