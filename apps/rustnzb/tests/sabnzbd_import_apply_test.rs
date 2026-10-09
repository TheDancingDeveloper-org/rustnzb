//! Regression tests for the SABnzbd import apply path (issue #62 follow-up):
//! a relative complete_dir/incomplete_dir must be rejected at apply time,
//! not silently written to config.toml where it later causes a confusing
//! startup crash.

use std::sync::Arc;

use arc_swap::ArcSwap;
use axum::Json;
use axum::extract::State;
use http::StatusCode;
use nzb_web::auth::{CredentialStore, TokenStore};
use nzb_web::log_buffer::LogBuffer;
use nzb_web::nzb_core::config::AppConfig;
use nzb_web::nzb_core::db::Database;
use nzb_web::nzb_core::sabnzbd_import::SabnzbdImportPreview;
use nzb_web::queue_manager::QueueManager;
use nzb_web::state::AppState;
use rustnzb::handlers::h_setup_apply;

fn test_state() -> (Arc<AppState>, tempfile::TempDir) {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let log_buffer = LogBuffer::default();
    let manager = QueueManager::new(
        Vec::new(),
        Database::open_memory().expect("database"),
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

fn preview_with_dirs(complete_dir: &str, incomplete_dir: &str) -> SabnzbdImportPreview {
    serde_json::from_value(serde_json::json!({
        "servers": [],
        "categories": [],
        "general": {
            "api_key": null,
            "complete_dir": complete_dir,
            "incomplete_dir": incomplete_dir,
            "speed_limit_bps": 0
        },
        "rss_feeds": [],
        "warnings": [],
        "skipped_fields": []
    }))
    .expect("preview JSON should deserialize")
}

/// Negative: a relative complete_dir must be rejected, not persisted.
#[tokio::test]
async fn apply_rejects_relative_complete_dir() {
    let (state, _tempdir) = test_state();
    let preview = preview_with_dirs("Downloads", "/downloads/incomplete");

    let error = match h_setup_apply(State(state.clone()), Json(preview)).await {
        Ok(_) => panic!("expected relative complete_dir to be rejected"),
        Err(e) => e,
    };

    assert_eq!(error.status(), StatusCode::BAD_REQUEST);
    assert!(error.to_string().contains("complete_dir"));
    assert_eq!(
        state.config().general.complete_dir,
        AppConfig::default().general.complete_dir,
        "rejected apply must not have mutated the persisted config"
    );
}

/// Negative: a relative incomplete_dir must be rejected, not persisted.
#[tokio::test]
async fn apply_rejects_relative_incomplete_dir() {
    let (state, _tempdir) = test_state();
    let preview = preview_with_dirs("/downloads/complete", "Downloads/incomplete");

    let error = match h_setup_apply(State(state.clone()), Json(preview)).await {
        Ok(_) => panic!("expected relative incomplete_dir to be rejected"),
        Err(e) => e,
    };

    assert_eq!(error.status(), StatusCode::BAD_REQUEST);
    assert!(error.to_string().contains("incomplete_dir"));
    assert_eq!(
        state.config().general.incomplete_dir,
        AppConfig::default().general.incomplete_dir,
        "rejected apply must not have mutated the persisted config"
    );
}

/// Positive: absolute complete_dir/incomplete_dir are applied normally.
#[tokio::test]
async fn apply_accepts_absolute_dirs() {
    let (state, _tempdir) = test_state();
    let preview = preview_with_dirs("/downloads/complete", "/downloads/incomplete");

    h_setup_apply(State(state.clone()), Json(preview))
        .await
        .expect("absolute dirs should be accepted");

    assert_eq!(
        state.config().general.complete_dir,
        std::path::PathBuf::from("/downloads/complete")
    );
    assert_eq!(
        state.config().general.incomplete_dir,
        std::path::PathBuf::from("/downloads/incomplete")
    );
}

fn preview_with_categories(categories: serde_json::Value) -> SabnzbdImportPreview {
    serde_json::from_value(serde_json::json!({
        "servers": [],
        "categories": categories,
        "general": {
            "api_key": null,
            "complete_dir": null,
            "incomplete_dir": null,
            "speed_limit_bps": 0
        },
        "rss_feeds": [],
        "warnings": [],
        "skipped_fields": []
    }))
    .expect("preview JSON should deserialize")
}

/// Negative: imported categories get the same validation as a category
/// save; an unsafe name or output_dir rejects the whole apply with 400,
/// names every offending entry, and changes nothing.
#[tokio::test]
async fn apply_rejects_unsafe_imported_categories() {
    let (state, _tempdir) = test_state();
    let before = state.config().categories.clone();
    let preview = preview_with_categories(serde_json::json!([
        {"name": "tv", "output_dir": "shows/tv", "post_processing": 3},
        {"name": "../escape", "output_dir": null, "post_processing": 3},
        {"name": "movies", "output_dir": "../../etc", "post_processing": 3},
    ]));

    let error = match h_setup_apply(State(state.clone()), Json(preview)).await {
        Ok(_) => panic!("expected unsafe categories to be rejected"),
        Err(e) => e,
    };

    assert_eq!(error.status(), StatusCode::BAD_REQUEST);
    let message = error.to_string();
    assert!(message.contains("../escape"), "{message}");
    assert!(message.contains("movies"), "{message}");
    assert!(!message.contains("'tv'"), "{message}");
    let after = state.config().categories.clone();
    assert_eq!(
        after.iter().map(|c| &c.name).collect::<Vec<_>>(),
        before.iter().map(|c| &c.name).collect::<Vec<_>>(),
        "rejected apply must not have replaced the categories"
    );
}

/// Positive: safe imported categories replace the configured ones.
#[tokio::test]
async fn apply_accepts_safe_imported_categories() {
    let (state, _tempdir) = test_state();
    let preview = preview_with_categories(serde_json::json!([
        {"name": "tv", "output_dir": "shows/tv", "post_processing": 3},
        {"name": "movies", "output_dir": "/srv/media/movies", "post_processing": 3},
    ]));

    h_setup_apply(State(state.clone()), Json(preview))
        .await
        .expect("safe categories should be accepted");

    let names: Vec<_> = state
        .config()
        .categories
        .iter()
        .map(|c| c.name.clone())
        .collect();
    assert_eq!(names, ["tv", "movies"]);
}
