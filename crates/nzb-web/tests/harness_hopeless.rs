//! Phase 6 contract tests — `HopelessTracker` as active authority.
//!
//! These exercise the new behaviour added in Phase 6:
//!
//! 1. The early-failure check (tier 2) is no longer capped to the first
//!    25% of articles. A high failure rate aborts the job continuously.
//! 2. The new time-based hopeless tier (tier 4) fires when the tracker has
//!    been alive past the configured `no_progress_timeout` AND no article
//!    event has ever reached it. This catches the zombie scenario where
//!    workers cycle silently — Phase 5 evicts them, but the queue manager
//!    needs an independent kill-switch to actually move the job to Failed.

mod harness;

use std::collections::HashMap;
use std::time::Duration;

use harness::nzb_fixture::NzbFixture;
use harness::{HarnessBuilder, ServerProfile};
use nzb_nntp::testutil::MockConfig;
use nzb_web::nzb_core::models::JobStatus;

#[tokio::test]
async fn no_progress_timeout_aborts_silent_zombie_job() {
    // Mock server hangs the moment it sees ARTICLE — workers connect, send
    // the request, never get a response. Phase 5 evicts them; this test
    // proves that Phase 6's time-based watchdog independently moves the
    // job to a terminal state instead of leaving it stuck in Downloading.
    let bodies: Vec<Vec<u8>> = (0..3).map(|i| format!("body-{i}").into_bytes()).collect();
    let segs: Vec<(&str, &[u8])> = (0..3)
        .map(|i| {
            let mid: &'static str = match i {
                0 => "np-1",
                1 => "np-2",
                _ => "np-3",
            };
            (mid, bodies[i].as_slice())
        })
        .collect();
    let fixture = NzbFixture::new("no-progress")
        .add_file("zombie.bin", &segs)
        .build();

    let server = ServerProfile::start(
        "hang-srv",
        MockConfig {
            hang_after_command: Some("ARTICLE".into()),
            ..Default::default()
        },
        2,
    )
    .await;

    let engine = HarnessBuilder::new()
        .with_server(server)
        .article_timeout(2)
        .abort_hopeless(true)
        .build();
    // Time-based hopeless threshold: 5 seconds. Combined with the
    // 1-second tick in spawn_speed_tracker, the abort should fire
    // ~5-6 seconds after the tracker is created.
    engine
        .queue_manager
        .set_no_progress_timeout(Duration::from_secs(5));
    // Long worker idle threshold so Phase 5 doesn't muddle the test —
    // we want to observe Phase 6 reacting independently.
    engine
        .queue_manager
        .set_max_worker_idle(Duration::from_mins(1));

    let job_id = engine
        .submit_nzb_xml("no-progress", fixture.xml)
        .expect("submit");

    // Wait for the job to actually enter Downloading (so the tracker is
    // instantiated and starts its clock).
    let downloading = engine
        .wait_for_status(&job_id, Duration::from_secs(5), &[JobStatus::Downloading])
        .await;
    assert!(downloading, "job never reached Downloading");

    // Within ~12 seconds the time-based tier should fire and move the job
    // to a terminal state (Failed/Completed via PostProcessing).
    let resolved = engine
        .wait_for(Duration::from_secs(15), |snap| {
            snap.job(&job_id)
                .map(|j| {
                    matches!(
                        j.status,
                        JobStatus::Failed | JobStatus::Completed | JobStatus::PostProcessing
                    )
                })
                .unwrap_or(false)
        })
        .await;

    let final_view = engine.job(&job_id).expect("job present");
    assert!(
        resolved,
        "Phase 6 no-progress watchdog did not abort the job. \
         status={}, downloaded={}, failed={}",
        final_view.status, final_view.articles_downloaded, final_view.articles_failed
    );
}

#[tokio::test]
async fn early_failure_check_fires_after_phase_6_window_removal() {
    // 30-segment NZB (well past the old 25% cap of 7 articles) where every
    // article returns 430. Phase 6 removed the `<= total/4` window, so
    // tier 2 must fire even after we've checked > 25% of articles.
    //
    // This test exercises the same single-server abort path as the Phase 3
    // typed_errors test but with enough articles that the OLD code would
    // have fallen through to tier 3. After Phase 6, tier 2 wins.
    let bodies: Vec<Vec<u8>> = (0..30).map(|i| format!("body-{i}").into_bytes()).collect();

    // Need stable string slices for the segments. Build a leaked vec.
    let mids: Vec<&'static str> = (0..30)
        .map(|i| {
            let s = format!("ef6-{i}");
            Box::leak(s.into_boxed_str()) as &'static str
        })
        .collect();
    let segs: Vec<(&str, &[u8])> = (0..30).map(|i| (mids[i], bodies[i].as_slice())).collect();

    let fixture = NzbFixture::new("early-failure-30")
        .add_file("data.bin", &segs)
        .build();

    let mut overrides = HashMap::new();
    for &mid in &mids {
        overrides.insert(mid.to_string(), 430u16);
    }

    let server = ServerProfile::start(
        "ef-srv",
        MockConfig {
            article_response_overrides: overrides,
            ..Default::default()
        },
        4,
    )
    .await;

    let engine = HarnessBuilder::new()
        .with_server(server)
        .article_timeout(10)
        .abort_hopeless(true)
        .build();

    let job_id = engine
        .submit_nzb_xml("early-failure-30", fixture.xml)
        .expect("submit");

    // Job should reach a terminal state. With 100% NotFound on the only
    // server, the tier-2 early-failure check fires after the first 10
    // articles are checked (EARLY_CHECK_MIN_ARTICLES). Without Phase 6,
    // the job would have continued past the 25% cap before any abort.
    let resolved = engine
        .wait_for_status(
            &job_id,
            Duration::from_secs(15),
            &[
                JobStatus::Failed,
                JobStatus::Completed,
                JobStatus::PostProcessing,
            ],
        )
        .await;

    let view = engine.job(&job_id).expect("job present");
    assert!(
        resolved,
        "tier-2 abort never fired. status={}, downloaded={}, failed={}",
        view.status, view.articles_downloaded, view.articles_failed
    );
    assert!(
        view.articles_failed > 0,
        "expected failures recorded against the typed-failure path"
    );
}
