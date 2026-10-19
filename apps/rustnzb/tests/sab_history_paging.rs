//! `GET /sabnzbd/api?mode=history` pages over the summary rows over real
//! HTTP: `start` and `limit` select the window, `noofslots` counts every
//! match (not just the page), `search` filters before paging, and only the
//! returned page is rebuilt from full rows. Sibling of
//! `history_pagination_api.rs`, which covers the native `/api/history` route.

use std::sync::Arc;

use arc_swap::ArcSwap;
use chrono::{Duration, Utc};
use nzb_web::auth::{CredentialStore, TokenStore};
use nzb_web::nzb_core::config::AppConfig;
use nzb_web::nzb_core::db::Database;
use nzb_web::nzb_core::models::{HistoryEntry, JobFailureCode, JobStatus};
use nzb_web::{AppState, LogBuffer, QueueManager};
use rustnzb::server::build_router;
use serde_json::Value;

/// Entry `i` completes `i` minutes before entry `0`, so SAB's newest-first
/// order is `Release.000`, `Release.001`, ... and the page order is known.
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
        par2_volumes_fetched: None,
        par2_volumes_available: None,
    }
}

/// Same shape as the native paging test: 100 completed `tv`, 15 failed
/// `movies` (all recent), then 5 completed `old` entries from 60 days ago.
fn seeded_database() -> Database {
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
    db
}

/// A running router over a real socket, aborted when dropped.
struct HistoryServer {
    base_url: String,
    handle: tokio::task::JoinHandle<()>,
    _tempdir: tempfile::TempDir,
}

impl Drop for HistoryServer {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

async fn start_history_server(db: Database) -> HistoryServer {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let config = AppConfig::default();
    let log_buffer = LogBuffer::new();
    let manager = QueueManager::new(
        Vec::new(),
        db,
        tempdir.path().join("incomplete"),
        tempdir.path().join("complete"),
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
        manager,
        log_buffer,
        Arc::new(TokenStore::new()),
        Arc::new(CredentialStore::new(tempdir.path().to_path_buf())),
    ));
    let router = build_router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind listener");
    let base_url = format!(
        "http://{}",
        listener.local_addr().expect("listener address")
    );
    let handle = tokio::spawn(async move {
        axum::serve(listener, router).await.expect("serve test app");
    });
    HistoryServer {
        base_url,
        handle,
        _tempdir: tempdir,
    }
}

async fn history_page(server: &HistoryServer, query: &str) -> Value {
    let url = format!(
        "{}/sabnzbd/api?mode=history&output=json&{query}",
        server.base_url
    );
    let response = reqwest::get(&url).await.expect("history request");
    assert!(
        response.status().is_success(),
        "history failed for {query:?}"
    );
    let body: Value = response.json().await.expect("history json");
    body["history"].clone()
}

/// The `name` of every slot on the page, in the order SAB returns them.
fn names(history: &Value) -> Vec<String> {
    history["slots"]
        .as_array()
        .expect("slots array")
        .iter()
        .map(|slot| slot["name"].as_str().expect("slot name").to_string())
        .collect()
}

fn release_names(range: std::ops::Range<usize>) -> Vec<String> {
    range.map(|i| format!("Release.{i:03}")).collect()
}

#[tokio::test]
async fn limit_and_start_select_the_window_and_noofslots_counts_everything() {
    let server = start_history_server(seeded_database()).await;

    let first = history_page(&server, "limit=10").await;
    assert_eq!(names(&first), release_names(0..10));
    assert_eq!(first["noofslots"], 120, "noofslots must count all history");

    let middle = history_page(&server, "start=10&limit=10").await;
    assert_eq!(names(&middle), release_names(10..20));
    assert_eq!(middle["noofslots"], 120);

    let last = history_page(&server, "start=115&limit=10").await;
    assert_eq!(names(&last), release_names(115..120), "final page is short");
    assert_eq!(last["noofslots"], 120);

    let past_end = history_page(&server, "start=500&limit=10").await;
    assert!(names(&past_end).is_empty());
    assert_eq!(past_end["noofslots"], 120);
}

