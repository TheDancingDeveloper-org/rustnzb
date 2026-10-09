//! Table-driven NNTP failure and lifecycle invariants.

mod harness;

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use harness::nzb_fixture::{FixtureCatalog, NzbFixture};
use harness::{HarnessBuilder, ServerProfile, yenc_articles};
use nzb_nntp::testutil::MockConfig;
use nzb_web::nzb_core::db::Database;
use nzb_web::nzb_core::models::JobStatus;
use nzb_web::nzb_core::nzb_parser;

#[tokio::test]
async fn transient_failures_recover_without_duplicate_completion() {
    let body = b"recoverable";
    let fixture = NzbFixture::new("failure-matrix")
        .add_file("payload.bin", &[("failure-matrix-1", body)])
        .build();
    let triples = fixture
        .articles
        .iter()
        .map(|(id, bytes, name)| (*id, *bytes, name.as_str()))
        .collect::<Vec<_>>();
    let mut sequences = HashMap::new();
    sequences.insert(
        "failure-matrix-1".to_string(),
        std::collections::VecDeque::from([(400, "temporary failure".into())]),
    );
    let server = ServerProfile::start(
        "failure-matrix",
        MockConfig {
            articles: yenc_articles(&triples),
            article_response_sequences: Some(std::sync::Arc::new(parking_lot::Mutex::new(
                sequences,
            ))),
            ..MockConfig::default()
        },
        1,
    )
    .await;
    let engine = HarnessBuilder::new().with_server(server).build();
    let id = engine
        .submit_nzb_xml("failure-matrix", fixture.xml)
        .unwrap();
    assert!(
        engine
            .wait_for_status(&id, Duration::from_secs(10), &[JobStatus::Completed])
            .await
    );
    let history = engine
        .queue_manager
        .history_get(&id)
        .expect("history query")
        .expect("completed history");
    assert_eq!(history.status, JobStatus::Completed);
    assert_eq!(history.downloaded_bytes, history.total_bytes);
}

#[tokio::test]
async fn cancellation_releases_connection_slots_and_removes_active_job() {
    let fixture = FixtureCatalog::single();
    let server = ServerProfile::start(
        "cancel",
        MockConfig {
            articles: fixture.mock_config().articles,
            hang_after_command: Some("ARTICLE".into()),
            ..MockConfig::default()
        },
        1,
    )
    .await;
    let engine = HarnessBuilder::cancellation(server)
        .article_timeout(2)
        .build();
    let id = engine
        .submit_nzb_xml(&fixture.name, fixture.xml)
        .expect("submit fixture");
    assert!(
        engine
            .wait_for_status(&id, Duration::from_secs(3), &[JobStatus::Downloading])
            .await
    );

    engine
        .queue_manager
        .remove_job(&id)
        .expect("cancel active job");
    assert!(engine.job(&id).is_none());
    assert!(
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if engine.queue_manager.connection_total() == 0 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .is_ok(),
        "cancelled jobs must release all NNTP slots"
    );
}

#[tokio::test]
async fn pause_and_resume_preserve_progress_until_single_completion() {
    let fixture = FixtureCatalog::multi_segment();
    let server = ServerProfile::start(
        "pause-resume",
        MockConfig {
            articles: fixture.articles.clone(),
            response_delay: Some(Duration::from_millis(120)),
            ..MockConfig::default()
        },
        1,
    )
    .await;
    let engine = HarnessBuilder::pause_resume(server)
        .article_timeout(5)
        .build();
    let id = engine
        .submit_nzb_xml(&fixture.name, fixture.xml)
        .expect("submit fixture");
    assert!(
        engine
            .wait_for_status(&id, Duration::from_secs(5), &[JobStatus::Downloading])
            .await
    );

    engine
        .queue_manager
        .pause_job(&id)
        .expect("pause active job");
    assert!(
        engine
            .wait_for_status(&id, Duration::from_secs(3), &[JobStatus::Paused])
            .await
    );
    let paused = engine.job(&id).expect("paused job");
    tokio::time::sleep(Duration::from_millis(250)).await;
    let still_paused = engine.job(&id).expect("paused job remains queued");
    assert_eq!(still_paused.status, JobStatus::Paused);
    assert_eq!(still_paused.articles_downloaded, paused.articles_downloaded);

    engine
        .queue_manager
        .resume_job(&id)
        .expect("resume paused job");
    assert!(
        engine
            .wait_for_status(&id, Duration::from_secs(10), &[JobStatus::Completed])
            .await
    );
    assert_eq!(engine.history_status(&id), Some(JobStatus::Completed));
}

#[tokio::test]
async fn authentication_failure_waits_for_recovery_without_leaking_connections() {
    let fixture = FixtureCatalog::single();
    let mut server = ServerProfile::start(
        "auth-failure",
        MockConfig {
            articles: fixture.mock_config().articles,
            auth_required: true,
            fail_auth: true,
            ..MockConfig::default()
        },
        1,
    )
    .await;
    server.config.username = Some("user".into());
    server.config.password = Some("pass".into());
    let engine = HarnessBuilder::hopeless(server).build();
    let id = engine
        .submit_nzb_xml(&fixture.name, fixture.xml)
        .expect("submit fixture");
    assert!(
        engine
            .wait_for(Duration::from_secs(8), |snapshot| {
                snapshot.job(&id).is_some_and(|job| {
                    job.status == JobStatus::Downloading && job.error_message.is_some()
                })
            })
            .await
    );
    assert_eq!(engine.queue_manager.connected_snapshot()[0].1, 0);
}

