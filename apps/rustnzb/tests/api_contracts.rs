//! Deterministic in-process API contract coverage for authentication,
//! validation, response shape, and configuration persistence.

mod support;

use std::sync::Arc;

use arc_swap::ArcSwap;
use nzb_web::auth::{CredentialStore, StoredCredentials, TokenStore};
use nzb_web::nzb_core::config::AppConfig;
use nzb_web::nzb_core::db::Database;
use nzb_web::{AppState, LogBuffer, QueueManager};
use rustnzb::server::build_router;

struct ContractApp {
    base_url: String,
    config_path: std::path::PathBuf,
    state: Arc<AppState>,
    _temp: tempfile::TempDir,
    handle: tokio::task::JoinHandle<()>,
}

impl Drop for ContractApp {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

async fn start_app(with_credentials: bool) -> ContractApp {
    let temp = tempfile::tempdir().unwrap();
    let config_path = temp.path().join("config.toml");
    let config = AppConfig::default();
    config.save(&config_path).unwrap();
    let logs = LogBuffer::new();
    let queue = QueueManager::new(
        Vec::new(),
        Database::open_memory().unwrap(),
        temp.path().join("incomplete"),
        temp.path().join("complete"),
        logs.clone(),
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
    let credentials = Arc::new(CredentialStore::new(temp.path().to_path_buf()));
    if with_credentials {
        credentials
            .set_credentials(StoredCredentials {
                username: "admin".into(),
                password: "password".into(),
            })
            .unwrap();
    }
    let state = Arc::new(AppState::new(
        Arc::new(ArcSwap::from_pointee(config)),
        config_path.clone(),
        queue,
        logs,
        Arc::new(TokenStore::new()),
        credentials,
    ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base_url = format!("http://{}", listener.local_addr().unwrap());
    let router_state = state.clone();
    let handle = tokio::spawn(async move {
        axum::serve(listener, build_router(router_state))
            .await
            .unwrap();
    });

    ContractApp {
        base_url,
        config_path,
        state,
        _temp: temp,
        handle,
    }
}

#[tokio::test]
async fn protected_routes_reject_missing_credentials_and_preserve_auth_contracts() {
    let app = start_app(true).await;
    let client = reqwest::Client::new();

    assert_eq!(
        client
            .get(format!("{}/api/config/servers", app.base_url))
            .send()
            .await
            .unwrap()
            .status(),
        reqwest::StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        client
            .get(format!("{}/api/health", app.base_url))
            .send()
            .await
            .unwrap()
            .status(),
        reqwest::StatusCode::OK
    );
    let bad_login = client
        .post(format!("{}/api/auth/login", app.base_url))
        .json(&serde_json::json!({"username":"admin","password":"wrong"}))
        .send()
        .await
        .unwrap();
    assert_eq!(bad_login.status(), reqwest::StatusCode::UNAUTHORIZED);

    let tokens = client
        .post(format!("{}/api/auth/login", app.base_url))
        .json(&serde_json::json!({"username":"admin","password":"password"}))
        .send()
        .await
        .unwrap();
    assert_eq!(tokens.status(), reqwest::StatusCode::OK);
    let tokens = tokens.json::<serde_json::Value>().await.unwrap();
    let access = tokens["access_token"].as_str().unwrap();
    let refresh = tokens["refresh_token"].as_str().unwrap();
    assert_eq!(tokens["token_type"], "Bearer");
    assert_eq!(tokens["expires_in"], 900);

    assert_eq!(
        client
            .get(format!("{}/api/config/servers", app.base_url))
            .bearer_auth(access)
            .send()
            .await
            .unwrap()
            .status(),
        reqwest::StatusCode::OK
    );
    let rotated = client
        .post(format!("{}/api/auth/refresh", app.base_url))
        .json(&serde_json::json!({"refresh_token": refresh}))
        .send()
        .await
        .unwrap();
    assert_eq!(rotated.status(), reqwest::StatusCode::OK);
    assert_eq!(
        client
            .post(format!("{}/api/auth/refresh", app.base_url))
            .json(&serde_json::json!({"refresh_token": refresh}))
            .send()
            .await
            .unwrap()
            .status(),
        reqwest::StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn config_routes_validate_duplicates_and_persist_successful_updates() {
    let app = start_app(false).await;
    let client = reqwest::Client::new();
    let server = serde_json::json!({
        "id":"", "name":"Primary", "host":" news.example.test ", "port":563,
        "ssl":true, "ssl_verify":true, "username":"", "password":"", "connections":8,
        "priority":0, "enabled":true, "retention":0, "pipelining":1, "optional":false,
        "compress":false, "ramp_up_delay_ms":50, "recv_buffer_size":2097152,
        "proxy_url":null, "trusted_fingerprint":null, "connect_timeout_secs":30
    });
    assert_eq!(
        client
            .post(format!("{}/api/config/servers", app.base_url))
            .json(&server)
            .send()
            .await
            .unwrap()
            .status(),
        reqwest::StatusCode::OK
    );
    let servers = client
        .get(format!("{}/api/config/servers", app.base_url))
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap();
    assert_eq!(servers.as_array().unwrap().len(), 1);
    assert_eq!(servers[0]["host"], "news.example.test");
    assert_eq!(servers[0]["username"], "");

    let category =
        serde_json::json!({"name":"tv", "output_dir":"/downloads/tv", "post_processing":3});
    assert_eq!(
        client
            .post(format!("{}/api/config/categories", app.base_url))
            .json(&category)
            .send()
            .await
            .unwrap()
            .status(),
        reqwest::StatusCode::OK
    );
    assert_eq!(
        client
            .post(format!("{}/api/config/categories", app.base_url))
            .json(&category)
            .send()
            .await
            .unwrap()
            .status(),
        reqwest::StatusCode::INTERNAL_SERVER_ERROR
    );

    let feed = serde_json::json!({"name":"daily", "url":"https://example.test/feed", "poll_interval_secs":60, "category":"tv", "filter_regex":null, "enabled":true, "auto_download":false});
    assert_eq!(
        client
            .post(format!("{}/api/config/rss-feeds", app.base_url))
            .json(&feed)
            .send()
            .await
            .unwrap()
            .status(),
        reqwest::StatusCode::OK
    );
    assert_eq!(
        client
            .put(format!("{}/api/config/speed-limit", app.base_url))
            .json(&serde_json::json!({"speed_limit_bps":1234}))
            .send()
            .await
            .unwrap()
            .status(),
        reqwest::StatusCode::OK
    );
    assert_eq!(
        client
            .get(format!("{}/api/config/speed-limit", app.base_url))
            .send()
            .await
            .unwrap()
            .json::<serde_json::Value>()
            .await
            .unwrap()["speed_limit_bps"],
        1234
    );

    let saved = AppConfig::load(&app.config_path).unwrap();
    assert_eq!(saved.servers.len(), 1);
    assert_eq!(
        saved
            .categories
            .iter()
            .map(|c| c.name.as_str())
            .collect::<Vec<_>>(),
        vec!["Default", "tv"]
    );
    assert_eq!(saved.rss_feeds[0].name, "daily");
    assert_eq!(saved.general.speed_limit_bps, 1234);
    assert_eq!(app.state.config().general.speed_limit_bps, 1234);
}
