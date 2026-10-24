//! The downloader and WebDAV share one connection budget per server. These
//! tests run the real worker pool (long-lived workers that take their budget
//! permits at startup) next to budgeted WebDAV pools, against mock servers
//! that count connections, and check the lifetimes the budget must honour:
//! idle workers hand their permits on, a socket that no waiter can use is
//! kept for reuse, and import and playback progress while downloads saturate.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use nzb_core::config::ServerConfig;
use nzb_core::models::{JobStatus, NzbFile, NzbJob, Priority};
use nzb_dispatch::bandwidth::{BandwidthConfig, BandwidthLimiter};
use nzb_dispatch::{DispatchEngine, DispatchHandle, ProgressUpdate};
use nzb_nntp::budget::{BudgetPolicy, BudgetRegistry, BudgetTier, ServerBudget};
use nzb_nntp::testutil::{MockConfig, MockNntpServer, test_config};
use nzb_nntp::{Article, ConnectionPool};
use parking_lot::Mutex;
use tokio::sync::mpsc;
use tokio::time::Instant;

const WAIT: Duration = Duration::from_secs(10);
/// One read must never come near the pool's 30 s acquire timeout.
const READ_WAIT: Duration = Duration::from_secs(5);
const MESSAGE_ID: &str = "msg@test";

fn encoded_payload() -> Vec<u8> {
    yenc_simd::encode_article(b"payload", "out.bin", 1, 1, 0, 7).0
}

/// A four-connection server entry at `port`, id "srv".
fn server(port: u16) -> ServerConfig {
    let mut config = test_config(port);
    config.id = "srv".into();
    config.name = "srv".into();
    config.connections = 4;
    config
}

/// Budgets with WebDAV enabled at the default 25% reserve (1 of 4).
fn dav_budgets() -> Arc<BudgetRegistry> {
    Arc::new(BudgetRegistry::new(BudgetPolicy {
        playback_reserve_pct: 25,
        repair_reserve_pct: 0,
    }))
}

fn downloader(config: &ServerConfig, budgets: &Arc<BudgetRegistry>) -> DispatchHandle {
    DispatchHandle::with_budgets(
        Arc::new(Mutex::new(vec![config.clone()])),
        Arc::new(BandwidthLimiter::new(BandwidthConfig::default())),
        0,
        Arc::clone(budgets),
    )
}

fn dav_pool(
    config: &ServerConfig,
    budget: &Arc<ServerBudget>,
    tier: BudgetTier,
) -> Arc<ConnectionPool> {
    Arc::new(ConnectionPool::with_budget(
        Arc::new(config.clone()),
        Arc::clone(budget),
        tier,
    ))
}

fn job(job_id: &str, root: &Path, message_ids: &[String]) -> NzbJob {
    let articles = message_ids
        .iter()
        .enumerate()
        .map(|(i, id)| Article {
            message_id: id.clone(),
            segment_number: i as u32 + 1,
            bytes: 7,
            downloaded: false,
            data_begin: None,
            data_size: None,
            crc32: None,
            tried_servers: Vec::new(),
            tries: 0,
        })
        .collect::<Vec<_>>();
    NzbJob {
        slice_map: None,
        held_par2_bytes: 0,
        id: job_id.into(),
        name: job_id.into(),
        category: "Default".into(),
        status: JobStatus::Downloading,
        priority: Priority::Normal,
        total_bytes: 7 * articles.len() as u64,
        downloaded_bytes: 0,
        file_count: 1,
        files_completed: 0,
        article_count: articles.len(),
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
        files: vec![NzbFile {
            id: "f1".into(),
            filename: "out.bin".into(),
            bytes: 7 * articles.len() as u64,
            bytes_downloaded: 0,
            is_par2: false,
            par2_setname: None,
            par2_vol: None,
            par2_blocks: None,
            assembled: false,
            groups: vec!["alt.binaries.test".into()],
            articles,
        }],
    }
}