#[tokio::test]
async fn restart_restores_checkpoint_and_skips_completed_article() {
    let fixture = NzbFixture::new("restart-matrix")
        .add_file(
            "restart.bin",
            &[("restart-1", b"first"), ("restart-2", b"second")],
        )
        .build();
    let triples = fixture
        .articles
        .iter()
        .map(|(id, bytes, name)| (*id, *bytes, name.as_str()))
        .collect::<Vec<_>>();
    let mut overrides = HashMap::new();
    overrides.insert("restart-1".to_string(), 430);
    let server = ServerProfile::start(
        "restart",
        MockConfig {
            articles: yenc_articles(&triples),
            article_response_overrides: overrides,
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
    let mut job = nzb_parser::parse_nzb("restart-matrix", &fixture.xml).unwrap();
    job.status = JobStatus::Downloading;
    job.work_dir = incomplete_dir.join(&job.id);
    job.output_dir = complete_dir.join(&job.name);
    let job_id = job.id.clone();
    let db = Database::open(&database_path).unwrap();
    db.queue_insert(&job).unwrap();
    db.queue_store_nzb_data(&job_id, &fixture.xml).unwrap();
    db.queue_store_job_data(
        &job_id,
        &serde_json::to_vec(&serde_json::json!({
            "files": {"restart.bin": [1]},
            "downloaded_bytes": 5,
            "articles_downloaded": 1,
            "articles_failed": 0,
            "files_completed": 0
        }))
        .unwrap(),
    )
    .unwrap();

    let engine = HarnessBuilder::restart_recovery(server)
        .with_database_path(database_path)
        .with_state_dir(PathBuf::from(state.path()))
        .build();
    engine.queue_manager.restore_from_db().unwrap();

    assert!(
        engine
            .wait_for_status(&job_id, Duration::from_secs(10), &[JobStatus::Completed])
            .await
    );
    let history = engine.queue_manager.history_get(&job_id).unwrap().unwrap();
    assert_eq!(history.status, JobStatus::Completed);
    assert_eq!(history.downloaded_bytes, history.total_bytes);
}

#[tokio::test]
async fn resuming_paused_jobs_respects_active_download_limit() {
    let first = NzbFixture::new("limit-first")
        .add_file(
            "first.bin",
            &[
                ("limit-first-1", b"first-one"),
                ("limit-first-2", b"first-two"),
                ("limit-first-3", b"first-three"),
                ("limit-first-4", b"first-four"),
            ],
        )
        .build();
    let second = NzbFixture::new("limit-second")
        .add_file(
            "second.bin",
            &[
                ("limit-second-1", b"second-one"),
                ("limit-second-2", b"second-two"),
                ("limit-second-3", b"second-three"),
                ("limit-second-4", b"second-four"),
            ],
        )
        .build();
    let triples = first
        .articles
        .iter()
        .chain(second.articles.iter())
        .map(|(id, bytes, name)| (*id, *bytes, name.as_str()))
        .collect::<Vec<_>>();
    let server = ServerProfile::start(
        "limit",
        MockConfig {
            articles: yenc_articles(&triples),
            response_delay: Some(Duration::from_millis(150)),
            ..MockConfig::default()
        },
        1,
    )
    .await;
    // max_active_downloads = 1
    let engine = HarnessBuilder::pause_resume(server)
        .article_timeout(10)
        .build();
    let first_id = engine
        .submit_nzb_xml("limit-first", first.xml)
        .expect("submit first");
    let second_id = engine
        .submit_nzb_xml("limit-second", second.xml)
        .expect("submit second");
    assert!(
        engine
            .wait_for_status(&first_id, Duration::from_secs(5), &[JobStatus::Downloading])
            .await
    );
    assert_eq!(engine.job(&second_id).unwrap().status, JobStatus::Queued);

    // Pausing the active job hands its slot to the queued one; pause that too
    // so both jobs hold a paused context in the worker pool.
    engine.queue_manager.pause_job(&first_id).unwrap();
    assert!(
        engine
            .wait_for_status(
                &second_id,
                Duration::from_secs(5),
                &[JobStatus::Downloading]
            )
            .await
    );
    engine.queue_manager.pause_job(&second_id).unwrap();

    engine.queue_manager.resume_job(&first_id).unwrap();
    engine.queue_manager.resume_job(&second_id).unwrap();
    let snapshot = engine.snapshot();
    let downloading = snapshot
        .jobs
        .iter()
        .filter(|job| job.status == JobStatus::Downloading)
        .count();
    assert_eq!(downloading, 1, "max_active_downloads=1 must hold on resume");
    assert_eq!(engine.job(&second_id).unwrap().status, JobStatus::Queued);

    // The queued job must still be started (from its paused pool context)
    // once the active one finishes.
    for id in [&first_id, &second_id] {
        assert!(
            engine
                .wait_for_status(id, Duration::from_secs(15), &[JobStatus::Completed])
                .await,
            "job {id} did not complete"
        );
    }
}
