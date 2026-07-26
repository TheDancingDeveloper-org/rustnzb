//! Integration test harness for `nzb-web`.
//!
//! Spawns the **real** `QueueManager`, `WorkerPool`, `SharedWorkQueue`, and
//! supervisor against one or more `MockNntpServer` instances. Lets tests
//! reproduce the exact code paths production runs, but with controlled fault
//! injection at the NNTP boundary (silent socket close, hang on ARTICLE,
//! per-message-id response codes, etc.).
//!
//! See `tests/harness/nzb_fixture.rs` for the NZB XML + yEnc fixture builder.
//!
//! Two test files consume this module:
//! - `harness_smoke.rs` — happy-path smoke test (Phase 2 deliverable).
//! - `harness_zombie.rs` — canary test for the recurring zombie-worker bug
//!   (#[ignore]'d until Phase 5 lands the idle-watchdog fix).

#![allow(dead_code)] // Different test files use different parts of the harness.

pub mod nzb_fixture;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use nzb_nntp::testutil::{MockConfig, MockNntpServer, test_config};
use nzb_web::log_buffer::LogBuffer;
use nzb_web::nzb_core::config::ServerConfig;
use nzb_web::nzb_core::db::Database;
use nzb_web::nzb_core::models::{JobStatus, NzbJob};
use nzb_web::nzb_core::nzb_parser;
use nzb_web::queue_manager::QueueManager;

use tempfile::TempDir;

/// One-shot installation of a `tracing` subscriber so test logs from the
/// worker pool / queue manager are visible under `RUST_LOG=...`. Cargo runs
/// tests in parallel inside one process; we install once and ignore the
/// "already set" error from subsequent calls.
pub fn init_test_tracing() {
    use std::sync::Once;
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
            )
            .with_test_writer()
            .try_init();
    });
}

// ---------------------------------------------------------------------------
// Server profile — one mock NNTP server + the ServerConfig that points at it
// ---------------------------------------------------------------------------

/// A single mock NNTP server bundled with the `ServerConfig` clients use to
/// reach it. Held by the harness so the server stays alive for the test's
/// lifetime.
pub struct ServerProfile {
    pub server: MockNntpServer,
    pub config: ServerConfig,
}

impl ServerProfile {
    pub async fn start(id: &str, mock_config: MockConfig, max_connections: u16) -> Self {
        let server = MockNntpServer::start(mock_config).await;
        // `test_config` is the only public constructor for ServerConfig
        // outside nzb-nntp (the struct is `#[non_exhaustive]`).
        let mut config = test_config(server.port());
        config.id = id.into();
        config.name = format!("mock-{id}");
        config.connections = max_connections;
        Self { server, config }
    }

    /// Set the server's dispatch priority (0 = highest). Used by priority
    /// integration tests to build primary + backup server topologies.
    pub fn with_priority(mut self, priority: u8) -> Self {
        self.config.priority = priority;
        self
    }
}

// ---------------------------------------------------------------------------
// HarnessBuilder
// ---------------------------------------------------------------------------

/// Constructs a `TestEngine` for an integration test. Each builder method is
/// independently optional; defaults are tuned for fast, deterministic tests.
pub struct HarnessBuilder {
    servers: Vec<ServerProfile>,
    article_timeout_secs: u64,
    max_active_downloads: usize,
    abort_hopeless: bool,
    early_failure_check: bool,
    required_completion_pct: f64,
    speed_limit_bps: u64,
}

impl HarnessBuilder {
    pub fn new() -> Self {
        Self {
            servers: Vec::new(),
            article_timeout_secs: 30,
            max_active_downloads: 5,
            abort_hopeless: true,
            early_failure_check: true,
            required_completion_pct: 100.0,
            speed_limit_bps: 0,
        }
    }

    pub fn with_server(mut self, profile: ServerProfile) -> Self {
        self.servers.push(profile);
        self
    }

    pub fn article_timeout(mut self, secs: u64) -> Self {
        self.article_timeout_secs = secs;
        self
    }

    pub fn max_active_downloads(mut self, n: usize) -> Self {
        self.max_active_downloads = n;
        self
    }

    pub fn abort_hopeless(mut self, abort: bool) -> Self {
        self.abort_hopeless = abort;
        self
    }

    /// Build the engine. Creates temp dirs, an in-memory database, and a
    /// fully-wired `QueueManager` whose worker pool is already running.
    pub fn build(self) -> TestEngine {
        init_test_tracing();
        let tempdir = TempDir::new().expect("create tempdir");
        let incomplete_dir = tempdir.path().join("incomplete");
        let complete_dir = tempdir.path().join("complete");
        std::fs::create_dir_all(&incomplete_dir).expect("create incomplete_dir");
        std::fs::create_dir_all(&complete_dir).expect("create complete_dir");

        let db = Database::open_memory().expect("open in-memory db");
        let server_configs: Vec<ServerConfig> =
            self.servers.iter().map(|p| p.config.clone()).collect();

        let queue_manager = QueueManager::new(
            server_configs,
            db,
            incomplete_dir.clone(),
            complete_dir.clone(),
            LogBuffer::default(),
            self.max_active_downloads,
            Vec::new(), // categories
            0,          // min_free_space
            self.speed_limit_bps,
            false, // direct_unpack
            5,     // max_nested_archive_depth
            self.abort_hopeless,
            self.early_failure_check,
            self.required_completion_pct,
            self.article_timeout_secs,
        );

        queue_manager.spawn_speed_tracker();

        TestEngine {
            queue_manager,
            _servers: self.servers,
            _tempdir: tempdir,
            incomplete_dir,
            complete_dir,
        }
    }
}

