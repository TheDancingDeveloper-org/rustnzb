//! GET /api/history pages through the full history: `total` counts every
//! matching entry (not just the page), `offset` pages, filters run
//! server-side, and the summary stats cover the whole time window.

use std::sync::Arc;

use arc_swap::ArcSwap;
use axum::extract::{Query, State};
use chrono::{Duration, Utc};
use nzb_web::auth::{CredentialStore, TokenStore};
use nzb_web::nzb_core::config::AppConfig;
use nzb_web::nzb_core::db::Database;
use nzb_web::nzb_core::models::{HistoryEntry, JobFailureCode, JobStatus};
use nzb_web::{AppState, LogBuffer, QueueManager};
use rustnzb::handlers::{HistoryQuery, h_history_list};
use serde_json::Value;

fn history(i: usize, status: JobStatus, category: &str, age_days: i64) -> HistoryEntry {
    let completed_at = Utc::now() - Duration::days(age_days) - Duration::minutes(i as i64);
    HistoryEntry {
        id: format!("h{i:03}"),
        name: format!("Release.{i:03}"),
        category: category.to_string(),
        status,
        total_bytes: 1000,
        downloaded_bytes: 1000,
        added_at: completed_at - Duration::seconds(60),
        completed_at,
        download_time_secs: None,
        output_dir: "/tmp/out".into(),
        stages: Vec::new(),
        error_message: (status == JobStatus::Failed).then(|| "CRC mismatch: bad".to_string()),
        failure_code: (status == JobStatus::Failed).then_some(JobFailureCode::ArchiveInvalid),
        post_processing: None,
        delete_archives: None,
        server_stats: Vec::new(),
        nzb_data: None,
        retry_data: None,
    }
}

/// 120 entries: 100 completed `tv` (recent), 15 failed `movies` (recent),
/// 5 completed `old` entries completed 60 days ago.
fn state_with_history() -> (Arc<AppState>, tempfile::TempDir) {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let db = Database::open_memory().expect("database");
    let mut i = 0;
    for _ in 0..100 {
        db.history_insert(&history(i, JobStatus::Completed, "tv", 0))
            .unwrap();
        i += 1;
    }
    for _ in 0..15 {
        db.history_insert(&history(i, JobStatus::Failed, "movies", 0))
            .unwrap();
        i += 1;
    }
    for _ in 0..5 {
        db.history_insert(&history(i, JobStatus::Completed, "old", 60))
            .unwrap();
        i += 1;
    }
    let log_buffer = LogBuffer::default();
    let manager = QueueManager::new(
        Vec::new(),
        db,
        tempdir.path().join("incomplete"),
        tempdir.path().join("complete"),
        log_buffer.clone(),
        1,
        Vec::new(),
        0,
        0,
        false,
        5,
        false,
        false,
        100.0,
        30,
    );
    let state = AppState::new(
        Arc::new(ArcSwap::from_pointee(AppConfig::default())),
        tempdir.path().join("config.toml"),
        manager,
        log_buffer,
        Arc::new(TokenStore::new()),
        Arc::new(CredentialStore::new(tempdir.path().to_path_buf())),
    );
    (Arc::new(state), tempdir)
}

async fn list(state: &Arc<AppState>, query: &str) -> Value {
    let uri: axum::http::Uri = format!("/api/history?{query}").parse().expect("uri");
    let q: Query<HistoryQuery> = Query::try_from_uri(&uri).expect("query");
    let resp = h_history_list(State(state.clone()), q)
        .await
        .unwrap_or_else(|_| panic!("history list failed for {query:?}"));
    serde_json::to_value(&resp.0).expect("json")
}

fn ids(v: &Value) -> Vec<String> {
    v["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["id"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test]
async fn total_counts_all_entries_and_offset_pages() {
    let (state, _dir) = state_with_history();

    let first = list(&state, "").await;
    assert_eq!(first["entries"].as_array().unwrap().len(), 50);
    assert_eq!(
        first["total"], 120,
        "total must count all history, not the page"
    );
    assert_eq!(first["offset"], 0);
    assert_eq!(first["limit"], 50);

    let third = list(&state, "offset=100&limit=50").await;
    assert_eq!(ids(&third).len(), 20);
    assert_eq!(third["total"], 120);
    assert_eq!(ids(&third)[0], "h100");

    let past_end = list(&state, "offset=500").await;
    assert!(ids(&past_end).is_empty());
    assert_eq!(past_end["total"], 120);
}

#[tokio::test]
async fn filters_apply_before_paging() {
    let (state, _dir) = state_with_history();

    let failed = list(&state, "status=failed&limit=10").await;
    assert_eq!(failed["total"], 15);
    assert_eq!(ids(&failed).len(), 10);

    let movies = list(&state, "category=movies&offset=10").await;
    assert_eq!(movies["total"], 15);
    assert_eq!(ids(&movies).len(), 5);

    let search = list(&state, "search=release.11").await;
    assert_eq!(
        ids(&search),
        vec![
            "h110", "h111", "h112", "h113", "h114", "h115", "h116", "h117", "h118", "h119"
        ]
    );

    let recent = list(&state, "days=7").await;
    assert_eq!(recent["total"], 115);
}

#[tokio::test]
async fn categories_and_stats_cover_the_whole_window_not_the_page() {
    let (state, _dir) = state_with_history();

    let all = list(&state, "limit=5").await;
    assert_eq!(
        all["categories"],
        serde_json::json!(["movies", "old", "tv"])
    );
    let stats = &all["stats"];
    assert_eq!(stats["completed"], 105);
    assert_eq!(stats["failed"], 15);
    assert_eq!(stats["completed_bytes"], 105_000);
    assert_eq!(stats["success_pct"], 88);
    assert_eq!(stats["avg_duration_secs"], 60.0);
    assert_eq!(
        stats["fail_reasons"],
        serde_json::json!([{ "reason": "CRC mismatch", "count": 15 }])
    );

    // The stats honour the time window but ignore status/category/name, so
    // the success rate stays meaningful while the table is filtered.
    let week = list(&state, "days=7&status=failed&limit=1").await;
    assert_eq!(week["stats"]["completed"], 100);
    assert_eq!(week["stats"]["failed"], 15);
    assert_eq!(week["stats"]["success_pct"], 87);
}

#[tokio::test]
async fn huge_days_query_is_rejected_instead_of_panicking() {
    let (state, _dir) = state_with_history();
    let uri: axum::http::Uri = "/api/history?days=100000000".parse().expect("uri");
    let q: Query<HistoryQuery> = Query::try_from_uri(&uri).expect("query");
    match h_history_list(State(state), q).await {
        Err(err) => assert_eq!(err.status(), axum::http::StatusCode::BAD_REQUEST),
        Ok(_) => panic!("an overflowing days window must be rejected, not listed"),
    }
}