#[tokio::test]
async fn omitted_or_zero_limit_falls_back_to_fifty() {
    let server = start_history_server(seeded_database()).await;

    let default = history_page(&server, "").await;
    assert_eq!(names(&default), release_names(0..50));

    let zero = history_page(&server, "limit=0").await;
    assert_eq!(
        names(&zero),
        release_names(0..50),
        "limit=0 means the default page size, not an empty page"
    );
    assert_eq!(zero["noofslots"], 120);
}

#[tokio::test]
async fn consecutive_pages_cover_every_entry_exactly_once() {
    let server = start_history_server(seeded_database()).await;

    let mut seen = Vec::new();
    let mut start = 0;
    loop {
        let page = history_page(&server, &format!("start={start}&limit=7")).await;
        let batch = names(&page);
        if batch.is_empty() {
            break;
        }
        start += batch.len();
        seen.extend(batch);
    }

    assert_eq!(
        seen,
        release_names(0..120),
        "pages must not overlap or skip"
    );
}

#[tokio::test]
async fn search_filters_before_paging_and_counts_the_matches() {
    let server = start_history_server(seeded_database()).await;

    // `release.11` matches Release.110 through Release.119 only.
    let search = history_page(&server, "search=release.11&limit=50").await;
    assert_eq!(names(&search), release_names(110..120));
    assert_eq!(search["noofslots"], 10);

    let paged = history_page(&server, "search=release.11&start=4&limit=3").await;
    assert_eq!(names(&paged), release_names(114..117));
    assert_eq!(paged["noofslots"], 10, "noofslots is the filtered count");

    let case_insensitive = history_page(&server, "search=RELEASE.11&limit=50").await;
    assert_eq!(names(&case_insensitive), release_names(110..120));
}

#[tokio::test]
async fn search_with_no_match_is_an_empty_page_not_an_error() {
    let server = start_history_server(seeded_database()).await;

    let none = history_page(&server, "search=no-such-release&limit=10").await;
    assert!(names(&none).is_empty());
    assert_eq!(none["noofslots"], 0);
}

#[tokio::test]
async fn search_and_status_combine_and_page_over_the_intersection() {
    let server = start_history_server(seeded_database()).await;

    // Failed entries are Release.100 through Release.114; `release.10`
    // matches Release.100 through Release.109, so the intersection is 100-109.
    let combined = history_page(&server, "search=release.10&status=failed&limit=5").await;
    assert_eq!(names(&combined), release_names(100..105));
    assert_eq!(combined["noofslots"], 10);

    let second = history_page(&server, "search=release.10&status=failed&start=5&limit=5").await;
    assert_eq!(names(&second), release_names(105..110));
    assert_eq!(second["noofslots"], 10);
}

#[tokio::test]
async fn paged_slots_carry_full_row_fields_loaded_for_that_page_only() {
    let server = start_history_server(seeded_database()).await;

    // Release.100 is the first failed entry; it sits on the second page.
    let page = history_page(&server, "start=100&limit=1").await;
    let slot = &page["slots"][0];
    assert_eq!(slot["name"], "Release.100");
    assert_eq!(slot["status"], "Failed");
    assert_eq!(slot["fail_message"], "CRC mismatch: bad");
    assert_eq!(slot["bytes"], 1000);
}

#[tokio::test]
async fn archive_only_request_has_no_matches() {
    let server = start_history_server(seeded_database()).await;

    let archived = history_page(&server, "archive=1&limit=10").await;
    assert!(names(&archived).is_empty());
    assert_eq!(archived["noofslots"], 0);
}

#[tokio::test]
async fn unchanged_last_history_update_short_circuits_the_page() {
    let server = start_history_server(seeded_database()).await;

    let fresh = history_page(&server, "limit=5").await;
    let generation = fresh["last_history_update"]
        .as_u64()
        .expect("last_history_update");

    let unchanged = history_page(
        &server,
        &format!("limit=5&last_history_update={generation}"),
    )
    .await;
    assert_eq!(unchanged, serde_json::json!(false));
}