async fn wait_until(what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + WAIT;
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting until {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Track the most permits ever held at once on `budget`.
fn watch_peak(budget: &Arc<ServerBudget>) -> (Arc<AtomicUsize>, Arc<AtomicBool>) {
    let peak = Arc::new(AtomicUsize::new(0));
    let stop = Arc::new(AtomicBool::new(false));
    tokio::spawn({
        let (budget, peak, stop) = (Arc::clone(budget), Arc::clone(&peak), Arc::clone(&stop));
        async move {
            while !stop.load(Ordering::Relaxed) {
                peak.fetch_max(budget.held_total(), Ordering::Relaxed);
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        }
    });
    (peak, stop)
}

/// `n` reads through `pool`, each a full acquire and release, as
/// nzbdav-stream does per article.
async fn reads(pool: &ConnectionPool, n: usize, what: &str) {
    for i in 0..n {
        let conn = tokio::time::timeout(READ_WAIT, pool.acquire())
            .await
            .unwrap_or_else(|_| panic!("{what} read {i} starved"))
            .unwrap_or_else(|e| panic!("{what} read {i} failed: {e}"));
        pool.release(conn);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn idle_downloader_lends_playback_more_than_its_reserve() {
    let mock = MockNntpServer::start(MockConfig::default()).await;
    let config = server(mock.port());
    let budgets = dav_budgets();
    let pool = downloader(&config, &budgets);
    pool.start();
    let budget = budgets.get("srv").unwrap();
    // The workers are long-lived and take what they may at startup: three
    // of four, one kept for WebDAV.
    wait_until("the workers hold their permits", || {
        budget.held(BudgetTier::Download) == 3
    })
    .await;

    // The queue is empty, so playback may borrow the whole allowance.
    let dav = dav_pool(&config, &budget, BudgetTier::Playback);
    let mut conns = Vec::new();
    for i in 0..4 {
        let conn = tokio::time::timeout(READ_WAIT, dav.acquire())
            .await
            .unwrap_or_else(|_| panic!("playback connection {i} starved behind an idle downloader"))
            .unwrap();
        conns.push(conn);
    }
    assert_eq!(budget.held(BudgetTier::Playback), 4);
    assert_eq!(budget.held_total(), 4);
    for conn in conns {
        dav.release(conn);
    }
    pool.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn import_and_playback_progress_and_reuse_sockets_while_downloads_saturate() {
    // Every download hangs mid-fetch, so the workers stay busy and hold
    // their permits for the whole test.
    let slow = MockNntpServer::start(MockConfig {
        hang_after_command: Some("ARTICLE".into()),
        ..MockConfig::default()
    })
    .await;
    let dav_mock = MockNntpServer::start(MockConfig::default()).await;
    let config = server(slow.port());
    let budgets = dav_budgets();
    let pool = downloader(&config, &budgets);
    pool.start();
    let budget = budgets.get("srv").unwrap();
    let (peak, stop) = watch_peak(&budget);

    let dir = tempfile::tempdir().unwrap();
    let ids: Vec<String> = (0..8).map(|i| format!("a{i}@test")).collect();
    let (tx, _rx) = mpsc::channel(64);
    pool.submit_job(&job("busy", dir.path(), &ids), tx);
    wait_until("three workers are fetching", || {
        budget.held(BudgetTier::Download) == 3
            && pool.active_connection_snapshot() == vec![("srv".into(), 3, 4)]
    })
    .await;

    // WebDAV talks to the same provider account (same server id, same
    // budget); its own mock counts the sockets it opens.
    let mut dav_config = config.clone();
    dav_config.port = dav_mock.port();
    let import = dav_pool(&dav_config, &budget, BudgetTier::Import);
    let playback = dav_pool(&dav_config, &budget, BudgetTier::Playback);
    let managers = [Arc::clone(&import), Arc::clone(&playback)]
        .map(|p| tokio::spawn(async move { p.manage_idle(Duration::from_secs(60)).await }));

    // A fourth worker is queued for a download permit the whole time.
    // Neither import nor playback reconnects for it: it could not take the
    // WebDAV floor anyway.
    reads(&import, 20, "import").await;
    assert_eq!(
        dav_mock.accepted_connections(),
        1,
        "import reconnected per read"
    );

    // Playback takes the floor over from import's parked socket, then
    // reuses its own.
    reads(&playback, 20, "playback").await;
    assert_eq!(
        dav_mock.accepted_connections(),
        2,
        "40 reads must log in once per tier, not once per read"
    );

    // Downloads were never preempted, and the provider never saw more than
    // its allowance.
    assert_eq!(budget.held(BudgetTier::Download), 3);
    stop.store(true, Ordering::Relaxed);
    assert!(peak.load(Ordering::Relaxed) <= 4, "budget exceeded");
    for manager in managers {
        manager.abort();
    }
    pool.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_playback_readers_take_turns_on_the_floor_while_downloads_saturate() {
    let slow = MockNntpServer::start(MockConfig {
        hang_after_command: Some("ARTICLE".into()),
        ..MockConfig::default()
    })
    .await;
    let dav_mock = MockNntpServer::start(MockConfig::default()).await;
    let config = server(slow.port());
    let budgets = dav_budgets();
    let pool = downloader(&config, &budgets);
    pool.start();
    let budget = budgets.get("srv").unwrap();
    let (peak, stop) = watch_peak(&budget);

    let dir = tempfile::tempdir().unwrap();
    let ids: Vec<String> = (0..8).map(|i| format!("a{i}@test")).collect();
    let (tx, _rx) = mpsc::channel(64);
    pool.submit_job(&job("busy", dir.path(), &ids), tx);
    wait_until("three workers are fetching", || {
        budget.held(BudgetTier::Download) == 3
            && pool.active_connection_snapshot() == vec![("srv".into(), 3, 4)]
    })
    .await;

    let mut dav_config = config.clone();
    dav_config.port = dav_mock.port();
    let playback = dav_pool(&dav_config, &budget, BudgetTier::Playback);
    let manager = tokio::spawn({
        let playback = Arc::clone(&playback);
        async move { playback.manage_idle(Duration::from_secs(60)).await }
    });

    // Several segments read at once through one pool, with only the
    // one-permit floor free: each reader picks up the socket the previous
    // one parked instead of waiting for a busy download.
    let readers: Vec<_> = (0..4)
        .map(|r| {
            let playback = Arc::clone(&playback);
            tokio::spawn(async move { reads(&playback, 10, &format!("playback {r}")).await })
        })
        .collect();
    for reader in readers {
        reader.await.unwrap();
    }
    assert_eq!(
        dav_mock.accepted_connections(),
        1,
        "concurrent readers must share the parked socket, not reconnect"
    );
    assert_eq!(budget.held(BudgetTier::Download), 3);
    stop.store(true, Ordering::Relaxed);
    assert!(peak.load(Ordering::Relaxed) <= 4, "budget exceeded");
    manager.abort();
    pool.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn without_webdav_idle_workers_close_their_sockets() {
    let mock = MockNntpServer::start(MockConfig::default()).await;
    let config = server(mock.port());
    // No WebDAV: no reserve and no pool sharing the budget.
    let budgets = Arc::new(BudgetRegistry::default());
    let pool = downloader(&config, &budgets);
    pool.set_idle_release(Duration::from_millis(300));
    pool.start();
    let budget = budgets.get("srv").unwrap();
    wait_until("every worker is connected", || {
        pool.connection_snapshot() == vec![("srv".into(), 4, 4)]
    })
    .await;

    // With nothing queued, every worker closes its socket after the release
    // delay, so the pool panel does not show idle sockets as connected.
    let all_released =
        || budget.held_total() == 0 && pool.connection_snapshot() == vec![("srv".into(), 0, 4)];
    wait_until("idle workers closed every socket", all_released).await;
    let logins = mock.accepted_connections();

    // They stay closed while there is still nothing to fetch.
    tokio::time::sleep(Duration::from_millis(1000)).await;
    assert!(all_released(), "an idle worker reopened a socket");
    assert_eq!(
        mock.accepted_connections(),
        logins,
        "an idle worker reconnected with nothing to fetch"
    );
    pool.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn idle_workers_give_their_permits_back_and_reconnect_for_work() {
    let mock = MockNntpServer::start(MockConfig {
        articles: HashMap::from([(MESSAGE_ID.to_string(), encoded_payload())]),
        ..MockConfig::default()
    })
    .await;
    let config = server(mock.port());
    let budgets = dav_budgets();
    let pool = downloader(&config, &budgets);
    pool.set_idle_release(Duration::from_millis(200));
    pool.start();
    let budget = budgets.get("srv").unwrap();
    // WebDAV is on: its pool shares the budget.
    let _dav = dav_pool(&config, &budget, BudgetTier::Playback);

    // With nothing queued, the workers close their sockets and hold none
    // of the allowance. The fourth worker, queued at startup, may get a
    // permit as the others give theirs back; it gives it back in turn.
    let all_released =
        || budget.held_total() == 0 && pool.connection_snapshot() == vec![("srv".into(), 0, 4)];
    wait_until("idle workers released every permit", all_released).await;
    tokio::time::sleep(Duration::from_millis(600)).await;
    wait_until("idle workers released every permit", all_released).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(budget.held_total(), 0, "an idle worker took a permit back");

    // New work: a worker takes a permit again, reconnects and downloads.
    let dir = tempfile::tempdir().unwrap();
    let (tx, mut rx) = mpsc::channel(64);
    pool.submit_job(&job("job", dir.path(), &[MESSAGE_ID.to_string()]), tx);
    let deadline = Instant::now() + WAIT;
    loop {
        let event = tokio::time::timeout_at(deadline, rx.recv())
            .await
            .expect("no progress after the idle release")
            .expect("progress channel closed");
        if matches!(event, ProgressUpdate::ArticleComplete { .. }) {
            break;
        }
    }
    pool.shutdown().await;
}
