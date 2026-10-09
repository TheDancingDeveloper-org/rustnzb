//! Retained incomplete work directories must not leak forever.
//!
//! A failed job keeps its partial download in `incomplete/` so a history
//! retry can reuse it. Deleting that history entry, removing the queue job,
//! or restarting with directories nothing references must reclaim the space,
//! without ever reaching outside the incomplete root.

mod harness;

use std::path::{Path, PathBuf};
use std::time::Duration;

use harness::nzb_fixture::NzbFixture;
use harness::{HarnessBuilder, ServerProfile, TestEngine, yenc_articles};
use nzb_nntp::testutil::MockConfig;
use nzb_web::nzb_core::db::Database;
use nzb_web::nzb_core::models::{HistoryEntry, JobFailureCode, JobStatus, NzbJob};
use nzb_web::nzb_core::nzb_parser;

/// Start an engine whose provider serves only the first of two files, so a
/// submitted job fails after writing part of its payload.
async fn failing_engine(tag: &str) -> (TestEngine, Vec<u8>) {
    let present = format!("{tag}-present@test");
    let missing = format!("{tag}-missing@test");
    let fixture = NzbFixture::new(tag)
        .add_file("present.bin", &[(present.as_str(), b"partial payload")])
        .add_file("missing.bin", &[(missing.as_str(), b"never served")])
        .build();
    let triples: Vec<(&str, &[u8], &str)> = fixture
        .articles
        .iter()
        .filter(|(m, _, _)| *m == present.as_str())
        .map(|(m, b, f)| (*m, *b, f.as_str()))
        .collect();
    let server = ServerProfile::start(
        tag,
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
    (engine, fixture.xml)
}

async fn submit_and_fail(engine: &TestEngine, xml: Vec<u8>) -> (String, PathBuf) {
    let id = engine.submit_nzb_xml("partial", xml).expect("submit");
    assert!(
        engine
            .wait_for_status(&id, Duration::from_secs(20), &[JobStatus::Failed])
            .await,
        "job did not fail"
    );
    let work_dir = engine.incomplete_dir.join(&id);
    assert!(
        work_dir.join("present.bin").exists(),
        "a failed job keeps its partial download for retry"
    );
    (id, work_dir)
}

#[tokio::test]
async fn deleting_failed_history_entry_removes_retained_work_dir() {
    let (engine, xml) = failing_engine("hist-del").await;
    let (id, work_dir) = submit_and_fail(&engine, xml).await;

    engine.queue_manager.history_remove(&id).unwrap();

    assert!(
        !work_dir.exists(),
        "deleting the history entry must remove its retained work dir"
    );
    assert!(engine.incomplete_dir.is_dir(), "incomplete root survives");
}

#[tokio::test]
async fn clearing_history_removes_retained_work_dirs() {
    let (engine, xml) = failing_engine("hist-clear").await;
    let (_id, work_dir) = submit_and_fail(&engine, xml).await;

    engine.queue_manager.history_clear().unwrap();

    assert!(!work_dir.exists());
    assert!(engine.incomplete_dir.is_dir());
}

#[tokio::test]
async fn history_delete_keeps_work_dir_a_queued_retry_is_using() {
    let (engine, xml) = failing_engine("hist-retry").await;
    let (id, work_dir) = submit_and_fail(&engine, xml.clone()).await;

    // Retry reuses the retained partial directory.
    let qm = &engine.queue_manager;
    let entry = qm.history_get(&id).unwrap().unwrap();
    let retry_data = qm.history_get_retry_data(&id).unwrap();
    let retry = qm
        .prepare_retry_job(&entry, &xml, retry_data.as_deref())
        .unwrap();
    assert_eq!(
        retry.work_dir, work_dir,
        "retry reuses the partial download"
    );
    qm.pause_all();
    qm.add_job(retry, Some(xml)).unwrap();

    qm.history_remove(&id).unwrap();

    assert!(
        work_dir.join("present.bin").exists(),
        "a work dir still used by a queued retry must survive history delete"
    );
}

#[tokio::test]
async fn deleting_a_queued_job_removes_its_work_dir() {
    let (engine, xml) = failing_engine("queue-del").await;
    engine.queue_manager.pause_all();
    let id = engine.submit_nzb_xml("queued", xml).unwrap();
    let work_dir = engine.incomplete_dir.join(&id);
    assert!(work_dir.is_dir());

    engine.queue_manager.remove_job(&id).unwrap();

    assert!(!work_dir.exists());
}

fn history_entry(id: &str, complete: &Path) -> HistoryEntry {
    HistoryEntry {
        id: id.to_string(),
        name: id.to_string(),
        category: "Default".to_string(),
        status: JobStatus::Failed,
        total_bytes: 1,
        downloaded_bytes: 0,
        added_at: chrono::Utc::now(),
        completed_at: chrono::Utc::now(),
        download_time_secs: None,
        output_dir: complete.join(id),
        stages: Vec::new(),
        error_message: Some("failed".to_string()),
        failure_code: Some(JobFailureCode::DownloadFailed),
        post_processing: None,
        delete_archives: None,
        server_stats: Vec::new(),
        nzb_data: None,
        retry_data: None,
    }
}

fn dir_with_file(path: &Path) {
    std::fs::create_dir_all(path).unwrap();
    std::fs::write(path.join("data.bin"), b"data").unwrap();
}

#[tokio::test]
async fn startup_sweeps_unreferenced_incomplete_dirs_only() {
    let state = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let database_path = state.path().join("rustnzb.db");
    let incomplete = state.path().join("incomplete");
    let complete = state.path().join("complete");
    // Work directories are named after job ids (UUIDs).
    let kept_history = "1a2b3c4d-5e6f-4a7b-8c9d-0e1f2a3b4c5d";
    let retried_history = "3c4d5e6f-7a8b-4c9d-8e0f-1a2b3c4d5e6f";
    let reused_partial = incomplete.join("2b3c4d5e-6f7a-4b8c-9d0e-1f2a3b4c5d6e");
    let orphan_one = incomplete.join("0b7c7f0e-3a51-4c4e-9a43-1f6f0c2b8d11");
    let orphan_two = incomplete.join("5d2a9c1e-7b3f-4e8a-b6c0-2e4f8a1d9c33");
    let job_named_file = incomplete.join("7f6e5d4c-3b2a-4190-8f7e-6d5c4b3a2918");
    let link = incomplete.join("9e8d7c6b-5a49-4382-a1b0-c9d8e7f6a5b4");
    let user_folder = incomplete.join("user-folder");

    let fixture = NzbFixture::new("queued")
        .add_file("queued.bin", &[("sweep-queued@test", b"queued")])
        .build();
    let mut queued: NzbJob = nzb_parser::parse_nzb("queued", &fixture.xml).unwrap();
    queued.work_dir = incomplete.join(&queued.id);
    queued.output_dir = complete.join("queued");
    queued.status = JobStatus::Paused;
    {
        let db = Database::open(&database_path).unwrap();
        db.queue_insert(&queued).unwrap();
        db.queue_store_nzb_data(&queued.id, &fixture.xml).unwrap();
        db.history_insert(&history_entry(kept_history, &complete))
            .unwrap();
        // A retry reused another job's partial dir; its checkpoint names it.
        let mut retried = history_entry(retried_history, &complete);
        retried.retry_data = Some(
            serde_json::to_vec(&serde_json::json!({
                "files": {},
                "downloaded_bytes": 0,
                "articles_downloaded": 0,
                "articles_failed": 0,
                "files_completed": 0,
                "work_dir": reused_partial,
            }))
            .unwrap(),
        );
        db.history_insert(&retried).unwrap();
    }

    dir_with_file(&queued.work_dir);
    dir_with_file(&incomplete.join(kept_history));
    dir_with_file(&reused_partial);
    dir_with_file(&orphan_one);
    dir_with_file(&orphan_two.join("nested"));
    dir_with_file(&user_folder);
    std::fs::write(&job_named_file, b"a file, not a dir").unwrap();
    dir_with_file(&outside.path().join("target"));
    #[cfg(unix)]
    std::os::unix::fs::symlink(outside.path().join("target"), &link).unwrap();

    let server = ServerProfile::start("sweep", MockConfig::default(), 1).await;
    let engine = HarnessBuilder::new()
        .with_server(server)
        .with_database_path(database_path)
        .with_state_dir(state.path().to_path_buf())
        .build();
    engine.queue_manager.restore_from_db().unwrap();

    assert!(!orphan_one.exists(), "unreferenced job dir is removed");
    assert!(!orphan_two.exists(), "nested content goes with it");
    assert!(
        queued.work_dir.join("data.bin").exists(),
        "queue job dir kept"
    );
    assert!(incomplete.join(kept_history).join("data.bin").exists());
    assert!(reused_partial.join("data.bin").exists());
    assert!(
        user_folder.join("data.bin").exists(),
        "only job-id named directories are swept"
    );
    assert!(job_named_file.is_file(), "files are left alone");
    assert!(incomplete.is_dir(), "the incomplete root itself is kept");
    assert!(
        outside.path().join("target").join("data.bin").exists(),
        "the sweep must never follow a symlink out of the incomplete root"
    );
    #[cfg(unix)]
    assert!(
        std::fs::symlink_metadata(&link).is_ok(),
        "symlinks are not directories and are left alone"
    );
}

#[tokio::test]
async fn startup_sweep_keeps_everything_when_a_queue_row_is_undecodable() {
    let state = tempfile::tempdir().unwrap();
    let database_path = state.path().join("rustnzb.db");
    let incomplete = state.path().join("incomplete");
    let complete = state.path().join("complete");
    let orphan = incomplete.join("0b7c7f0e-3a51-4c4e-9a43-1f6f0c2b8d11");

    let fixture = NzbFixture::new("queued")
        .add_file("queued.bin", &[("undecodable-queued@test", b"queued")])
        .build();
    let mut queued: NzbJob = nzb_parser::parse_nzb("queued", &fixture.xml).unwrap();
    queued.work_dir = incomplete.join(&queued.id);
    queued.output_dir = complete.join("queued");
    queued.status = JobStatus::Paused;
    {
        let db = Database::open(&database_path).unwrap();
        db.queue_insert(&queued).unwrap();
        db.queue_store_nzb_data(&queued.id, &fixture.xml).unwrap();
        drop(db);
        // One undecodable column makes queue_list() fail for the whole table.
        let status = std::process::Command::new("sqlite3")
            .arg(&database_path)
            .arg(format!(
                "UPDATE queue SET total_bytes = 'x' WHERE id = '{}'",
                queued.id
            ))
            .status()
            .expect("sqlite3");
        assert!(status.success(), "corrupting the queue row failed");
    }

    dir_with_file(&queued.work_dir);
    dir_with_file(&orphan);

    let server = ServerProfile::start("sweep-bad-row", MockConfig::default(), 1).await;
    let engine = HarnessBuilder::new()
        .with_server(server)
        .with_database_path(database_path)
        .with_state_dir(state.path().to_path_buf())
        .build();
    // Restoration fails on the same undecodable row. The sweep must not have
    // deleted anything first: a read error fails closed.
    assert!(engine.queue_manager.restore_from_db().is_err());

    assert!(
        queued.work_dir.join("data.bin").exists(),
        "the queued job's work dir survives an unreadable queue"
    );
    assert!(
        orphan.join("data.bin").exists(),
        "even an orphan survives when the reference set cannot be read"
    );
}
