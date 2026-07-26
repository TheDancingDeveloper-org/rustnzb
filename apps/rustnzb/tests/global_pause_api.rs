use std::sync::Arc;

use arc_swap::ArcSwap;
use axum::body::to_bytes;
use axum::extract::{Path, Query, State};
use axum::response::IntoResponse;
use http::StatusCode;
use nzb_web::auth::{CredentialStore, TokenStore};
use nzb_web::log_buffer::LogBuffer;
use nzb_web::nzb_core::config::AppConfig;
use nzb_web::nzb_core::db::Database;
use nzb_web::queue_manager::QueueManager;
use nzb_web::sabnzbd_compat::{SabApiRequest, h_sabnzbd_api_get};
use nzb_web::state::AppState;
use rustnzb::handlers::{h_queue_pause_all, h_queue_resume};

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

#[tokio::test]
async fn individual_resume_endpoint_returns_conflict_during_global_pause() {
    let (state, _tempdir) = test_state();

    let _ = h_queue_pause_all(State(state.clone())).await.unwrap();
    let error = match h_queue_resume(State(state), Path("any-job".to_string())).await {
        Ok(_) => panic!("individual resume unexpectedly succeeded"),
        Err(error) => error,
    };

    assert_eq!(error.status(), StatusCode::CONFLICT);
    assert!(error.to_string().contains("globally paused"));
}

#[tokio::test]
async fn sab_queue_item_resume_reports_failure_during_global_pause() {
    let (state, _tempdir) = test_state();
    let _ = h_queue_pause_all(State(state.clone())).await.unwrap();

    let response = h_sabnzbd_api_get(
        State(state),
        Query(SabApiRequest {
            mode: Some("queue".to_string()),
            name: Some("resume".to_string()),
            value: Some("SABnzbd_nzo_any-job".to_string()),
            ..Default::default()
        }),
    )
    .await
    .unwrap()
    .into_response();
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

    assert_eq!(json["status"], false);
    assert!(json["error"].as_str().unwrap().contains("globally paused"));
}
