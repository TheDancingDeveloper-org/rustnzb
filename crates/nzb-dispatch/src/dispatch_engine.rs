//! `DispatchEngine` trait — boundary between the job queue and the article dispatcher.
//!
//! This is the contract the queue layer depends on; it hides the concrete
//! `WorkerPool` implementation. The reverse direction (dispatcher → queue) is
//! already a decoupled channel (`mpsc::Sender<ProgressUpdate>`), so only this
//! one trait is needed to cleanly separate the two layers.
//!
//! A `DispatchEngine` is responsible for turning an [`NzbJob`] into article
//! fetches against the configured NNTP servers and reporting progress via
//! the per-job [`ProgressUpdate`] channel. It must be able to pause, resume,
//! cancel, and abort individual jobs, reconcile its worker set with the
//! server list, and shut down gracefully.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;

use crate::bandwidth::BandwidthLimiter;
use crate::download_engine::{ConnectionTracker, ProgressUpdate, WorkerPool, build_job_submission};
use nzb_core::config::ServerConfig;
use nzb_core::models::NzbJob;
use parking_lot::Mutex;

/// Article-dispatch engine: accepts jobs, drives NNTP fetches, emits progress.
///
/// Constructed by the facade/queue layer and owned as `Arc<dyn DispatchEngine>`.
/// All methods are `&self`; implementations use interior mutability to allow
/// concurrent use across tokio tasks.
#[async_trait::async_trait]
pub trait DispatchEngine: Send + Sync {
    /// Spawn workers for all currently enabled servers and start the
    /// supervisor task. Idempotent (safe to call more than once).
    fn start(&self);

    /// Register a new job and begin dispatching its unfinished articles.
    /// Progress is streamed to `progress_tx` as `ProgressUpdate`s; the
    /// channel is closed when the job reaches a terminal state.
    fn submit_job(&self, job: &NzbJob, progress_tx: mpsc::Sender<ProgressUpdate>);

    /// Pause dispatch for `job_id`. In-flight articles finish; no new work
    /// is popped for this job until [`resume_job`](Self::resume_job).
    fn pause_job(&self, job_id: &str);

    /// Resume a paused job.
    fn resume_job(&self, job_id: &str);

    /// Cancel `job_id`. In-flight articles may still complete but their
    /// results are discarded; no terminal progress update is emitted beyond
    /// the one triggered by cancellation.
    fn cancel_job(&self, job_id: &str);

    /// Abort `job_id` with a human-readable reason. Emits
    /// [`ProgressUpdate::JobAborted`] once outstanding articles drain.
    /// Returns `true` only for the caller that won terminal ownership.
    fn abort_job(&self, job_id: &str, reason: String) -> bool;

    /// Is `job_id` currently known to the dispatcher?
    fn has_job(&self, job_id: &str) -> bool;

    /// Release a terminal job's dispatcher and assembler resources before
    /// post-processing opens the completed files.
    fn release_completed_job(&self, job_id: &str);

    /// Replace server configuration and reconcile connection budgets/workers.
    fn update_servers(&self, servers: Vec<ServerConfig>);

    /// Per-server allocated worker slots and configured limits.
    fn connection_snapshot(&self) -> Vec<(String, usize, usize)>;

    /// Per-server connections actively transferring articles and limits.
    fn active_connection_snapshot(&self) -> Vec<(String, usize, usize)>;

    /// Total allocated worker slots across current server pools.
    fn connection_total(&self) -> usize;

    /// Re-read the server list and adjust workers to match. Call after any
    /// mutation to the server config (add, remove, enable, disable, resize).
    fn reconcile_servers(&self);

    /// Override the idle-worker eviction threshold. Tests shrink this to
    /// make the watchdog converge in seconds; production uses the default.
    fn set_max_worker_idle(&self, d: Duration);

    /// Lifetime count of worker evictions performed by the heartbeat
    /// watchdog. Increases by 1 each time the supervisor reclaims a stalled
    /// worker. Useful for tests and observability.
    fn eviction_count(&self) -> u64;

    /// Snapshot of per-server lifetime attempt counters. Used by the
    /// queue manager to emit a diagnostic breakdown alongside a job abort
    /// — distinguishes "server returned 430 for every article" (dead NZB)
    /// from "server had auth errors" (transient). Default is empty for
    /// engines that don't track per-server stats.
    fn server_stats_snapshot(&self) -> Vec<(String, ServerAttemptStats)> {
        Vec::new()
    }

