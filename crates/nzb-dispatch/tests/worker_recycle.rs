//! Live server-config edits reach the worker pool through the public
//! `DispatchEngine` surface. Each test runs real jobs against the mock NNTP
//! server, edits the server list while the pool is up, and checks what the
//! workers do afterwards.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use nzb_core::config::ServerConfig;
use nzb_core::models::{JobStatus, NzbFile, NzbJob, Priority};
use nzb_dispatch::bandwidth::{BandwidthConfig, BandwidthLimiter};
use nzb_dispatch::{DispatchEngine, DispatchHandle, ProgressUpdate};
use nzb_nntp::Article;
use nzb_nntp::testutil::{MockConfig, MockNntpServer, test_config};
use parking_lot::Mutex;
use tokio::sync::mpsc;
use tokio::time::Instant;

const EVENT_TIMEOUT: Duration = Duration::from_secs(15);
const MESSAGE_ID: &str = "msg@test";

fn encoded_payload() -> Vec<u8> {
    yenc_simd::encode_article(b"payload", "out.bin", 1, 1, 0, 7).0
}

/// A server entry pointing at `port`, with one connection and the given id.
fn server(port: u16) -> ServerConfig {
    let mut config = test_config(port);
    config.id = "srv".into();
    config.name = "srv".into();
    config.connections = 1;
    config
}

fn pool_with(servers: Vec<ServerConfig>) -> DispatchHandle {
    DispatchHandle::new(
        Arc::new(Mutex::new(servers)),
        Arc::new(BandwidthLimiter::new(BandwidthConfig::default())),
        0,
    )
}

/// One job with a single article, written to `root/out.bin`.
fn one_article_job(job_id: &str, root: &Path) -> NzbJob {
    let article = Article {
        message_id: MESSAGE_ID.into(),
        segment_number: 1,
        bytes: 7,
        downloaded: false,
        data_begin: None,
        data_size: None,
        crc32: None,
        tried_servers: Vec::new(),
        tries: 0,
    };
    NzbJob {
        slice_map: None,
        held_par2_bytes: 0,
        id: job_id.into(),
        name: job_id.into(),
        category: "Default".into(),
        status: JobStatus::Downloading,
        priority: Priority::Normal,
        total_bytes: 7,
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
        files: vec![NzbFile {
            id: "f1".into(),
            filename: "out.bin".into(),
            bytes: 7,
            bytes_downloaded: 0,
            is_par2: false,
            par2_setname: None,
            par2_vol: None,
            par2_blocks: None,
            assembled: false,
            groups: vec!["alt.binaries.test".into()],
            articles: vec![article],
        }],
    }
}

/// Wait for the first progress event matching `want`, skipping the rest.
async fn wait_for(
    rx: &mut mpsc::Receiver<ProgressUpdate>,
    mut want: impl FnMut(&ProgressUpdate) -> bool,
) -> ProgressUpdate {
    let deadline = Instant::now() + EVENT_TIMEOUT;
    loop {
        let event = tokio::time::timeout_at(deadline, rx.recv())
            .await
            .expect("timed out waiting for a progress event")
            .expect("progress channel closed before the event arrived");
        if want(&event) {
            return event;
        }
    }
}

/// Submit a one-article job and return the first event that says the pool
/// either delivered the article or reported that its providers are down.
async fn submit_and_observe(pool: &DispatchHandle, job: &NzbJob) -> ProgressUpdate {
    let (tx, mut rx) = mpsc::channel(64);
    pool.submit_job(job, tx);
    wait_for(&mut rx, |event| {
        matches!(
            event,
            ProgressUpdate::ArticleComplete { .. } | ProgressUpdate::WaitingForProviders { .. }
        )
    })
    .await
}

