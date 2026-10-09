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
async fn first_boot_only_exposes_setup_and_setup_is_single_use() {
    let app = start_app(false).await;
    let client = reqwest::Client::new();

    assert_eq!(
        client
            .get(format!("{}/api/status", app.base_url))
            .send()
            .await
            .unwrap()
            .status(),
        reqwest::StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        client
            .get(format!("{}/api/setup/status", app.base_url))
            .send()
            .await
            .unwrap()
            .status(),
        reqwest::StatusCode::OK
    );

    let setup = client
        .post(format!("{}/api/auth/setup", app.base_url))
        .json(&serde_json::json!({"username":"owner","password":"secret"}))
        .send()
        .await
        .unwrap();
    assert_eq!(setup.status(), reqwest::StatusCode::OK);
    let access = setup.json::<serde_json::Value>().await.unwrap()["access_token"]
        .as_str()
        .unwrap()
        .to_string();

    assert_eq!(
        client
            .get(format!("{}/api/status", app.base_url))
            .bearer_auth(&access)
            .send()
            .await
            .unwrap()
            .status(),
        reqwest::StatusCode::OK
    );
    assert_eq!(
        client
            .post(format!("{}/api/auth/setup", app.base_url))
            .json(&serde_json::json!({"username":"other","password":"secret"}))
            .send()
            .await
            .unwrap()
            .status(),
        reqwest::StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn config_routes_validate_duplicates_and_persist_successful_updates() {
    let app = start_app(false).await;
    let client = reqwest::Client::new();
    let setup = client
        .post(format!("{}/api/auth/setup", app.base_url))
        .json(&serde_json::json!({"username":"owner","password":"secret"}))
        .send()
        .await
        .unwrap();
    assert_eq!(setup.status(), reqwest::StatusCode::OK);
    let access = setup.json::<serde_json::Value>().await.unwrap()["access_token"]
        .as_str()
        .unwrap()
        .to_string();
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
            .bearer_auth(&access)
            .json(&server)
            .send()
            .await
            .unwrap()
            .status(),
        reqwest::StatusCode::OK
    );
    let servers = client
        .get(format!("{}/api/config/servers", app.base_url))
        .bearer_auth(&access)
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
            .bearer_auth(&access)
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
            .bearer_auth(&access)
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
            .bearer_auth(&access)
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
            .bearer_auth(&access)
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
            .bearer_auth(&access)
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

async fn setup_access(app: &ContractApp, client: &reqwest::Client) -> String {
    let setup = client
        .post(format!("{}/api/auth/setup", app.base_url))
        .json(&serde_json::json!({"username":"owner","password":"secret"}))
        .send()
        .await
        .unwrap();
    assert_eq!(setup.status(), reqwest::StatusCode::OK);
    setup.json::<serde_json::Value>().await.unwrap()["access_token"]
        .as_str()
        .unwrap()
        .to_string()
}

fn set_fetch_policy(app: &ContractApp, allow_private: bool, allowed_hosts: &[&str]) {
    let mut config = (*app.state.config()).clone();
    config.general.fetch_allow_private = allow_private;
    config.general.fetch_allowed_hosts = allowed_hosts.iter().map(|h| h.to_string()).collect();
    app.state.update_config(config).unwrap();
}

/// Serve one HTTP response with `body` on a loopback port and return its URL.
async fn serve_once(body: Vec<u8>) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = [0; 2048];
        let _ = socket.read(&mut request).await;
        let head = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        let _ = socket.write_all(head.as_bytes()).await;
        let _ = socket.write_all(&body).await;
        let _ = socket.shutdown().await;
    });
    format!("http://{address}/release.nzb")
}

#[tokio::test]
async fn add_url_reaches_lan_indexer_only_when_allowed() {
    let app = start_app(false).await;
    let client = reqwest::Client::new();
    let access = setup_access(&app, &client).await;

    // Default policy: a loopback/LAN indexer is refused before any request.
    let refused = client
        .post(format!("{}/api/queue/add-url", app.base_url))
        .bearer_auth(&access)
        .json(&serde_json::json!({"url": "http://127.0.0.1:9/release.nzb"}))
        .send()
        .await
        .unwrap();
    assert!(!refused.status().is_success());
    assert!(refused.text().await.unwrap().contains("private/reserved"));

    set_fetch_policy(&app, false, &["127.0.0.0/8"]);
    let url = serve_once(support::sample_nzb_bytes()).await;
    let added = client
        .post(format!("{}/api/queue/add-url", app.base_url))
        .bearer_auth(&access)
        .json(&serde_json::json!({ "url": url }))
        .send()
        .await
        .unwrap();
    let status = added.status();
    assert!(
        status.is_success(),
        "{status}: {}",
        added.text().await.unwrap()
    );
}

