//! BUG-108 / BUG-111: group, header and thread listings with extreme
//! pagination values.

use std::sync::Arc;

use arc_swap::ArcSwap;
use axum::Json;
use axum::extract::{Query, State};
use nzb_web::auth::{CredentialStore, TokenStore};
use nzb_web::nzb_core::config::AppConfig;
use nzb_web::nzb_core::db::Database;
use nzb_web::nzb_core::nzb_nntp::XoverEntry;
use nzb_web::{AppState, QueueManager};
use rustnzb::group_handlers::{
    GroupListQuery, HeaderListQuery, IdPath, MAX_PAGE_LIMIT, h_group_list, h_header_list,
    h_thread_list,
};
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
        config.general.max_nested_archive_depth,
        config.general.abort_hopeless,
        config.general.early_failure_check,
        config.general.required_completion_pct,
        config.general.article_timeout_secs,
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

fn seed(state: &AppState) -> i64 {
    let qm = &state.queue_manager;
    qm.with_db(|db| db.group_upsert_batch(&[("alt.binaries.test".to_string(), 1, 1)]))
        .unwrap();
    let group_id = qm
        .with_db(|db| db.group_list(false, None, 10, 0))
        .unwrap()
        .pop()
        .unwrap()
        .id;
    qm.with_db(|db| {
        db.header_insert_batch(
            group_id,
            &[XoverEntry {
                article_num: 1,
                subject: "Subject".into(),
                from: "poster@example.test".into(),
                date: "2026-07-08".into(),
                message_id: "<m1@test>".into(),
                references: String::new(),
                bytes: 1,
                lines: 1,
            }],
        )
    })
    .unwrap();
    group_id
}

/// `limit=18446744073709551615` used to reach SQLite as an out-of-range
/// integer literal and fail with a 500 "datatype mismatch".
#[tokio::test]
async fn huge_limit_is_clamped_instead_of_failing() {
    let (state, _tempdir) = build_test_state();
    let group_id = seed(&state);

    let Json(groups) = h_group_list(
        State(state.clone()),
        Query(GroupListQuery {
            limit: Some(usize::MAX),
            offset: Some(usize::MAX),
            ..Default::default()
        }),
    )
    .await
    .expect("group list");
    assert_eq!(groups["limit"], MAX_PAGE_LIMIT);
    assert_eq!(groups["total"], 1);

    let Json(headers) = h_header_list(
        State(state.clone()),
        IdPath(group_id),
        Query(HeaderListQuery {
            limit: Some(usize::MAX),
            search: Some("Subject".into()),
            ..Default::default()
        }),
    )
    .await
    .expect("header list");
    assert_eq!(headers["limit"], MAX_PAGE_LIMIT);
    assert_eq!(headers["total"], 1);
    assert_eq!(headers["headers"].as_array().unwrap().len(), 1);

    let Json(threads) = h_thread_list(
        State(state.clone()),
        IdPath(group_id),
        Query(HeaderListQuery {
            limit: Some(usize::MAX),
            ..Default::default()
        }),
    )
    .await
    .expect("thread list");
    assert_eq!(threads["total"], 1);
}
