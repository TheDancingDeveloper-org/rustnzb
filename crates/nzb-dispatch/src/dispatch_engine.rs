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

use crate::download_engine::{ProgressUpdate, WorkerPool, build_job_submission};
use nzb_core::models::NzbJob;

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
    pub fn new(pool: Arc<WorkerPool>) -> Self {
        Self(pool)
    }

    /// Escape hatch: access the underlying pool. Intended for callers that
    /// still need pool-specific APIs not yet promoted to the trait (none
    /// today, but keeps the migration incremental).
    pub fn pool(&self) -> &Arc<WorkerPool> {
        &self.0
    }
}

#[async_trait::async_trait]
impl DispatchEngine for DispatchHandle {
    fn start(&self) {
        self.0.start();
    }

    fn submit_job(&self, job: &NzbJob, progress_tx: mpsc::Sender<ProgressUpdate>) {
        let (ctx, items) = build_job_submission(job, progress_tx);
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