#[tokio::test]
async fn rss_feed_url_is_validated_when_saved() {
    let app = start_app(false).await;
    let client = reqwest::Client::new();
    let access = setup_access(&app, &client).await;
    let feed = |url: &str| serde_json::json!({"name":"lan", "url": url, "enabled": true});

    let rejected = client
        .post(format!("{}/api/config/rss-feeds", app.base_url))
        .bearer_auth(&access)
        .json(&feed("http://192.168.1.10/api?t=rss"))
        .send()
        .await
        .unwrap();
    assert_eq!(rejected.status(), reqwest::StatusCode::BAD_REQUEST);
    assert!(rejected.text().await.unwrap().contains("Feed URL rejected"));
    assert!(app.state.config().rss_feeds.is_empty());

    set_fetch_policy(&app, true, &[]);
    assert_eq!(
        client
            .post(format!("{}/api/config/rss-feeds", app.base_url))
            .bearer_auth(&access)
            .json(&feed("http://192.168.1.10/api?t=rss"))
            .send()
            .await
            .unwrap()
            .status(),
        reqwest::StatusCode::OK
    );

    // Link-local metadata stays blocked even with private ranges allowed.
    assert_eq!(
        client
            .put(format!("{}/api/config/rss-feeds/lan", app.base_url))
            .bearer_auth(&access)
            .json(&feed("http://169.254.169.254/latest"))
            .send()
            .await
            .unwrap()
            .status(),
        reqwest::StatusCode::BAD_REQUEST
    );
    assert_eq!(
        app.state.config().rss_feeds[0].url,
        "http://192.168.1.10/api?t=rss"
    );
}

async fn login(app: &ContractApp, client: &reqwest::Client) -> (String, String) {
    let tokens = client
        .post(format!("{}/api/auth/login", app.base_url))
        .json(&serde_json::json!({"username":"admin","password":"password"}))
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap();
    (
        tokens["access_token"].as_str().unwrap().to_string(),
        tokens["refresh_token"].as_str().unwrap().to_string(),
    )
}

async fn status_with(app: &ContractApp, client: &reqwest::Client, access: &str) -> u16 {
    client
        .get(format!("{}/api/status", app.base_url))
        .bearer_auth(access)
        .send()
        .await
        .unwrap()
        .status()
        .as_u16()
}

#[tokio::test]
async fn logout_revokes_the_access_token() {
    let app = start_app(true).await;
    let client = reqwest::Client::new();

    // The web UI posts only the refresh token; the paired access token must die too.
    let (access, refresh) = login(&app, &client).await;
    let (other_access, _) = login(&app, &client).await;
    assert_eq!(status_with(&app, &client, &access).await, 200);
    let logout = client
        .post(format!("{}/api/auth/logout", app.base_url))
        .json(&serde_json::json!({ "refresh_token": refresh }))
        .send()
        .await
        .unwrap();
    assert_eq!(logout.status(), reqwest::StatusCode::NO_CONTENT);
    assert_eq!(status_with(&app, &client, &access).await, 401);
    assert_eq!(status_with(&app, &client, &other_access).await, 200);

    // A client that presents its access token in the header is logged out as well.
    let (access, refresh) = login(&app, &client).await;
    let logout = client
        .post(format!("{}/api/auth/logout", app.base_url))
        .bearer_auth(&access)
        .json(&serde_json::json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(logout.status(), reqwest::StatusCode::NO_CONTENT);
    assert_eq!(status_with(&app, &client, &access).await, 401);
    assert_eq!(
        client
            .post(format!("{}/api/auth/refresh", app.base_url))
            .json(&serde_json::json!({ "refresh_token": refresh }))
            .send()
            .await
            .unwrap()
            .status(),
        reqwest::StatusCode::UNAUTHORIZED
    );
}
