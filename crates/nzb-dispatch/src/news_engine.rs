//! `NewsDispatchEngine` — `DispatchEngine` impl backed by the `nzb-news` crate.
//!
//! The layered news engine is a pure NNTP fetch layer: it takes per-article
//! work items and emits [`nzb_news::FetchOutcome`]s. This adapter bolts the
//! rest of the pipeline on top of it so it satisfies the contract the old
//! `WorkerPool` engine used to satisfy:
//!
//! 1. **Fetch** — delegated to `nzb_news::spawn_downloader`.
//! 2. **Decode** — `nzb_decode::decode_yenc` on each successful outcome.
//! 3. **Assemble** — `FileAssembler::assemble_article` writes the decoded
//!    bytes at the yEnc-declared offset.
//! 4. **Progress** — translates per-article outcomes into
//!    [`ProgressUpdate::ArticleComplete`] / [`ProgressUpdate::ArticleFailed`];
//!    drives job-level terminal via `JobContext::resolve_one`.
//!
//! Per-job lifecycle (pause/resume/cancel/abort) is tracked in this adapter
//! because the news engine is job-agnostic. We keep a `JobContext` per job
//! (same struct the old engine used — it owns the assembler, progress
//! channel, deobfuscation state, and terminal-emit logic).
//!
//! MVP limitations — marked with TODO comments:
//! - `pause_job` / `resume_job` are no-ops (work items are submitted
//!   eagerly; pause-gating is a follow-up).
//! - `reconcile_servers` is a no-op (nzb-news doesn't expose mid-flight
//!   server reconfiguration yet; requires a downloader rebuild).
//! - `set_max_worker_idle` / `eviction_count` are stubs (no idle-worker
//!   pool concept in nzb-news).

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use parking_lot::{Mutex, RwLock};
use tokio::sync::{Notify, mpsc};
use tracing::{debug, info};

use nzb_core::models::NzbJob;
use nzb_nntp::config::ServerConfig;

use crate::article_failure::{ArticleFailure, ArticleFailureKind};
use crate::dispatch_engine::DispatchEngine;
use crate::download_engine::{JobContext, ProgressUpdate, build_job_submission};

// ---------------------------------------------------------------------------
// Tuning knobs
// ---------------------------------------------------------------------------

/// How many articles the nzb-news downloader will hold in-flight across all
/// servers at once. Matches the old engine's rough ceiling.
const DEFAULT_MAX_CONCURRENT_FETCHES: usize = 40;

/// Work channel depth inside nzb-news. Articles are buffered here between
/// `submit_job` enqueue and the per-server fan-out.
const DEFAULT_WORK_CHANNEL_CAPACITY: usize = 4096;

/// Outcome channel depth. Must be large enough that a momentary backlog in
/// the decode path doesn't block the fetch loop.
const DEFAULT_OUTCOME_CHANNEL_CAPACITY: usize = 4096;

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

/// Configuration for [`NewsDispatchEngine`]. Mirrors the knobs exposed by
/// the old engine so the swap is drop-in from the caller's perspective.
///
/// `servers` is held as an `Arc<Mutex<_>>` so the caller (queue manager) can
/// mutate it and [`DispatchEngine::reconcile_servers`] will pick up the new
/// list without requiring a new config instance or a full engine rebuild.
#[derive(Clone)]
pub struct NewsEngineConfig {
    pub servers: Arc<Mutex<Vec<ServerConfig>>>,
    pub article_timeout: Duration,
    pub max_concurrent_fetches: usize,
    pub work_channel_capacity: usize,
    pub outcome_channel_capacity: usize,
    /// Optional backup-server probe policy. Forwarded verbatim to
    /// `nzb_news::DownloaderConfig::probe_policy`.
    ///
    /// `None` disables probing (every cascade article tries every server).
    /// `Some(_)` enables fast-fail on a backup server when the probed
    /// hit-rate falls below the threshold for that job.
    ///
    /// Defaults to `Some(ServerProbePolicy::default())` which matches the
    /// nzb-news default (probe 10 articles, require >=10% hits).
    pub probe_policy: Option<nzb_news::ServerProbePolicy>,
}

