//! Statistics ledger accounting across retries and restarts.
//!
//! A job's statistics row must count only the bytes it actually fetched and
//! the active time it spent fetching them, so neither a history retry nor a
//! restart mid-download double-counts bytes or inflates the recorded speed.

mod harness;

use std::path::PathBuf;
use std::time::Duration;

use harness::nzb_fixture::NzbFixture;
use harness::{HarnessBuilder, ServerProfile, yenc_articles};
use nzb_nntp::testutil::MockConfig;
use nzb_web::nzb_core::db::Database;
use nzb_web::nzb_core::models::JobStatus;
use nzb_web::nzb_core::nzb_parser;

#[tokio::test]
async fn history_retry_does_not_count_carried_over_bytes_again() {
    let fixture = NzbFixture::new("stats-retry")
        .add_file(
            "present.bin",
            &[("stats-retry-present", b"partial payload")],
        )
        .add_file("missing.bin", &[("stats-retry-missing", b"never served")])
        .build();
    let triples: Vec<(&str, &[u8], &str)> = fixture
        .articles
        .iter()
        .filter(|(m, _, _)| *m == "stats-retry-present")
        .map(|(m, b, f)| (*m, *b, f.as_str()))
        .collect();
    let server = ServerProfile::start(
        "stats-retry",
        MockConfig {
            articles: yenc_articles(&triples),
            ..Default::default()
        },
        2,
    )
    .await;
    let engine = HarnessBuilder::new()
        .with_server(server)
        .article_timeout(5)
        .build();
    let qm = &engine.queue_manager;

    let id = engine
        .submit_nzb_xml("stats-retry", fixture.xml.clone())
        .unwrap();
    assert!(
        engine
            .wait_for(Duration::from_secs(20), |_| engine.history_status(&id)
                == Some(JobStatus::Failed))
            .await,
        "first attempt did not fail"
    );
    let first = qm.global_statistics(&[]).lifetime;
    assert_eq!(first.downloads, 1);
    assert!(first.bytes_downloaded > 0);

    // The retry reuses the partial download: the present article is already
    // on disk, so the retry fetches nothing new before failing again.
    let entry = qm.history_get(&id).unwrap().unwrap();
    let retry_data = qm.history_get_retry_data(&id).unwrap();
    let retry = qm
        .prepare_retry_job(&entry, &fixture.xml, retry_data.as_deref())
        .unwrap();
    assert!(retry.downloaded_bytes > 0, "retry carries the checkpoint");
    let retry_id = retry.id.clone();
    qm.add_job(retry, Some(fixture.xml)).unwrap();
    assert!(
        engine
            .wait_for(Duration::from_secs(20), |_| engine
                .history_status(&retry_id)
                == Some(JobStatus::Failed))
            .await,
        "retry did not fail"
    );

    let after = qm.global_statistics(&[]).lifetime;
    assert_eq!(after.downloads, 2);
    assert_eq!(
        after.bytes_downloaded, first.bytes_downloaded,
        "bytes carried over from the failed attempt are already in its statistics row"
    );
    // The history row still reports the job's cumulative progress.
    let retried = qm.history_get(&retry_id).unwrap().unwrap();
    assert_eq!(retried.downloaded_bytes, entry.downloaded_bytes);
}

#[tokio::test]
async fn restart_keeps_server_stats_and_active_time_from_before_the_restart() {
    let fixture = NzbFixture::new("stats-restart")
        .add_file(
            "restart.bin",
            &[
                ("stats-restart-1", b"first"),
                ("stats-restart-2", b"second"),
            ],
        )
        .build();
    let triples = fixture
        .articles
        .iter()
        .map(|(id, bytes, name)| (*id, *bytes, name.as_str()))
        .collect::<Vec<_>>();
    let server = ServerProfile::start(
        "stats-restart",
        MockConfig {
            articles: yenc_articles(&triples),
            ..MockConfig::default()
        },
        1,
    )
    .await;
    let state = tempfile::tempdir().expect("restart state");
    let database_path = state.path().join("queue.sqlite");
    let incomplete_dir = state.path().join("incomplete");
    let complete_dir = state.path().join("complete");
    std::fs::create_dir_all(&incomplete_dir).unwrap();
    std::fs::create_dir_all(&complete_dir).unwrap();
    let mut job = nzb_parser::parse_nzb("stats-restart", &fixture.xml).unwrap();
    job.status = JobStatus::Downloading;
    job.work_dir = incomplete_dir.join(&job.id);
    job.output_dir = complete_dir.join(&job.name);
    job.downloaded_bytes = 5;
    let job_id = job.id.clone();
    let db = Database::open(&database_path).unwrap();
    db.queue_insert(&job).unwrap();
    db.queue_store_nzb_data(&job_id, &fixture.xml).unwrap();
    // The checkpoint the previous process left behind: one article fetched
    // from this server over 100 seconds of active download time.
    db.queue_store_job_data(
        &job_id,
        &serde_json::to_vec(&serde_json::json!({
            "files": {"restart.bin": [1]},
            "downloaded_bytes": 5,
            "articles_downloaded": 1,
            "articles_failed": 0,
            "files_completed": 0,
            "download_time_secs": 100.0,
            "server_stats": [{
                "server_id": "stats-restart",
                "server_name": "mock-stats-restart",
                "articles_downloaded": 1,
                "articles_failed": 0,
                "bytes_downloaded": 5
            }]
        }))
        .unwrap(),
    )
    .unwrap();
    drop(db);

    let engine = HarnessBuilder::restart_recovery(server)
        .with_database_path(database_path)
        .with_state_dir(PathBuf::from(state.path()))
        .build();
    engine.queue_manager.restore_from_db().unwrap();
    assert!(
        engine
            .wait_for(Duration::from_secs(10), |_| engine.history_status(&job_id)
                == Some(JobStatus::Completed))
            .await
    );

    let history = engine.queue_manager.history_get(&job_id).unwrap().unwrap();
    assert_eq!(history.downloaded_bytes, history.total_bytes);
    let server_stats = history
        .server_stats
        .iter()
        .find(|s| s.server_id == "stats-restart")
        .expect("server stats");
    assert_eq!(
        server_stats.articles_downloaded, 2,
        "the article fetched before the restart is still attributed to its server"
    );
    assert_eq!(server_stats.bytes_downloaded, history.downloaded_bytes);
    let download_time = history.download_time_secs.expect("download time");
    assert!(
        download_time >= 100.0,
        "active time from before the restart is kept, got {download_time}"
    );

    let lifetime = engine.queue_manager.global_statistics(&[]).lifetime;
    assert_eq!(lifetime.bytes_downloaded, history.downloaded_bytes);
    assert!(lifetime.total_duration_secs >= 100.0);
    assert!(
        lifetime.fastest_download_bps <= history.downloaded_bytes / 100,
        "speed must not be inflated by bytes fetched before the restart, got {} B/s",
        lifetime.fastest_download_bps
    );
}
