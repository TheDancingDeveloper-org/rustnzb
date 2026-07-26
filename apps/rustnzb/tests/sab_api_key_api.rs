use std::sync::Arc;

use arc_swap::ArcSwap;
use nzb_web::auth::{CredentialStore, StoredCredentials, TokenStore};
use nzb_web::nzb_core::config::AppConfig;
use nzb_web::nzb_core::db::Database;
use nzb_web::{AppState, LogBuffer, QueueManager};
use rustnzb::server::build_router;

async fn start_test_server() -> (
    String,
    Arc<AppState>,
    tempfile::TempDir,
    tokio::task::JoinHandle<()>,
) {
    let tempdir = tempfile::tempdir().expect("create tempdir");
    let mut config = AppConfig::default();
    config.general.api_key = Some("original-sab-key".into());

    let log_buffer = LogBuffer::new();
    let manager = QueueManager::new(
        Vec::new(),
        Database::open_memory().expect("open database"),
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
    let credential_store = Arc::new(CredentialStore::new(tempdir.path().to_path_buf()));
    credential_store
        .set_credentials(StoredCredentials {
            username: "admin".into(),
            password: "correct horse battery staple".into(),
        })
        .expect("store credentials");
    let state = Arc::new(AppState::new(
        Arc::new(ArcSwap::from_pointee(config)),
        tempdir.path().join("config.toml"),
        manager,
        log_buffer,
        Arc::new(TokenStore::new()),
        credential_store,
    ));
    let router = build_router(state.clone());
    #[cfg(feature = "webdav")]
    let router = router.layer(axum::Extension(None::<Arc<rustnzb::dav::DavHandle>>));

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

    (base_url, state, tempdir, handle)
}

#[tokio::test]
async fn sab_api_key_is_admin_only_and_rotation_takes_effect_immediately() {
    let (base_url, state, _tempdir, handle) = start_test_server().await;
    let client = reqwest::Client::new();

    let unauthorized = client
        .get(format!("{base_url}/api/config/sabnzbd-api-key"))
        .send()
        .await
        .expect("unauthenticated request");
    assert_eq!(unauthorized.status(), reqwest::StatusCode::UNAUTHORIZED);

    let initial = client
        .get(format!("{base_url}/api/config/sabnzbd-api-key"))
        .basic_auth("admin", Some("correct horse battery staple"))
        .send()
        .await
        .expect("read current key");
    assert!(initial.status().is_success());
    assert_eq!(
        initial
            .json::<serde_json::Value>()
            .await
            .expect("key response")["api_key"],
        "original-sab-key"
    );

    let rotated = client
        .post(format!("{base_url}/api/config/sabnzbd-api-key/rotate"))
        .basic_auth("admin", Some("correct horse battery staple"))
        .json(&serde_json::json!({}))
        .send()
        .await
        .expect("rotate key");
    assert!(rotated.status().is_success());
    let new_key = rotated
        .json::<serde_json::Value>()
        .await
        .expect("rotation response")["api_key"]
        .as_str()
        .expect("rotated key")
        .to_string();
    assert_ne!(new_key, "original-sab-key");
    assert_eq!(new_key.len(), 32);
    assert_eq!(
        state.config().general.api_key.as_deref(),
        Some(new_key.as_str())
    );

    // Native header authentication consults the live config, not the key that
    // was present when the router was created.
    let old_native_key = client
        .get(format!("{base_url}/api/config/sabnzbd-api-key"))
        .header("X-Api-Key", "original-sab-key")
        .send()
        .await
        .expect("old native key request");
    assert_eq!(old_native_key.status(), reqwest::StatusCode::UNAUTHORIZED);
    let new_native_key = client
        .get(format!("{base_url}/api/config/sabnzbd-api-key"))
        .header("X-Api-Key", &new_key)
        .send()
        .await
        .expect("new native key request");
    assert!(new_native_key.status().is_success());

    let old_sab_key = client
        .get(format!(
            "{base_url}/sabnzbd/api?mode=queue&apikey=original-sab-key"
        ))
        .send()
        .await
        .expect("old SAB key request")
        .json::<serde_json::Value>()
        .await
        .expect("old SAB response");
    assert_eq!(old_sab_key["status"], false);
    assert_eq!(old_sab_key["error"], "API Key Incorrect");

    let new_sab_key = client
        .get(format!(
            "{base_url}/sabnzbd/api?mode=queue&apikey={new_key}"
        ))
        .send()
        .await
        .expect("new SAB key request")
        .json::<serde_json::Value>()
        .await
        .expect("new SAB response");
    assert!(new_sab_key.get("queue").is_some());

    handle.abort();
}