/// Mock that accepts only `reader`/`right`, and serves one article.
async fn authed_mock() -> MockNntpServer {
    MockNntpServer::start(MockConfig {
        articles: HashMap::from([(MESSAGE_ID.to_string(), encoded_payload())]),
        auth_required: true,
        valid_credentials: Some(("reader".into(), "right".into())),
        ..MockConfig::default()
    })
    .await
}

#[tokio::test]
async fn password_edit_while_live_recycles_workers_onto_new_credentials() {
    let mock = authed_mock().await;
    let mut good = server(mock.port());
    good.username = Some("reader".into());
    good.password = Some("right".into());
    let pool = pool_with(vec![good.clone()]);
    pool.start();

    let first = tempfile::tempdir().unwrap();
    let delivered = submit_and_observe(&pool, &one_article_job("job-a", first.path())).await;
    assert!(
        matches!(delivered, ProgressUpdate::ArticleComplete { .. }),
        "the article must download before the edit, got {delivered:?}"
    );

    // The old workers authenticated with "right". After the edit they must be
    // replaced by workers that present the new password, which the mock
    // rejects. The outage report is the proof that the replacements used it.
    let mut rotated = good;
    rotated.password = Some("rotated-wrong".into());
    pool.update_servers(vec![rotated]);

    let second = tempfile::tempdir().unwrap();
    let after_edit = submit_and_observe(&pool, &one_article_job("job-b", second.path())).await;
    assert!(
        matches!(after_edit, ProgressUpdate::WaitingForProviders { .. }),
        "workers must present the edited password, got {after_edit:?}"
    );

    pool.shutdown().await;
}

#[tokio::test]
async fn unchanged_server_config_keeps_serving_jobs() {
    // Control for the password test: the same live update path with no edit
    // must leave the workers able to deliver.
    let mock = authed_mock().await;
    let mut good = server(mock.port());
    good.username = Some("reader".into());
    good.password = Some("right".into());
    let pool = pool_with(vec![good.clone()]);
    pool.start();

    let first = tempfile::tempdir().unwrap();
    submit_and_observe(&pool, &one_article_job("job-a", first.path())).await;

    pool.update_servers(vec![good]);

    let second = tempfile::tempdir().unwrap();
    let after_update = submit_and_observe(&pool, &one_article_job("job-b", second.path())).await;
    assert!(
        matches!(after_update, ProgressUpdate::ArticleComplete { .. }),
        "an identical config must keep serving, got {after_update:?}"
    );

    pool.shutdown().await;
}

#[tokio::test]
async fn tls_flip_while_live_recycles_workers_onto_tls() {
    // Plain-TCP mock. Switching the live server to TLS must replace the
    // workers with ones that try a TLS handshake, which this mock cannot
    // answer, so the article stops being delivered.
    let mock = MockNntpServer::start(MockConfig {
        articles: HashMap::from([(MESSAGE_ID.to_string(), encoded_payload())]),
        ..MockConfig::default()
    })
    .await;
    let plain = server(mock.port());
    let pool = pool_with(vec![plain.clone()]);
    pool.start();

    let first = tempfile::tempdir().unwrap();
    let delivered = submit_and_observe(&pool, &one_article_job("job-a", first.path())).await;
    assert!(matches!(delivered, ProgressUpdate::ArticleComplete { .. }));

    let mut tls = plain;
    tls.ssl = true;
    pool.update_servers(vec![tls]);

    let second = tempfile::tempdir().unwrap();
    let after_edit = submit_and_observe(&pool, &one_article_job("job-b", second.path())).await;
    assert!(
        matches!(after_edit, ProgressUpdate::WaitingForProviders { .. }),
        "workers must attempt TLS after the edit, got {after_edit:?}"
    );

    pool.shutdown().await;
}

