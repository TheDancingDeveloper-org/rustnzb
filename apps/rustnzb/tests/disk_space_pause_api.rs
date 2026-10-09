//! The disk-space guard's pause must be distinguishable from a user pause
//! in the native API and the SABnzbd compatibility layer.

use std::sync::Arc;

use arc_swap::ArcSwap;
use axum::body::to_bytes;
use axum::extract::{Query, State};
use axum::response::IntoResponse;
use nzb_web::auth::{CredentialStore, TokenStore};
use nzb_web::log_buffer::LogBuffer;
use nzb_web::nzb_core::config::AppConfig;
use nzb_web::nzb_core::db::Database;
use nzb_web::nzb_core::nzb_parser;
use nzb_web::queue_manager::QueueManager;
use nzb_web::sabnzbd_compat::{SabApiRequest, h_sabnzbd_api_get};
use nzb_web::state::AppState;
use rustnzb::handlers::{QueueQuery, h_queue_list, h_status};

const NZB: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<nzb xmlns="http://www.newzbin.com/DTD/2003/nzb">
  <file poster="mock@rustnzb.test" date="1743465600" subject='"a.bin" yEnc (1/1)'>
    <groups><group>alt.binaries.test</group></groups>
    <segments><segment bytes="100" number="1">a1@rustnzb.test</segment></segments>
  </file>
</nzb>"#;

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

/// Trip the disk guard with one job in the queue; returns the job id.
fn hold_for_disk_space(state: &AppState) -> String {
    let qm = &state.queue_manager;
    qm.pause_all();
    let mut job = nzb_parser::parse_nzb("held", NZB.as_bytes()).expect("parse nzb");
    job.work_dir = qm.incomplete_dir().join(&job.id);
    job.output_dir = qm
        .output_dir_for(&job.category, &job.name)
        .expect("output dir");
    let id = job.id.clone();
    qm.add_job(job, Some(NZB.as_bytes().to_vec()))
        .expect("add job");
    qm.resume_all();
    // No volume can ever have u64::MAX bytes free.
    qm.set_min_free_space(u64::MAX);
    qm.enforce_disk_guard();
    assert!(qm.is_paused());
    id
}

async fn sab(state: Arc<AppState>, mode: &str) -> serde_json::Value {
    let response = h_sabnzbd_api_get(
        State(state),
        Query(SabApiRequest {
            mode: Some(mode.to_string()),
            ..Default::default()
        }),
    )
    .await
    .unwrap()
    .into_response();
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    serde_json::from_slice(&body).unwrap()
}

async fn status_json(state: Arc<AppState>) -> serde_json::Value {
    #[cfg(feature = "webdav")]
    let status = h_status(State(state), axum::Extension(None)).await;
    #[cfg(not(feature = "webdav"))]
    let status = h_status(State(state)).await;
    serde_json::to_value(status.ok().unwrap().0).unwrap()
}

#[tokio::test]
async fn native_queue_and_status_report_disk_space_pause_reason() {
    let (state, _tempdir) = test_state();
    let id = hold_for_disk_space(&state);

    let queue = h_queue_list(State(state.clone()), Query(QueueQuery::default()))
        .await
        .ok()
        .unwrap()
        .0;
    let queue = serde_json::to_value(queue).unwrap();
    assert_eq!(queue["paused"], true);
    assert_eq!(queue["pause_reason"], "disk_space");
    let job = &queue["jobs"][0];
    assert_eq!(job["id"], id.as_str());
    assert_eq!(job["status"], "paused");
    assert_eq!(job["pause_reason"], "disk_space");

    let status = status_json(state.clone()).await;
    assert_eq!(status["paused"], true);
    assert_eq!(status["pause_reason"], "disk_space");

    // Once space frees up the hold lifts and the reason clears.
    state.queue_manager.set_min_free_space(1);
    state.queue_manager.enforce_disk_guard();
    let status = status_json(state.clone()).await;
    assert_eq!(status["paused"], false);
    assert!(status["pause_reason"].is_null());
}

#[tokio::test]
async fn native_queue_reports_user_pause_as_global() {
    let (state, _tempdir) = test_state();
    state.queue_manager.pause_all();
    let queue = h_queue_list(State(state.clone()), Query(QueueQuery::default()))
        .await
        .ok()
        .unwrap()
        .0;
    let queue = serde_json::to_value(queue).unwrap();
    assert_eq!(queue["pause_reason"], "global");
}

#[tokio::test]
async fn sab_reports_a_disk_space_warning_while_the_guard_holds_downloads() {
    let (state, _tempdir) = test_state();

    let queue = sab(state.clone(), "queue").await;
    assert_eq!(queue["queue"]["have_warnings"], "0");

    hold_for_disk_space(&state);

    let queue = sab(state.clone(), "queue").await;
    assert_eq!(queue["queue"]["status"], "Paused");
    assert_eq!(queue["queue"]["paused"], true);
    assert_eq!(queue["queue"]["have_warnings"], "1");

    let full = sab(state.clone(), "fullstatus").await;
    assert_eq!(full["status"]["have_warnings"], "1");
    let warning = &full["status"]["warnings"][0];
    assert_eq!(warning["type"], "WARNING");
    assert!(warning["text"].as_str().unwrap().contains("diskspace"));
    assert!(warning["time"].is_i64());

    let warnings = sab(state.clone(), "warnings").await;
    assert_eq!(warnings["warnings"][0], *warning);

    state.queue_manager.set_min_free_space(1);
    state.queue_manager.enforce_disk_guard();
    let full = sab(state, "fullstatus").await;
    assert_eq!(full["status"]["have_warnings"], "0");
    assert_eq!(full["status"]["warnings"], serde_json::json!([]));
}
