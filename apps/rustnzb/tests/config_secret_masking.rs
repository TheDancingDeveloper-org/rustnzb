//! Config reads never return a stored secret, and config writes honour the
//! mask / absent / `null` rules.

use std::sync::Arc;

use arc_swap::ArcSwap;
use nzb_web::auth::{CredentialStore, StoredCredentials, TokenStore};
use nzb_web::nzb_core::config::{AppConfig, ServerConfig};
use nzb_web::nzb_core::db::Database;
use nzb_web::{AppState, LogBuffer, QueueManager};
use rustnzb::server::build_router;

const USER: &str = "admin";
const PASS: &str = "correct horse battery staple";
const API_KEY: &str = "seeded-api-key-7f3a";
const SERVER_PASSWORD: &str = "seeded-server-pass";
const PROXY_URL: &str = "socks5://proxyuser:proxysecret@proxy.example.net:1080";

async fn start_test_server(
    config: AppConfig,
) -> (
    String,
    Arc<AppState>,
    tempfile::TempDir,
    tokio::task::JoinHandle<()>,
) {
    let tempdir = tempfile::tempdir().expect("create tempdir");
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
            username: USER.into(),
            password: PASS.into(),
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

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind listener");
    let base_url = format!("http://{}", listener.local_addr().expect("address"));
    let handle = tokio::spawn(async move {
        axum::serve(listener, router).await.expect("serve test app");
    });
    (base_url, state, tempdir, handle)
}

fn seeded_config() -> AppConfig {
    let mut config = AppConfig::default();
    config.general.api_key = Some(API_KEY.into());
    config.general.cache_size = 64 * 1024 * 1024;
    config.general.log_file = Some("/var/log/rustnzb.log".into());
    let mut server = ServerConfig::new("srv-1", "news.example.com");
    server.password = Some(SERVER_PASSWORD.into());
    server.proxy_url = Some(PROXY_URL.into());
    config.servers.push(server);
    #[cfg(feature = "webdav")]
    {
        config.dav.password = Some("seeded-dav-pass".into());
        config.dav.api_key = Some("seeded-dav-key".into());
        config.dav.username = Some("davuser".into());
    }
    config
}

fn assert_no_secret(body: &str) {
    for secret in [API_KEY, SERVER_PASSWORD, "proxysecret", "seeded-dav"] {
        assert!(!body.contains(secret), "config read leaked `{secret}`");
    }
}

#[tokio::test]
async fn get_config_never_returns_stored_secrets() {
    let (base_url, _state, _tempdir, handle) = start_test_server(seeded_config()).await;
    let client = reqwest::Client::new();

    let response = client
        .get(format!("{base_url}/api/config"))
        .basic_auth(USER, Some(PASS))
        .send()
        .await
        .expect("get config");
    assert!(response.status().is_success());
    let body = response.text().await.expect("config body");
    assert_no_secret(&body);

    let json: serde_json::Value = serde_json::from_str(&body).expect("json");
    assert_eq!(json["general"]["api_key"], "********");
    assert_eq!(json["servers"][0]["password"], "********");
    assert_eq!(
        json["servers"][0]["proxy_url"],
        "socks5://********@proxy.example.net:1080"
    );
    // Dead settings are hidden from the API surface.
    assert!(json["general"].get("cache_size").is_none());
    assert!(json["general"].get("log_file").is_none());

    handle.abort();
}

#[cfg(feature = "webdav")]
#[tokio::test]
async fn get_dav_config_masks_password_and_api_key() {
    let (base_url, _state, _tempdir, handle) = start_test_server(seeded_config()).await;
    let client = reqwest::Client::new();

    let response = client
        .get(format!("{base_url}/api/config/dav"))
        .basic_auth(USER, Some(PASS))
        .send()
        .await
        .expect("get dav config");
    assert!(response.status().is_success());
    let body = response.text().await.expect("dav body");
    assert_no_secret(&body);

    let json: serde_json::Value = serde_json::from_str(&body).expect("json");
    assert_eq!(json["password"], "********");
    assert_eq!(json["api_key"], "********");
    assert_eq!(json["username"], "davuser");

    handle.abort();
}

#[cfg(feature = "webdav")]
#[tokio::test]
async fn put_dav_config_keeps_masked_or_absent_secrets_and_null_clears() {
    let (base_url, state, _tempdir, handle) = start_test_server(seeded_config()).await;
    let client = reqwest::Client::new();
    let url = format!("{base_url}/api/config/dav");

    // A client round-trips what it read: the masks keep the stored values.
    let masked = client
        .put(&url)
        .basic_auth(USER, Some(PASS))
        .json(&serde_json::json!({
            "enabled": true,
            "username": "davuser",
            "password": "********",
            "api_key": "********",
        }))
        .send()
        .await
        .expect("put masked");
    assert!(masked.status().is_success());
    let dav = state.config().dav.clone();
    assert!(dav.enabled);
    assert_eq!(dav.password.as_deref(), Some("seeded-dav-pass"));
    assert_eq!(dav.api_key.as_deref(), Some("seeded-dav-key"));

    // Absent keys keep the stored values too.
    let absent = client
        .put(&url)
        .basic_auth(USER, Some(PASS))
        .json(&serde_json::json!({ "enabled": true, "username": "davuser" }))
        .send()
        .await
        .expect("put absent");
    assert!(absent.status().is_success());
    let dav = state.config().dav.clone();
    assert_eq!(dav.password.as_deref(), Some("seeded-dav-pass"));
    assert_eq!(dav.api_key.as_deref(), Some("seeded-dav-key"));

    // An explicit null clears the secret.
    let cleared = client
        .put(&url)
        .basic_auth(USER, Some(PASS))
        .json(&serde_json::json!({
            "enabled": true,
            "username": "davuser",
            "password": null,
            "api_key": null,
        }))
        .send()
        .await
        .expect("put null");
    assert!(cleared.status().is_success());
    let dav = state.config().dav.clone();
    assert!(dav.password.is_none());
    assert!(dav.api_key.is_none());

    handle.abort();
}

#[tokio::test]
async fn put_server_masked_proxy_keeps_stored_url_and_null_password_clears() {
    let (base_url, state, _tempdir, handle) = start_test_server(seeded_config()).await;
    let client = reqwest::Client::new();
    let url = format!("{base_url}/api/config/servers/srv-1");

    // The masked proxy URL and the mask password round-trip without change.
    let masked = client
        .put(&url)
        .basic_auth(USER, Some(PASS))
        .json(&serde_json::json!({
            "password": "********",
            "proxy_url": "socks5://********@proxy.example.net:1080",
        }))
        .send()
        .await
        .expect("put masked server");
    assert!(masked.status().is_success());
    let server = state.config().servers[0].clone();
    assert_eq!(server.password.as_deref(), Some(SERVER_PASSWORD));
    assert_eq!(server.proxy_url.as_deref(), Some(PROXY_URL));

    // An explicit null clears the server password.
    let cleared = client
        .put(&url)
        .basic_auth(USER, Some(PASS))
        .json(&serde_json::json!({ "password": null }))
        .send()
        .await
        .expect("put null server");
    assert!(cleared.status().is_success());
    assert!(state.config().servers[0].password.is_none());

    handle.abort();
}