#[tokio::test]
async fn raising_connection_count_live_adds_concurrent_transfers() {
    // Each response is delayed, so a worker stays busy long enough for a
    // second and third connection to be seen transferring at the same time.
    // Six 7-byte parts of one 42-byte file, so the file is complete only when
    // every article has been written at its own offset.
    let articles: HashMap<String, Vec<u8>> = (1..=6)
        .map(|n| {
            let offset = u64::from(n - 1) * 7;
            let (encoded, _) = yenc_simd::encode_article(b"payload", "out.bin", n, 6, offset, 42);
            (format!("msg{n}@test"), encoded)
        })
        .collect();
    let mock = MockNntpServer::start(MockConfig {
        articles,
        response_delay: Some(Duration::from_millis(200)),
        ..MockConfig::default()
    })
    .await;
    let pool = pool_with(vec![server(mock.port())]);
    pool.start();

    let root = tempfile::tempdir().unwrap();
    let mut job = one_article_job("job-a", root.path());
    let template = job.files[0].articles[0].clone();
    job.files[0].articles = (1..=6)
        .map(|n| Article {
            message_id: format!("msg{n}@test"),
            segment_number: n,
            ..template.clone()
        })
        .collect();
    job.files[0].bytes = 42;
    job.article_count = 6;

    let (tx, mut rx) = mpsc::channel(64);
    pool.submit_job(&job, tx);

    let mut raised = server(mock.port());
    raised.connections = 3;
    pool.update_servers(vec![raised]);

    let deadline = Instant::now() + EVENT_TIMEOUT;
    let mut peak = 0;
    while peak < 2 && Instant::now() < deadline {
        let active = pool
            .active_connection_snapshot()
            .into_iter()
            .find(|(id, _, _)| id == "srv")
            .map_or(0, |(_, active, _)| active);
        peak = peak.max(active);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        peak >= 2,
        "raising connections live must add concurrent transfers, peak was {peak}"
    );

    let finished = wait_for(&mut rx, |event| {
        matches!(event, ProgressUpdate::JobFinished { .. })
    })
    .await;
    assert!(matches!(
        finished,
        ProgressUpdate::JobFinished { success: true, .. }
    ));
    assert_eq!(
        std::fs::read(root.path().join("out.bin")).unwrap(),
        b"payload".repeat(6)
    );

    pool.shutdown().await;
}

#[tokio::test]
async fn idle_sockets_closed_by_the_server_leave_the_open_count() {
    // The provider recycles sessions after a few commands, so the idle
    // keepalive (or the article fetch itself) runs into a closed socket. The
    // queue is then empty, so the worker must report the socket gone and
    // not reconnect.
    let mock = MockNntpServer::start(MockConfig {
        articles: HashMap::from([(MESSAGE_ID.to_string(), encoded_payload())]),
        close_after_n_commands: Some(4),
        ..MockConfig::default()
    })
    .await;
    let pool = pool_with(vec![server(mock.port())]);
    let keepalive = Duration::from_millis(100);
    pool.set_idle_keepalive(keepalive);
    pool.start();

    let dir = tempfile::tempdir().unwrap();
    let delivered = submit_and_observe(&pool, &one_article_job("job-a", dir.path())).await;
    assert!(
        matches!(delivered, ProgressUpdate::ArticleComplete { .. }),
        "the article must download, got {delivered:?}"
    );

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let open: usize = pool.connection_snapshot().iter().map(|(_, n, _)| n).sum();
        if open == 0 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "open count stayed at {open} after the server closed the idle socket"
        );
        tokio::time::sleep(keepalive / 2).await;
    }

    // Nothing is queued, so the worker must stay disconnected.
    tokio::time::sleep(keepalive * 5).await;
    assert_eq!(pool.connection_snapshot(), vec![("srv".into(), 0, 1)]);

    // New work reconnects and is served.
    let dir = tempfile::tempdir().unwrap();
    let again = submit_and_observe(&pool, &one_article_job("job-b", dir.path())).await;
    assert!(
        matches!(again, ProgressUpdate::ArticleComplete { .. }),
        "work after an idle close must reconnect and download, got {again:?}"
    );

    pool.shutdown().await;
}