impl Default for HarnessBuilder {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// TestEngine — the live handle a test interacts with
// ---------------------------------------------------------------------------

/// A running test engine: real `QueueManager` + worker pool + the mock NNTP
/// servers it talks to. Drop kills the mocks and tears down the temp dirs.
pub struct TestEngine {
    pub queue_manager: Arc<QueueManager>,
    _servers: Vec<ServerProfile>,
    _tempdir: TempDir,
    pub incomplete_dir: PathBuf,
    pub complete_dir: PathBuf,
}

impl TestEngine {
    /// Submit raw NZB XML bytes as a new job. Returns the assigned job ID.
    pub fn submit_nzb_xml(&self, name: &str, xml: Vec<u8>) -> anyhow::Result<String> {
        let mut job: NzbJob = nzb_parser::parse_nzb(name, &xml)?;
        // The parser leaves work_dir/output_dir as empty PathBuf — production
        // callers (rustnzbd / Arz) fill these in based on app config. For
        // tests we anchor them under the harness's TempDir so the assembler
        // writes its output files into a sandbox that gets cleaned up on
        // drop, instead of leaking into nzb-web's source tree.
        job.work_dir = self.incomplete_dir.join(&job.id);
        job.output_dir = self.complete_dir.join(&job.id);
        let id = job.id.clone();
        self.queue_manager.add_job(job, Some(xml))?;
        Ok(id)
    }

    /// A snapshot of the current job state. Cheap to call; takes per-job
    /// locks briefly to read but doesn't hold them.
    pub fn snapshot(&self) -> Snapshot {
        let jobs: Vec<JobView> = self
            .queue_manager
            .get_jobs()
            .into_iter()
            .map(JobView::from)
            .collect();
        Snapshot { jobs }
    }

    /// Look up a single job by id. None if the job is not in the active
    /// queue (e.g. moved to history).
    pub fn job(&self, id: &str) -> Option<JobView> {
        self.snapshot().jobs.into_iter().find(|j| j.id == id)
    }

    /// Poll `predicate` against fresh snapshots until it returns `true` or
    /// the timeout elapses. Returns `true` on success. Polls every 100 ms.
    pub async fn wait_for<F>(&self, timeout: Duration, mut predicate: F) -> bool
    where
        F: FnMut(&Snapshot) -> bool,
    {
        let deadline = Instant::now() + timeout;
        loop {
            let snap = self.snapshot();
            if predicate(&snap) {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// Wait for a specific job to reach any of the given statuses.
    pub async fn wait_for_status(
        &self,
        job_id: &str,
        timeout: Duration,
        statuses: &[JobStatus],
    ) -> bool {
        self.wait_for(timeout, |s| {
            s.jobs
                .iter()
                .find(|j| j.id == job_id)
                .map(|j| statuses.contains(&j.status))
                .unwrap_or(false)
        })
        .await
    }
}

// ---------------------------------------------------------------------------
// Snapshot view types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Snapshot {
    pub jobs: Vec<JobView>,
}

impl Snapshot {
    pub fn job(&self, id: &str) -> Option<&JobView> {
        self.jobs.iter().find(|j| j.id == id)
    }
}

#[derive(Debug, Clone)]
pub struct JobView {
    pub id: String,
    pub name: String,
    pub status: JobStatus,
    pub article_count: usize,
    pub articles_downloaded: usize,
    pub articles_failed: usize,
    pub downloaded_bytes: u64,
    pub total_bytes: u64,
}

impl From<NzbJob> for JobView {
    fn from(j: NzbJob) -> Self {
        Self {
            id: j.id,
            name: j.name,
            status: j.status,
            article_count: j.article_count,
            articles_downloaded: j.articles_downloaded,
            articles_failed: j.articles_failed,
            downloaded_bytes: j.downloaded_bytes,
            total_bytes: j.total_bytes,
        }
    }
}

// ---------------------------------------------------------------------------
// Convenience: bundle yEnc-encoded fixture articles into a MockConfig.articles
// ---------------------------------------------------------------------------

/// Build a `MockConfig::articles` map from a list of `(message_id, body)`
/// pairs by yEnc-encoding each body so the production decoder can parse it.
pub fn yenc_articles(articles: &[(&str, &[u8], &str)]) -> HashMap<String, Vec<u8>> {
    let mut out = HashMap::new();
    for &(msg_id, body, filename) in articles {
        let (encoded, _crc) = yenc_simd::encode_article(body, filename, 1, 1, 0, body.len() as u64);
        out.insert(msg_id.to_string(), encoded);
    }
    out
}