impl NewsEngineConfig {
    /// Construct a config from an owned server list. Wraps the list in an
    /// `Arc<Mutex<_>>` internally; if the caller already owns a shared Arc
    /// (e.g. queue manager's live server list), use
    /// [`NewsEngineConfig::with_shared_servers`] instead so mutations are
    /// visible to the engine.
    pub fn new(servers: Vec<ServerConfig>, article_timeout: Duration) -> Self {
        Self::with_shared_servers(Arc::new(Mutex::new(servers)), article_timeout)
    }

    /// Construct a config sharing an existing `Arc<Mutex<Vec<ServerConfig>>>`
    /// with the caller. Mutating the Arc from outside and then calling
    /// [`DispatchEngine::reconcile_servers`] rebuilds the downloader with
    /// the latest server list — this is how live "add/remove server"
    /// operations reach the fetch layer.
    pub fn with_shared_servers(
        servers: Arc<Mutex<Vec<ServerConfig>>>,
        article_timeout: Duration,
    ) -> Self {
        Self {
            servers,
            article_timeout,
            max_concurrent_fetches: DEFAULT_MAX_CONCURRENT_FETCHES,
            work_channel_capacity: DEFAULT_WORK_CHANNEL_CAPACITY,
            outcome_channel_capacity: DEFAULT_OUTCOME_CHANNEL_CAPACITY,
            probe_policy: Some(nzb_news::ServerProbePolicy::default()),
        }
    }
}

// ---------------------------------------------------------------------------
// Adapter state
// ---------------------------------------------------------------------------

/// Shared state held behind an `Arc` so the outcome-dispatcher task can
/// access jobs while the engine is owned by the caller.
struct Inner {
    config: NewsEngineConfig,
    /// Populated by `start()`. Holds the downloader's work-submission sender.
    handle: RwLock<Option<nzb_news::DownloaderHandle>>,
    /// Job map: `job_id` → per-job state. Cloned into the outcome task.
    jobs: RwLock<HashMap<String, Arc<JobEntry>>>,
    /// Monotonic tag issued for every `WorkItem` submitted to the downloader.
    /// Also serves as the routing key back to the originating article when
    /// outcomes come out the other side.
    next_tag: AtomicU64,
    /// `tag` → in-flight article metadata. We remove on outcome; this is the
    /// only place that holds the file_id / segment_number for an article
    /// mid-flight. Cleared on job cancel to free memory fast.
    in_flight: RwLock<HashMap<u64, InFlight>>,
}

/// Per-job state owned by the adapter.
struct JobEntry {
    /// Reused from the old engine — owns the assembler, progress channel,
    /// deobfuscation state, and terminal emit logic. Everything the adapter
    /// needs on the success/failure path is already a field here.
    context: Arc<JobContext>,
    /// When true, the pump task holds items in `pending` instead of
    /// forwarding them to the downloader. In-flight articles still
    /// complete — pause gates the *next* work only.
    paused: AtomicBool,
    /// Set by `cancel_job`. The pump exits and drains the queue.
    cancelled: AtomicBool,
    /// Work items waiting to be handed to the downloader. `submit_job`
    /// pushes all items here; the pump task drains on start and on
    /// resume.
    pending: Mutex<VecDeque<nzb_news::WorkItem>>,
    /// Wake-up signal for the pump task. Notified when `submit_job` adds
    /// items or when `resume_job` / `cancel_job` changes the gate.
    pump_wake: Notify,
}

/// Metadata recorded when a `WorkItem` is dispatched so we can route the
/// outcome back to the right job / file / segment.
#[derive(Clone)]
struct InFlight {
    job_id: String,
    file_id: String,
    segment_number: u32,
}

// ---------------------------------------------------------------------------
// NewsDispatchEngine
// ---------------------------------------------------------------------------

/// `DispatchEngine` impl backed by the layered nzb-news fetch engine.
pub struct NewsDispatchEngine {
    inner: Arc<Inner>,
}

impl NewsDispatchEngine {
    /// Construct the engine. Does **not** spawn the downloader —
    /// [`DispatchEngine::start`] does that.
    pub fn new(config: NewsEngineConfig) -> Self {
        Self {
            inner: Arc::new(Inner {
                config,
                handle: RwLock::new(None),
                jobs: RwLock::new(HashMap::new()),
                next_tag: AtomicU64::new(1),
                in_flight: RwLock::new(HashMap::new()),
            }),
        }
    }
}

