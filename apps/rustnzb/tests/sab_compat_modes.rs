//! SABnzbd-compatible API modes that main serves, checked over real HTTP:
//! `mode=warnings` (read and `name=clear`), the `mode=change_complete_action`
//! no-op that *arr clients send, and the unknown-mode error shape.
//!
//! Pending PR #200 (WI-1719), not yet on main, and deliberately not covered
//! here: `mode=addlocalfile`, and post-processing script arguments and
//! environment. Add them to this file once that PR lands.

use std::sync::Arc;

use arc_swap::ArcSwap;
use nzb_web::auth::{CredentialStore, TokenStore};
use nzb_web::nzb_core::config::AppConfig;
use nzb_web::nzb_core::db::Database;
use nzb_web::{AppState, LogBuffer, QueueManager};
use rustnzb::server::build_router;
use serde_json::Value;

/// A running router over a real socket, aborted when dropped.
struct CompatServer {
    base_url: String,
    handle: tokio::task::JoinHandle<()>,
    _tempdir: tempfile::TempDir,
}

impl Drop for CompatServer {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

async fn start_compat_server() -> CompatServer {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let config = AppConfig::default();
    let log_buffer = LogBuffer::new();
    let manager = QueueManager::new(
        Vec::new(),
        Database::open_memory().expect("database"),
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
    CompatServer {
        base_url,
        handle,
        _tempdir: tempdir,
    }
}

async fn sab_call(server: &CompatServer, query: &str) -> Value {
    let url = format!("{}/sabnzbd/api?output=json&{query}", server.base_url);
    let response = reqwest::get(&url).await.expect("sab request");
    assert!(
        response.status().is_success(),
        "sab call failed for {query:?}"
    );
    response.json().await.expect("sab json")
}

#[tokio::test]
async fn warnings_is_an_empty_list_on_a_fresh_server() {
    let server = start_compat_server().await;

    let body = sab_call(&server, "mode=warnings").await;
    assert_eq!(body, serde_json::json!({ "warnings": [] }));
}

#[tokio::test]
async fn warnings_clear_is_acknowledged() {
    let server = start_compat_server().await;

    let cleared = sab_call(&server, "mode=warnings&name=clear").await;
    assert_eq!(cleared, serde_json::json!({ "status": true }));

    // Clearing does not invent warnings: the list is still empty afterwards.
    let after = sab_call(&server, "mode=warnings").await;
    assert_eq!(after, serde_json::json!({ "warnings": [] }));
}

#[tokio::test]
async fn change_complete_action_refuses_power_actions_and_accepts_a_clear() {
    let server = start_compat_server().await;

    // RustNZB cannot act on the host, so power actions are refused rather
    // than silently ignored.
    let body = sab_call(&server, "mode=change_complete_action&value=shutdown").await;
    assert_eq!(
        body,
        serde_json::json!({
            "status": false,
            "error": "unsupported complete action 'shutdown': only script_<name> is supported",
        })
    );

    // An empty value clears the action.
    let cleared = sab_call(&server, "mode=change_complete_action&value=").await;
    assert_eq!(cleared, serde_json::json!({ "status": true }));
}

#[tokio::test]
async fn unknown_mode_reports_an_error_naming_the_mode() {
    let server = start_compat_server().await;

    let body = sab_call(&server, "mode=no_such_mode").await;
    assert_eq!(body["status"], false);
    assert_eq!(body["error"], "Unknown mode: no_such_mode");
}
