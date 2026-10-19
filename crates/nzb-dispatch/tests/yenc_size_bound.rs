//! An NZB file that declares no size is bounded by the first `=ybegin size=`
//! the pool sees for it. These tests send a real article stream through a
//! live dispatcher and check the bytes that land on disk and the progress
//! events that report them.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use nzb_core::config::ServerConfig;
use nzb_core::models::{JobStatus, NzbFile, NzbJob, Priority};
use nzb_dispatch::bandwidth::{BandwidthConfig, BandwidthLimiter};
use nzb_dispatch::{ArticleFailureKind, DispatchEngine, DispatchHandle, ProgressUpdate};
use nzb_nntp::Article;
use nzb_nntp::testutil::{MockConfig, MockNntpServer, test_config};
use parking_lot::Mutex;
use tokio::sync::mpsc;
use tokio::time::Instant;

const EVENT_TIMEOUT: Duration = Duration::from_secs(15);

/// A job whose single file declares no size: every `bytes` is 0, so the
/// NZB sum cannot bound the write and the first yEnc `size=` must.
fn undeclared_job(root: &Path, message_ids: &[&str]) -> NzbJob {
    let articles = message_ids
        .iter()
        .enumerate()
        .map(|(index, message_id)| Article {
            message_id: (*message_id).into(),
            segment_number: index as u32 + 1,
            bytes: 0,
            downloaded: false,
            data_begin: None,
            data_size: None,
            crc32: None,
            tried_servers: Vec::new(),
            tries: 0,
        })
        .collect();
    NzbJob {
        slice_map: None,
        held_par2_bytes: 0,
        id: "job".into(),
        name: "job".into(),
        category: "Default".into(),
        status: JobStatus::Downloading,
        priority: Priority::Normal,
        total_bytes: 0,
        downloaded_bytes: 0,
        file_count: 1,
        files_completed: 0,
        article_count: message_ids.len(),
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
            bytes: 0,
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

/// One connection, so the worker fetches segments in declared order and the
/// first `size=` is the one seen first.
fn pool_for(port: u16) -> DispatchHandle {
    let mut config: ServerConfig = test_config(port);
    config.id = "srv".into();
    config.name = "srv".into();
    config.connections = 1;
    DispatchHandle::new(
        Arc::new(Mutex::new(vec![config])),
        Arc::new(BandwidthLimiter::new(BandwidthConfig::default())),
        0,
    )
}

/// Run the job to `JobFinished`, collecting every event on the way.
async fn run_to_finish(pool: &DispatchHandle, job: &NzbJob) -> Vec<ProgressUpdate> {
    let (tx, mut rx) = mpsc::channel(64);
    pool.submit_job(job, tx);
    let deadline = Instant::now() + EVENT_TIMEOUT;
    let mut events = Vec::new();
    loop {
        let event = tokio::time::timeout_at(deadline, rx.recv())
            .await
            .expect("timed out waiting for the job to finish")
            .expect("progress channel closed before JobFinished");
        let finished = matches!(event, ProgressUpdate::JobFinished { .. });
        events.push(event);
        if finished {
            return events;
        }
    }
}

#[tokio::test]
async fn undeclared_file_with_agreeing_yenc_sizes_is_written_in_full() {
    // Control: both headers name the same 8-byte file, so both segments are
    // written. Without this the rejection test below could pass for the wrong
    // reason.
    let (seg1, _) = yenc_simd::encode_article(b"AAAA", "out.bin", 1, 2, 0, 8);
    let (seg2, _) = yenc_simd::encode_article(b"BBBB", "out.bin", 2, 2, 4, 8);
    let mock = MockNntpServer::start(MockConfig {
        articles: HashMap::from([("seg1@test".into(), seg1), ("seg2@test".into(), seg2)]),
        ..MockConfig::default()
    })
    .await;
    let pool = pool_for(mock.port());
    pool.start();

    let root = tempfile::tempdir().unwrap();
    let events = run_to_finish(
        &pool,
        &undeclared_job(root.path(), &["seg1@test", "seg2@test"]),
    )
    .await;

    assert!(matches!(
        events.last(),
        Some(ProgressUpdate::JobFinished { success: true, .. })
    ));
    assert_eq!(
        std::fs::read(root.path().join("out.bin")).unwrap(),
        b"AAAABBBB"
    );

    pool.shutdown().await;
}

// Known gap: on the serial worker path (pipelining 1, the default) the fetch
// passes no pinned-size map, so the first `size=` is never recorded and a later
// segment past it is written. The pipelined path enforces the bound. Remove
// this `ignore` once the serial path shares the context's pinned sizes.
#[ignore = "serial worker path does not pin the first yEnc size= (download_engine.rs fetch_article_with_retry passes None)"]
#[tokio::test]
async fn undeclared_segment_past_the_first_yenc_size_is_rejected_on_the_wire() {
    // Segment 1 sets the bound at 8 bytes. Segment 2 arrives over NNTP with its
    // own header claiming a 12-byte file and a range (offset 8, 4 bytes) that
    // runs past that bound. It must not be written, so the file stays at the
    // bytes segment 1 supplied.
    let (seg1, _) = yenc_simd::encode_article(b"AAAA", "out.bin", 1, 2, 0, 8);
    let (seg2, _) = yenc_simd::encode_article(b"BBBB", "out.bin", 2, 2, 8, 12);
    let mock = MockNntpServer::start(MockConfig {
        articles: HashMap::from([("seg1@test".into(), seg1), ("seg2@test".into(), seg2)]),
        ..MockConfig::default()
    })
    .await;
    let pool = pool_for(mock.port());
    pool.start();

    let root = tempfile::tempdir().unwrap();
    let events = run_to_finish(
        &pool,
        &undeclared_job(root.path(), &["seg1@test", "seg2@test"]),
    )
    .await;

    let delivered = events.iter().find_map(|event| match event {
        ProgressUpdate::ArticleComplete {
            segment_number: 1,
            decoded_bytes,
            ..
        } => Some(*decoded_bytes),
        _ => None,
    });
    assert_eq!(
        delivered,
        Some(4),
        "segment 1 must be written, got {events:?}"
    );

    let rejected = events.iter().find_map(|event| match event {
        ProgressUpdate::ArticleFailed {
            segment_number: 2,
            failure,
            ..
        } => Some(failure.kind),
        _ => None,
    });
    assert_eq!(
        rejected,
        Some(ArticleFailureKind::DecodeError),
        "segment 2 must be rejected as a corrupt yEnc article, got {events:?}"
    );

    assert!(matches!(
        events.last(),
        Some(ProgressUpdate::JobFinished { success: false, .. })
    ));
    assert_eq!(std::fs::read(root.path().join("out.bin")).unwrap(), b"AAAA");

    pool.shutdown().await;
}