#[async_trait::async_trait]
impl DispatchEngine for NewsDispatchEngine {
    fn start(&self) {
        let mut slot = self.inner.handle.write();
        if slot.is_some() {
            return; // idempotent
        }

        let servers_snapshot = self.inner.config.servers.lock().clone();
        if servers_snapshot.is_empty() {
            // Deferred start: with zero servers, spawning the downloader
            // would create an internal work queue with no per-server
            // workers, and any items pushed would sit in limbo.
            // `reconcile_servers` will start it once servers are added.
            info!("NewsDispatchEngine start deferred — no servers configured");
            return;
        }
        spawn_and_install_downloader(&self.inner, &mut slot, servers_snapshot);
    }

    fn submit_job(&self, job: &NzbJob, progress_tx: mpsc::Sender<ProgressUpdate>) {
        // Reuse the old engine's job-submission builder: creates the
        // FileAssembler, registers files, and filters out already-downloaded
        // articles. We only use the returned `JobContext` — the WorkItem
        // vec it produces is in the old engine's format; we build nzb-news
        // work items fresh below.
        let (ctx, legacy_items) = build_job_submission(job, progress_tx);

        // Build nzb-news wrapper types. One NzbObject for the job, one
        // NzbFile per file, and one Article per work item.
        let news_files: Vec<Arc<nzb_news::NzbFile>> = job
            .files
            .iter()
            .map(|f| {
                Arc::new(nzb_news::NzbFile::new(
                    &f.id,
                    &job.id,
                    &f.filename,
                    f.articles.len() as u32,
                ))
            })
            .collect();
        let news_files_by_id: HashMap<String, Arc<nzb_news::NzbFile>> = news_files
            .iter()
            .map(|nf| (nf.id.clone(), Arc::clone(nf)))
            .collect();
        let total_articles = legacy_items.len() as u64;
        let news_job = Arc::new(nzb_news::NzbObject::new(
            &job.id,
            &job.name,
            total_articles,
            job.total_bytes,
            news_files.clone(),
        ));

        // Convert each legacy WorkItem into an nzb-news WorkItem, recording
        // the routing metadata into in_flight and the item itself into the
        // job's pending queue. The pump task forwards from pending to the
        // downloader, gated by the `paused` flag.
        let mut pending = VecDeque::with_capacity(legacy_items.len());
        let tag_counter = &self.inner.next_tag;
        for item in legacy_items {
            let tag = tag_counter.fetch_add(1, Ordering::Relaxed);
            let file = match news_files_by_id.get(&item.file_id) {
                Some(f) => Arc::clone(f),
                None => continue, // shouldn't happen — file_id came from the same job
            };
            self.inner.in_flight.write().insert(
                tag,
                InFlight {
                    job_id: item.job_id.clone(),
                    file_id: item.file_id.clone(),
                    segment_number: item.segment_number,
                },
            );
            let article = Arc::new(nzb_news::Article::new(
                item.message_id.clone(),
                item.file_id.clone(),
                item.job_id.clone(),
                0,
                item.segment_number,
                tag,
            ));
            pending.push_back(nzb_news::WorkItem {
                tag,
                article,
                file,
                job: Arc::clone(&news_job),
            });
        }

        let entry = Arc::new(JobEntry {
            context: Arc::clone(&ctx),
            paused: AtomicBool::new(false),
            cancelled: AtomicBool::new(false),
            pending: Mutex::new(pending),
            pump_wake: Notify::new(),
        });
        self.inner
            .jobs
            .write()
            .insert(ctx.job_id.clone(), Arc::clone(&entry));

        // Spawn the pump. It acquires a sender from the engine handle on
        // each iteration and parks if the downloader is absent — so
        // submitting a job before `start()` (or during a 0-server startup
        // window) is safe: items wait in `pending` until `reconcile_servers`
        // spawns the downloader.
        let job_id = job.id.clone();
        tokio::spawn(pump_loop(entry, Arc::clone(&self.inner), job_id));
    }

    fn pause_job(&self, job_id: &str) {
        if let Some(entry) = self.inner.jobs.read().get(job_id) {
            // Local gate — stops the pump from handing new items to nzb-news.
            entry.paused.store(true, Ordering::SeqCst);
        }
        // Scheduler-level gate — holds already-submitted articles in
        // nzb-news's own pending queue. Without this, anything already
        // accepted into `work_channel_capacity` (default 4096) would still
        // route to servers despite the local gate.
        if let Some(h) = self.inner.handle.read().as_ref() {
            h.pause_job(job_id);
        }
        debug!(job_id, "paused");
    }