    /// Gracefully shut down: stop accepting new work, signal all workers,
    /// and wait for the supervisor to exit.
    async fn shutdown(&self);
}

/// Per-server lifetime counters reported via
/// [`DispatchEngine::server_stats_snapshot`]. `not_found` is the strongest
/// signal for a dead NZB; `transient_failed` separates "missing articles"
/// from "server flaky / auth issues".
#[derive(Debug, Clone, Copy, Default)]
pub struct ServerAttemptStats {
    pub attempted: u64,
    pub succeeded: u64,
    pub not_found: u64,
    pub transient_failed: u64,
}

// ---------------------------------------------------------------------------
// DispatchHandle — wraps Arc<WorkerPool> to implement DispatchEngine.
//
// Why a wrapper: several WorkerPool methods have `self: &Arc<Self>` receivers
// (they spawn tasks that clone the Arc). That signature is incompatible with
// a `dyn`-object trait method. The wrapper holds `Arc<WorkerPool>` and can
// call the concrete Arc-receiver methods on it.
// ---------------------------------------------------------------------------

/// Dynamic-dispatch wrapper around `Arc<WorkerPool>` — the one concrete
/// [`DispatchEngine`] impl today. Extract this into `nzb-dispatch` in Phase B.
pub struct DispatchHandle(Arc<WorkerPool>);

impl DispatchHandle {
    pub fn new(
        servers: Arc<Mutex<Vec<ServerConfig>>>,
        bandwidth: Arc<BandwidthLimiter>,
        article_timeout_secs: u64,
    ) -> Self {
        let tracker = Arc::new(ConnectionTracker::new());
        for server in servers.lock().iter() {
            tracker.set_limit(&server.id, &server.name, server.connections as usize);
        }
        Self(WorkerPool::new(
            servers,
            bandwidth,
            tracker,
            article_timeout_secs,
        ))
    }
}

#[async_trait::async_trait]
impl DispatchEngine for DispatchHandle {
    fn start(&self) {
        self.0.start();
    }

    fn submit_job(&self, job: &NzbJob, progress_tx: mpsc::Sender<ProgressUpdate>) {
        let enabled_server_ids = self
            .0
            .servers
            .lock()
            .iter()
            .filter(|server| server.enabled)
            .map(|server| server.id.clone())
            .collect::<Vec<_>>();
        let (ctx, items) = build_job_submission(job, progress_tx, &enabled_server_ids);
        self.0.submit_job(ctx, items);
    }

    fn pause_job(&self, job_id: &str) {
        self.0.pause_job(job_id);
    }

    fn resume_job(&self, job_id: &str) {
        self.0.resume_job(job_id);
    }

    fn cancel_job(&self, job_id: &str) {
        self.0.cancel_job(job_id);
    }

    fn abort_job(&self, job_id: &str, reason: String) -> bool {
        self.0.abort_job(job_id, reason)
    }

    fn has_job(&self, job_id: &str) -> bool {
        self.0.has_job(job_id)
    }

    fn release_completed_job(&self, job_id: &str) {
        self.0.release_completed_job(job_id);
    }

    fn update_servers(&self, servers: Vec<ServerConfig>) {
        let new_ids: std::collections::HashSet<_> =
            servers.iter().map(|server| server.id.clone()).collect();
        for (old_id, _, _) in self.0.conn_tracker().snapshot() {
            if !new_ids.contains(&old_id) {
                self.0.conn_tracker().remove_server(&old_id);
            }
        }
        for server in &servers {
            self.0
                .conn_tracker()
                .set_limit(&server.id, &server.name, server.connections as usize);
        }
        *self.0.servers.lock() = servers;
        self.0.reconcile_servers();
    }

    fn connection_snapshot(&self) -> Vec<(String, usize, usize)> {
        self.0.conn_tracker().snapshot()
    }

    fn active_connection_snapshot(&self) -> Vec<(String, usize, usize)> {
        self.0.conn_tracker().connected_snapshot()
    }

    fn connection_total(&self) -> usize {
        self.0.conn_tracker().total()
    }

    fn reconcile_servers(&self) {
        self.0.reconcile_servers();
    }

    fn set_max_worker_idle(&self, d: Duration) {
        self.0.set_max_worker_idle(d);
    }

    fn eviction_count(&self) -> u64 {
        self.0.eviction_count()
    }

    async fn shutdown(&self) {
        self.0.shutdown().await;
    }
}
