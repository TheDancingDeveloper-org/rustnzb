//! BUG-57 — speed-limit changes must reach in-flight and newly added jobs.
//!
//! These drive a real `QueueManager` + worker pool against a mock NNTP
//! server and change the limit through `QueueManager::set_speed_limit`, the
//! same call `PUT /api/config/speed-limit` makes. A tiny limit must throttle
//! the download, and raising it — or clearing it to 0 (unlimited) — must
//! release workers parked on the old limiter within about a second.

mod harness;

use std::time::{Duration, Instant};

use harness::nzb_fixture::NzbFixture;
use harness::{HarnessBuilder, ServerProfile, TestEngine, yenc_articles};
use nzb_nntp::testutil::MockConfig;
use nzb_web::nzb_core::models::JobStatus;

const SEGMENT_BYTES: usize = 64 * 1024;

/// Build an NZB of `segments` 64 KiB articles served by a mock with
/// `connections` connections, and an engine with `speed_limit_bps` set at
/// construction (the startup path that reads `general.speed_limit_bps`).
async fn engine_with_job(
    name: &str,
    segments: usize,
    connections: u16,
    speed_limit_bps: u64,
) -> (TestEngine, String, Vec<u8>) {
    let ids: Vec<String> = (0..segments).map(|i| format!("{name}-{i}@test")).collect();
    let bodies: Vec<Vec<u8>> = (0..segments)
        .map(|i| vec![(i % 251) as u8; SEGMENT_BYTES])
        .collect();
    let segs: Vec<(&str, &[u8])> = ids
        .iter()
        .zip(&bodies)
        .map(|(id, body)| (id.as_str(), body.as_slice()))
        .collect();
    let fixture = NzbFixture::new(name).add_file("data.bin", &segs).build();
    let triples: Vec<(&str, &[u8], &str)> = fixture
        .articles
        .iter()
        .map(|(m, b, f)| (*m, *b, f.as_str()))
        .collect();
    let server = ServerProfile::start(
        name,
        MockConfig {
            articles: yenc_articles(&triples),
            ..Default::default()
        },
        connections,
    )
    .await;
    let engine = HarnessBuilder::new()
        .with_server(server)
        .article_timeout(30)
        .speed_limit_bps(speed_limit_bps)
        .build();
    (engine, name.to_string(), fixture.xml)
}

fn downloaded(engine: &TestEngine, job_id: &str) -> usize {
    engine
        .job(job_id)
        .map(|j| j.articles_downloaded)
        .unwrap_or(usize::MAX) // left the queue => finished
}

async fn assert_completes_within(engine: &TestEngine, job_id: &str, limit: Duration) {
    let start = Instant::now();
    let done = engine
        .wait_for_status(
            job_id,
            limit,
            &[
                JobStatus::Completed,
                JobStatus::PostProcessing,
                JobStatus::Verifying,
                JobStatus::Repairing,
                JobStatus::Extracting,
            ],
        )
        .await
        || engine
            .wait_for(Duration::ZERO, |s| {
                s.job(job_id)
                    .is_some_and(|j| j.articles_downloaded + j.articles_failed >= j.article_count)
            })
            .await;
    assert!(
        done,
        "job did not finish downloading within {limit:?} of the limit change (downloaded {})",
        downloaded(engine, job_id)
    );
    eprintln!("finished {:?} after the limit change", start.elapsed());
}

/// A tiny limit configured at startup throttles the first job; clearing it
/// to unlimited releases the parked workers promptly.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn startup_limit_throttles_and_unlimited_releases() {
    let (engine, name, xml) = engine_with_job("bw-startup", 6, 2, 20).await;
    let job_id = engine.submit_nzb_xml(&name, xml).expect("submit");

    tokio::time::sleep(Duration::from_millis(1_500)).await;
    assert_eq!(
        downloaded(&engine, &job_id),
        0,
        "a 20 B/s limit must hold back 64 KiB articles"
    );

    engine.queue_manager.set_speed_limit(0);
    assert_completes_within(&engine, &job_id, Duration::from_secs(3)).await;
}

/// The live repro: the limit is set (as by the PUT handler) while idle, then
/// a job is added. It must be throttled, and raising the limit must release
/// it promptly rather than after the old 20 B/s waits drain.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn limit_set_before_job_throttles_and_raise_releases() {
    let (engine, name, xml) = engine_with_job("bw-preset", 6, 2, 0).await;
    engine.queue_manager.set_speed_limit(20);
    let job_id = engine.submit_nzb_xml(&name, xml).expect("submit");

    tokio::time::sleep(Duration::from_millis(1_500)).await;
    assert_eq!(
        downloaded(&engine, &job_id),
        0,
        "a limit set before the job was added must throttle it"
    );

    engine.queue_manager.set_speed_limit(10_000_000);
    assert_completes_within(&engine, &job_id, Duration::from_secs(3)).await;
}

/// Lowering the limit mid-download throttles in-flight work, and clearing it
/// unthrottles again.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mid_download_changes_throttle_and_unthrottle() {
    // 40 x 64 KiB at 256 KiB/s on one connection is ~10 s unthrottled-ish.
    let (engine, name, xml) = engine_with_job("bw-mid", 40, 1, 256 * 1024).await;
    let job_id = engine.submit_nzb_xml(&name, xml).expect("submit");

    assert!(
        engine
            .wait_for(Duration::from_secs(5), |s| s
                .job(&job_id)
                .is_some_and(|j| j.articles_downloaded >= 2))
            .await,
        "download should make progress under a 256 KiB/s limit"
    );

    engine.queue_manager.set_speed_limit(20);
    // Articles already past the limiter (pipelined) may still land.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let after_throttle = downloaded(&engine, &job_id);
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    assert_eq!(
        downloaded(&engine, &job_id),
        after_throttle,
        "lowering the limit to 20 B/s must stop progress"
    );

    engine.queue_manager.set_speed_limit(0);
    assert_completes_within(&engine, &job_id, Duration::from_secs(3)).await;
}