    fn resume_job(&self, job_id: &str) {
        if let Some(entry) = self.inner.jobs.read().get(job_id) {
            entry.paused.store(false, Ordering::SeqCst);
            entry.pump_wake.notify_waiters();
        }
        if let Some(h) = self.inner.handle.read().as_ref() {
            h.resume_job(job_id);
        }
        debug!(job_id, "resumed");
    }

    fn cancel_job(&self, job_id: &str) {
        let entry = self.inner.jobs.write().remove(job_id);
        if let Some(entry) = entry {
            // Signal pump to drain + exit.
            entry.cancelled.store(true, Ordering::SeqCst);
            entry.pump_wake.notify_waiters();
            // Drop any not-yet-dispatched items so the pump sees an empty
            // queue and exits promptly.
            entry.pending.lock().clear();
            // Clear in-flight entries for this job so stale outcomes are
            // dropped silently by the dispatcher (unknown-tag path).
            self.inner
                .in_flight
                .write()
                .retain(|_, m| m.job_id != job_id);
            // Purge nzb-news scheduler-level state: items already accepted
            // into the downloader's work_channel or pending list get emitted
            // as Cancelled outcomes and removed. Without this, a cancelled
            // job would keep routing its buffered articles to servers.
            if let Some(h) = self.inner.handle.read().as_ref() {
                h.purge_job(job_id);
            }
            debug!(job_id, "cancelled");
        }
    }

    fn abort_job(&self, job_id: &str, reason: String) -> bool {
        let entry = self.inner.jobs.read().get(job_id).cloned();
        let Some(entry) = entry else {
            return false;
        };
        {
            let mut abort_reason = entry.context.abort_reason.lock();
            if abort_reason.is_some() {
                return false;
            }
            *abort_reason = Some(reason);
        }
        entry.cancelled.store(true, Ordering::SeqCst);
        entry.context.cancelled.store(true, Ordering::SeqCst);
        entry.pump_wake.notify_waiters();

        // Pending items have never reached the downloader and can settle
        // immediately. Already-submitted items are purged below and settle
        // through Cancelled/success/failure outcomes. The final resolution
        // owns terminal emission, so assemblers cannot race post-processing.
        let pending: Vec<_> = entry.pending.lock().drain(..).collect();
        for item in pending {
            self.inner.in_flight.write().remove(&item.tag);
            entry.context.resolve_one_public();
        }
        if let Some(h) = self.inner.handle.read().as_ref() {
            h.purge_job(job_id);
        }
        if entry.context.articles_remaining.load(Ordering::SeqCst) == 0 {
            entry.context.emit_terminal_public();
        }
        true
    }

    fn has_job(&self, job_id: &str) -> bool {
        self.inner.jobs.read().contains_key(job_id)
    }

    fn reconcile_servers(&self) {
        // Rebuild the downloader with the current server list.
        //
        // First-time (0 → N): when `start` was deferred for lack of
        // servers, pump_loops parked waiting for a handle. Spawning the
        // downloader here and notifying pumps resumes dispatch cleanly —
        // no items are lost because `pump_loop` leaves unsent items in
        // `pending` until a sender is available.
        //
        // Reconfigure (N → M, N > 0): the downloader is rebuilt and the
        // old one shut down. Articles already in the old downloader's
        // internal queue that had not completed may be lost; their job
        // will stall until nzb-news grows a dynamic-server API. For the
        // common "add/edit server" UI flows this is rare in practice and
        // the user can retry a stalled job manually. Documented as a
        // limitation rather than a silent partial failure.
        let servers_snapshot = self.inner.config.servers.lock().clone();
        let server_count = servers_snapshot.len();

        let old_handle = if servers_snapshot.is_empty() {
            // Remove handle; pumps will park until a server is added.
            self.inner.handle.write().take()
        } else {
            let mut slot = self.inner.handle.write();
            let old = slot.take();
            spawn_and_install_downloader(&self.inner, &mut slot, servers_snapshot);
            old
        };

        if let Some(old) = old_handle {
            old.shutdown();
        }

        // Wake all pump loops so they re-read the handle and either pick
        // up the new sender or park on `pump_wake` until one arrives.
        let entries: Vec<Arc<JobEntry>> = self.inner.jobs.read().values().map(Arc::clone).collect();
        for entry in entries {
            entry.pump_wake.notify_waiters();
        }

        info!(
            servers = server_count,
            "NewsDispatchEngine reconciled server list"
        );
    }

