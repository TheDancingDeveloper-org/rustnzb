use std::sync::Arc;

use arc_swap::ArcSwap;
use axum::Json;
use axum::extract::{Path, State};
use nzb_web::auth::{CredentialStore, TokenStore};
use nzb_web::nzb_core::config::AppConfig;
use nzb_web::nzb_core::db::Database;
use nzb_web::nzb_core::models::MarkReadInput;
use nzb_web::nzb_core::nzb_nntp::XoverEntry;
use nzb_web::{AppState, QueueManager};
use rustnzb::group_handlers::h_header_mark_read;
use tempfile::TempDir;

fn build_test_state() -> (Arc<AppState>, TempDir) {
    let config = AppConfig::default();
    let db = Database::open_memory().expect("open in-memory database");
    let tempdir = TempDir::new().expect("create tempdir");
    let incomplete_dir = tempdir.path().join("incomplete");
    let complete_dir = tempdir.path().join("complete");
    std::fs::create_dir_all(&incomplete_dir).expect("create incomplete dir");
    std::fs::create_dir_all(&complete_dir).expect("create complete dir");

    let log_buffer = nzb_web::LogBuffer::new();
    let queue_manager = QueueManager::new(
        config.servers.clone(),
        db,
        incomplete_dir,
        complete_dir,
        log_buffer.clone(),
        config.general.max_active_downloads,
        config.categories.clone(),
        config.general.min_free_space_bytes,
        config.general.speed_limit_bps,
        false,
        config.general.abort_hopeless,
        config.general.early_failure_check,
        config.general.required_completion_pct,
        config.general.article_timeout_secs,
        None,
    );
    let state = Arc::new(AppState::new(
        Arc::new(ArcSwap::from_pointee(config)),
        tempdir.path().join("config.toml"),
        queue_manager,
        log_buffer,
        Arc::new(TokenStore::new()),
        Arc::new(CredentialStore::new(tempdir.path().to_path_buf())),
    ));

    (state, tempdir)
}

fn seed_headers(state: &Arc<AppState>) -> (i64, Vec<i64>) {
    let group_name = "alt.binaries.test".to_string();
    state
        .queue_manager
        .with_db(|db| db.group_upsert_batch(&[(group_name.clone(), 3, 1)]))
        .expect("insert group");
    let group = state
        .queue_manager
        .with_db(|db| db.group_list(false, Some(&group_name), 10, 0))
        .expect("list groups")
        .into_iter()
        .next()
        .expect("group exists");

    let headers = vec![
        XoverEntry {
            article_num: 1,
            subject: "Subject 1".into(),
            from: "poster@example.test".into(),
            date: "2026-07-08".into(),
            message_id: "<msg-1@test>".into(),
            references: String::new(),
            bytes: 100,
            lines: 10,
        },
        XoverEntry {
            article_num: 2,
            subject: "Subject 2".into(),
            from: "poster@example.test".into(),
            date: "2026-07-08".into(),
            message_id: "<msg-2@test>".into(),
            references: String::new(),
            bytes: 200,
            lines: 20,
        },
        XoverEntry {
            article_num: 3,
            subject: "Subject 3".into(),
            from: "poster@example.test".into(),
            date: "2026-07-08".into(),
            message_id: "<msg-3@test>".into(),
            references: String::new(),
            bytes: 300,
            lines: 30,
        },
    ];
    state
        .queue_manager
        .with_db(|db| db.header_insert_batch(group.id, &headers))
        .expect("insert headers");
    let header_ids = state
        .queue_manager
        .with_db(|db| db.header_list(group.id, None, 10, 0))
        .expect("list headers")
        .into_iter()
        .map(|header| header.id)
        .collect();

    (group.id, header_ids)
}

#[tokio::test]
async fn mark_read_marks_requested_headers_in_one_handler_call() {
    let (state, _tempdir) = build_test_state();
    let (group_id, header_ids) = seed_headers(&state);

    let Json(payload) = h_header_mark_read(
        State(state.clone()),
        Path(group_id),
        Json(MarkReadInput {
            header_ids: header_ids[..2].to_vec(),
        }),
    )
    .await
    .expect("handler succeeds");

    assert_eq!(payload["marked"], 2);

    let unread = state
        .queue_manager
        .with_db(|db| db.header_unread_count(group_id))
        .expect("count unread");
    assert_eq!(unread, 1);

    let headers = state
        .queue_manager
        .with_db(|db| db.header_list(group_id, None, 10, 0))
        .expect("list headers");
    let marked_count = headers.iter().filter(|header| header.read).count();
    assert_eq!(marked_count, 2);
}
