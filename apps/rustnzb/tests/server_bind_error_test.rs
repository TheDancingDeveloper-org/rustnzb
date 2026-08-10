//! Regression tests for issue #62: startup failures must surface actionable
//! context (the failing address/path) instead of a bare `os error 13`.

use std::path::PathBuf;
use std::sync::Arc;

use arc_swap::ArcSwap;
use nzb_web::auth::{CredentialStore, TokenStore};
use nzb_web::nzb_core::config::AppConfig;
use nzb_web::nzb_core::db::Database;
use nzb_web::{AppState, QueueManager};
use rustnzb::server::build_router;

async fn build_state(config: AppConfig) -> Arc<AppState> {
    let db = Database::open_memory().expect("Failed to create in-memory database");
    let tmp_dir = tempfile::tempdir().expect("Failed to create temp dir");
    let incomplete_dir = tmp_dir.path().join("incomplete");
    let complete_dir = tmp_dir.path().join("complete");
    std::fs::create_dir_all(&incomplete_dir).expect("Failed to create incomplete dir");
    std::fs::create_dir_all(&complete_dir).expect("Failed to create complete dir");

    let log_buffer = nzb_web::LogBuffer::new();
    let qm = QueueManager::new(
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
    let token_store = Arc::new(TokenStore::new());
    let credential_store = Arc::new(CredentialStore::new(tmp_dir.path().to_path_buf()));
    // Leak the tempdir so it outlives the returned state for the duration of the test.
    std::mem::forget(tmp_dir);

    Arc::new(AppState::new(
        Arc::new(ArcSwap::from_pointee(config)),
        PathBuf::from("config.toml"),
        qm,
        log_buffer,
        token_store,
        credential_store,
    ))
}

/// Negative: binding to an address already occupied by another listener must
/// fail with an error whose message identifies the address, not a bare
/// `os error` string (see apps/rustnzb/src/server.rs `serve`).
#[tokio::test]
async fn serve_reports_context_on_bind_failure() {
    // Occupy a free port first, keeping the listener alive for the test.
    let occupied = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("failed to occupy a port for the test");
    let addr = occupied.local_addr().expect("failed to read local addr");

    let mut config = AppConfig::default();
    config.general.listen_addr = addr.ip().to_string();
    config.general.port = addr.port();

    let state = build_state(config).await;
    let router = build_router(state.clone());
    #[cfg(feature = "webdav")]
    let router = router.layer(axum::Extension(None::<Arc<rustnzb::dav::DavHandle>>));

    let result = rustnzb::server::serve(state, router).await;
    let err = match result {
        Ok(()) => panic!(
            "expected serve() to fail: port {} is already bound",
            addr.port()
        ),
        Err(e) => e,
    };

    let debug_text = format!("{err:?}");
    assert!(
        debug_text.contains(&addr.port().to_string()),
        "error should mention the failing bind address, got: {debug_text}"
    );
    assert!(
        debug_text.to_lowercase().contains("bind"),
        "error should mention it was a bind failure, got: {debug_text}"
    );

    drop(occupied);
}

/// Positive: binding to a free port succeeds, and the server can be reached.
#[tokio::test]
async fn serve_succeeds_on_free_port() {
    let mut config = AppConfig::default();
    config.general.listen_addr = "127.0.0.1".to_string();
    config.general.port = 0; // OS-assigned free port

    let state = build_state(config).await;
    let router = build_router(state.clone());
    #[cfg(feature = "webdav")]
    let router = router.layer(axum::Extension(None::<Arc<rustnzb::dav::DavHandle>>));

    let handle = tokio::spawn(async move { rustnzb::server::serve(state, router).await });

    // Give the server a moment to either bind successfully or fail fast.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert!(
        !handle.is_finished(),
        "serve() should still be running (bind succeeded)"
    );

    handle.abort();
}