    fn set_max_worker_idle(&self, _d: Duration) {
        // No per-worker idle concept in nzb-news; workers are persistent
        // until the downloader shuts down.
    }

    fn eviction_count(&self) -> u64 {
        0
    }

    fn server_stats_snapshot(&self) -> Vec<(String, crate::dispatch_engine::ServerAttemptStats)> {
        let guard = self.inner.handle.read();
        let Some(h) = guard.as_ref() else {
            return Vec::new();
        };
        h.server_stats_snapshot()
            .into_iter()
            .map(|(id, s)| {
                (
                    id,
                    crate::dispatch_engine::ServerAttemptStats {
                        attempted: s.attempted,
                        succeeded: s.succeeded,
                        not_found: s.not_found,
                        transient_failed: s.transient_failed,
                    },
                )
            })
            .collect()
    }

    async fn shutdown(&self) {
        let handle = self.inner.handle.write().take();
        if let Some(h) = handle {
            h.shutdown();
            h.join().await;
        }
    }
}

// ---------------------------------------------------------------------------
// Outcome dispatcher
// ---------------------------------------------------------------------------

/// Main loop: consume `FetchOutcome`s from nzb-news and translate each into
/// a `ProgressUpdate`, doing decode + assembly inline on success. Runs until
/// the outcome channel is closed (downloader shutdown).
async fn outcome_dispatcher(
    inner: Arc<Inner>,
    mut outcomes: mpsc::Receiver<nzb_news::FetchOutcome>,
) {
    while let Some(outcome) = outcomes.recv().await {
        match outcome {
            nzb_news::FetchOutcome::Success {
                tag,
                server_id,
                bytes,
                article_bytes: _,
            } => {
                // Spawn each success so decode+assemble runs in parallel.
                // The old engine got this for free because every worker did
                // its own fetch+decode+assemble — centralising here would
                // serialise all post-fetch work to a single task.
                let inner2 = Arc::clone(&inner);
                tokio::spawn(async move {
                    process_success(inner2, tag, server_id, bytes).await;
                });
            }
            nzb_news::FetchOutcome::Failed {
                tag,
                last_error,
                explicit_global_absence,
                explicit_not_found_servers,
            } => {
                process_failure(
                    &inner,
                    tag,
                    last_error,
                    explicit_global_absence,
                    explicit_not_found_servers,
                );
            }
            nzb_news::FetchOutcome::Cancelled { tag } => {
                if let Some(meta) = inner.in_flight.write().remove(&tag)
                    && let Some(entry) = inner.jobs.read().get(&meta.job_id).cloned()
                {
                    entry.context.resolve_one_public();
                }
            }
        }
    }
    debug!("outcome_dispatcher exiting: channel closed");
}

async fn process_success(inner: Arc<Inner>, tag: u64, server_id: String, raw: Vec<u8>) {
    let meta = inner.in_flight.write().remove(&tag);
    let Some(meta) = meta else {
        return; // stale / cancelled
    };

    let entry = inner.jobs.read().get(&meta.job_id).cloned();
    let Some(entry) = entry else {
        return; // job cancelled after submit
    };
    let ctx = &entry.context;
    if entry.cancelled.load(Ordering::SeqCst) {
        ctx.resolve_one_public();
        return;
    }

    // Decode (CPU-bound; SIMD is fast but not free).
    let decode_start = Instant::now();
    let decoded = match nzb_decode::decode_yenc(&raw) {
        Ok(d) => d,
        Err(e) => {
            let failure = ArticleFailure::decode_error(server_id, format!("yEnc decode: {e}"));
            emit_failed(ctx, &meta, failure);
            return;
        }
    };
    let decode_us = decode_start.elapsed().as_micros() as u64;

    // Record yEnc filename for deobfuscation.
    if let Some(ref fname) = decoded.filename
        && !fname.is_empty()
    {
        ctx.yenc_names
            .lock()
            .insert(meta.file_id.clone(), fname.clone());
    }

    let data_begin = decoded.part_begin.unwrap_or(0);

    // Assemble.
    let assemble_start = Instant::now();
    let file_complete = match ctx.assembler.assemble_article(
        &meta.job_id,
        &meta.file_id,
        meta.segment_number,
        data_begin,
        &decoded.data,
    ) {
        Ok(b) => b,
        Err(e) => {
            let failure = ArticleFailure::decode_error(server_id, format!("assembly: {e}"));
            emit_failed(ctx, &meta, failure);
            return;
        }
    };
    let assemble_us = assemble_start.elapsed().as_micros() as u64;

    // Timing stats.
    ctx.total_decode_us.fetch_add(decode_us, Ordering::Relaxed);
    ctx.total_assemble_us
        .fetch_add(assemble_us, Ordering::Relaxed);
    ctx.total_articles_decoded.fetch_add(1, Ordering::Relaxed);

    // Emit progress.
    let decoded_bytes = decoded.data.len() as u64;
    let _ = ctx.progress_tx.try_send(ProgressUpdate::ArticleComplete {
        job_id: meta.job_id.clone(),
        file_id: meta.file_id.clone(),
        segment_number: meta.segment_number,
        decoded_bytes,
        file_complete,
        server_id: Some(server_id),
    });

    ctx.resolve_one_public();
}

fn process_failure(
    inner: &Inner,
    tag: u64,
    last_error: Option<String>,
    explicit_global_absence: bool,
    explicit_not_found_servers: Vec<String>,
) {
    let meta = inner.in_flight.write().remove(&tag);
    let Some(meta) = meta else {
        return;
    };
    let entry = inner.jobs.read().get(&meta.job_id).cloned();
    let Some(entry) = entry else {
        return;
    };
    if entry.cancelled.load(Ordering::SeqCst) {
        entry.context.resolve_one_public();
        return;
    }
    let mut msg = last_error.unwrap_or_else(|| "all servers exhausted".into());
    if explicit_global_absence {
        msg = format!(
            "{msg}; explicit provider outcomes: {}",
            explicit_not_found_servers
                .iter()
                .map(|server| format!("{server}=not_found"))
                .collect::<Vec<_>>()
                .join(",")
        );
    }
    // nzb-news doesn't carry structured error info at the outcome layer —
    // only the last attempt's error string. Pattern-match common causes
    // so the hopeless-tracker and queue_manager can distinguish "server
    // is broken/quota-exhausted" (transient, don't count toward hopeless)
    // from "article genuinely missing everywhere" (counts toward
    // hopeless). Without this, an auth/quota failure trickles through as
    // NotFound and aborts the job with "articles confirmed missing" —
    // confusing diagnostics that blame the content instead of the server.
    let kind = if explicit_global_absence {
        ArticleFailureKind::NotFound
    } else {
        classify_error_message(&msg)
    };
    let failure = ArticleFailure {
        kind,
        server_id: String::new(),
        message: msg,
    };
    emit_failed(&entry.context, &meta, failure);
}

/// Map an opaque nzb-news error string to a typed [`ArticleFailureKind`].
///
/// The strings come from `nzb_nntp::error::NntpError` (via nzb-news) and are
/// the only signal we have at this layer — nzb-news's `FetchOutcome` carries
/// `Option<String>` rather than a structured kind. Order of checks matters:
/// more specific patterns are tested first.
fn classify_error_message(msg: &str) -> ArticleFailureKind {
    let m = msg.to_ascii_lowercase();
    // NNTP response codes in the message body are the strongest signal.
    if m.contains("(482)") || m.contains("(481)") || m.contains("auth") {
        return ArticleFailureKind::AuthFailed;
    }
    if m.contains("(403)") || m.contains("permission") || m.contains("forbidden") {
        return ArticleFailureKind::PermissionDenied;
    }
    if m.contains("(430)") || m.contains("article not found") || m.contains("no such article") {
        return ArticleFailureKind::NotFound;
    }
    if m.contains("(502)") || m.contains("service unavailable") {
        return ArticleFailureKind::ServerDown;
    }
    if m.contains("timeout") || m.contains("timed out") {
        return ArticleFailureKind::Timeout;
    }
    if m.contains("connection") || m.contains("eof") || m.contains("reset") || m.contains("closed")
    {
        return ArticleFailureKind::ConnectionClosed;
    }
    // Opaque/unknown errors carry no proof of article absence.
    ArticleFailureKind::Other
}

#[cfg(test)]
#[allow(clippy::items_after_test_module)]
mod classify_tests {
    use super::*;

    #[test]
    fn classifies_auth_failures() {
        let msg = "Authentication failed: PASS rejected (482): Your block account is fully used";
        assert_eq!(classify_error_message(msg), ArticleFailureKind::AuthFailed);
    }

    #[test]
    fn classifies_not_found() {
        let msg = "NNTP (430) No such article";
        assert_eq!(classify_error_message(msg), ArticleFailureKind::NotFound);
    }

    #[test]
    fn classifies_service_unavailable() {
        let msg = "Service unavailable (502)";
        assert_eq!(classify_error_message(msg), ArticleFailureKind::ServerDown);
    }

    #[test]
    fn classifies_timeout() {
        let msg = "read timed out after 60s";
        assert_eq!(classify_error_message(msg), ArticleFailureKind::Timeout);
    }

    #[test]
    fn unknown_defaults_to_other() {
        assert_eq!(
            classify_error_message("all servers exhausted"),
            ArticleFailureKind::Other
        );
    }
}

fn emit_failed(ctx: &JobContext, meta: &InFlight, failure: ArticleFailure) {
    ctx.articles_failed.fetch_add(1, Ordering::Relaxed);
    let _ = ctx.progress_tx.try_send(ProgressUpdate::ArticleFailed {
        job_id: meta.job_id.clone(),
        file_id: meta.file_id.clone(),
        segment_number: meta.segment_number,
        failure,
    });
    ctx.resolve_one_public();
}

// ---------------------------------------------------------------------------
// Per-job pump task
// ---------------------------------------------------------------------------

/// Drains a job's `pending` queue into the downloader's work channel,
/// respecting the `paused` gate and exiting on `cancelled`.
///
/// The pump parks on `pump_wake` when `pending` is empty or when
/// `paused` is true. `submit_job` / `resume_job` notify to wake it.
async fn pump_loop(entry: Arc<JobEntry>, inner: Arc<Inner>, job_id: String) {
    loop {
        if entry.cancelled.load(Ordering::SeqCst) {
            debug!(job_id, "pump exiting: cancelled");
            return;
        }
        if entry.paused.load(Ordering::SeqCst) {
            entry.pump_wake.notified().await;
            continue;
        }
        let next = entry.pending.lock().pop_front();
        let Some(item) = next else {
            // Queue empty. submit_job enqueues every article up-front, so
            // an empty queue means we're done. Park anyway so cancel can
            // wake us.
            entry.pump_wake.notified().await;
            continue;
        };

        // Snapshot the current sender. If the downloader is absent (not
        // started yet, or torn down during reconcile_servers with zero
        // servers), stash the item back on the front of `pending` and
        // park; reconcile_servers will notify us when a new handle exists.
        let sender = inner.handle.read().as_ref().map(|h| h.sender());
        let Some(sender) = sender else {
            entry.pending.lock().push_front(item);
            entry.pump_wake.notified().await;
            continue;
        };

        // Send. On SendError (sender closed mid-reconcile), return the
        // item to the queue and park — the new handle is on its way.
        if let Err(e) = sender.send(item).await {
            entry.pending.lock().push_front(e.0);
            entry.pump_wake.notified().await;
            continue;
        }
    }
}

/// Build a new `DownloaderConfig` from the engine's static knobs plus the
/// given server list, spawn the downloader, install its handle in `slot`,
/// and launch the outcome dispatcher task. Used by both `start()` and
/// `reconcile_servers` to avoid duplicating the construction.
///
/// Precondition: `servers` is non-empty; caller decides the zero-server
/// policy. `slot` must already be held under a write lock.
fn spawn_and_install_downloader(
    inner: &Arc<Inner>,
    slot: &mut Option<nzb_news::DownloaderHandle>,
    servers: Vec<ServerConfig>,
) {
    let cfg = &inner.config;
    let server_count = servers.len();
    let dl_config = nzb_news::DownloaderConfig {
        servers,
        max_concurrent_fetches: cfg.max_concurrent_fetches,
        article_timeout: cfg.article_timeout,
        work_channel_capacity: cfg.work_channel_capacity,
        outcome_channel_capacity: cfg.outcome_channel_capacity,
        probe_policy: cfg.probe_policy.clone(),
    };
    let (handle, outcomes) = nzb_news::spawn_downloader(dl_config);
    let inner_for_task = Arc::clone(inner);
    tokio::spawn(outcome_dispatcher(inner_for_task, outcomes));
    *slot = Some(handle);
    info!(
        servers = server_count,
        "NewsDispatchEngine downloader spawned"
    );
}
