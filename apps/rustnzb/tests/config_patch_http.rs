//! PATCH /api/config: registry validation, If-Match, locks and step-up.

mod support;

use std::sync::Arc;
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;
use nzb_web::auth::{CredentialStore, TokenStore};
use nzb_web::nzb_core::config::AppConfig;
use nzb_web::nzb_core::db::Database;
use nzb_web::nzb_core::overrides::{OverrideSet, OverrideSource, OverrideValue};
use nzb_web::{AppState, LogBuffer, QueueManager};
use reqwest::StatusCode;
use rustnzb::server::{build_router, into_make_service};
use support::start_test_server_with_overrides;

async fn access_token(client: &reqwest::Client, base_url: &str) -> String {
    let setup = client
        .post(format!("{base_url}/api/auth/setup"))
        .json(&serde_json::json!({
            "username": "patch-test",
            "password": "patch-test-password"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(setup.status(), StatusCode::OK);
    setup.json::<serde_json::Value>().await.unwrap()["access_token"]
        .as_str()
        .unwrap()
        .to_string()
}

async fn etag(client: &reqwest::Client, base_url: &str, token: &str) -> String {
    let response = client
        .get(format!("{base_url}/api/config/effective"))
        .bearer_auth(token)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    response
        .headers()
        .get(reqwest::header::ETAG)
        .expect("ETag")
        .to_str()
        .unwrap()
        .to_string()
}

fn error_kind(body: &serde_json::Value) -> &str {
    body["error_kind"].as_str().unwrap_or("")
}

#[tokio::test]
async fn patch_persists_a_live_setting_and_reports_restart_keys() {
    let app = start_test_server_with_overrides(OverrideSet::default()).await;
    let client = reqwest::Client::new();
    let token = access_token(&client, &app.base_url).await;
    let tag = etag(&client, &app.base_url, &token).await;

    let live = client
        .patch(format!("{}/api/config", app.base_url))
        .bearer_auth(&token)
        .header(reqwest::header::IF_MATCH, &tag)
        .json(&serde_json::json!({
            "settings": { "general.max_active_downloads": 3 }
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(live.status(), StatusCode::OK);
    let body: serde_json::Value = live.json().await.unwrap();
    assert_eq!(
        body["applied"],
        serde_json::json!(["general.max_active_downloads"])
    );
    assert_eq!(body["restart_required"], serde_json::json!([]));
    let on_disk = AppConfig::load(&app.config_path).unwrap();
    assert_eq!(on_disk.general.max_active_downloads, 3);

    let tag = etag(&client, &app.base_url, &token).await;
    let restart = client
        .patch(format!("{}/api/config", app.base_url))
        .bearer_auth(&token)
        .header(reqwest::header::IF_MATCH, &tag)
        .json(&serde_json::json!({
            "settings": { "general.shutdown_timeout_secs": 12 }
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(restart.status(), StatusCode::OK);
    let body: serde_json::Value = restart.json().await.unwrap();
    assert_eq!(
        body["restart_required"],
        serde_json::json!(["general.shutdown_timeout_secs"])
    );
    let on_disk = AppConfig::load(&app.config_path).unwrap();
    assert_eq!(on_disk.general.shutdown_timeout_secs, 12);
}

#[tokio::test]
async fn missing_if_match_is_428_and_stale_is_412() {
    let app = start_test_server_with_overrides(OverrideSet::default()).await;
    let client = reqwest::Client::new();
    let token = access_token(&client, &app.base_url).await;
    let before = AppConfig::load(&app.config_path)
        .unwrap()
        .general
        .max_active_downloads;

    let missing = client
        .patch(format!("{}/api/config", app.base_url))
        .bearer_auth(&token)
        .json(&serde_json::json!({
            "settings": { "general.max_active_downloads": 4 }
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(missing.status(), StatusCode::PRECONDITION_REQUIRED);
    let body: serde_json::Value = missing.json().await.unwrap();
    assert_eq!(error_kind(&body), "precondition_required");

    let stale = client
        .patch(format!("{}/api/config", app.base_url))
        .bearer_auth(&token)
        .header(reqwest::header::IF_MATCH, "\"nope\"")
        .json(&serde_json::json!({
            "settings": { "general.max_active_downloads": 4 }
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(stale.status(), StatusCode::PRECONDITION_FAILED);
    let body: serde_json::Value = stale.json().await.unwrap();
    assert_eq!(error_kind(&body), "precondition_failed");

    let on_disk = AppConfig::load(&app.config_path).unwrap();
    assert_eq!(on_disk.general.max_active_downloads, before);
}

#[tokio::test]
async fn locked_setting_names_the_source() {
    let mut overrides = OverrideSet::default();
    overrides.set(OverrideValue::Port(7771), OverrideSource::Cli);
    let app = start_test_server_with_overrides(overrides).await;
    let client = reqwest::Client::new();
    let token = access_token(&client, &app.base_url).await;
    let tag = etag(&client, &app.base_url, &token).await;
    let before = AppConfig::load(&app.config_path).unwrap().general.port;

    let response = client
        .patch(format!("{}/api/config", app.base_url))
        .bearer_auth(&token)
        .header(reqwest::header::IF_MATCH, &tag)
        .header("X-Confirm-Password", "patch-test-password")
        .json(&serde_json::json!({
            "settings": { "general.port": 8088 }
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);
    let body: serde_json::Value = response.json().await.unwrap();
    let text = body["human_readable"].as_str().unwrap_or("");
    assert!(text.contains("cli"), "{text}");
    assert!(text.contains("general.port"), "{text}");
    assert_eq!(
        AppConfig::load(&app.config_path).unwrap().general.port,
        before
    );
}

#[tokio::test]
async fn invalid_value_is_400_and_names_the_key() {
    let app = start_test_server_with_overrides(OverrideSet::default()).await;
    let client = reqwest::Client::new();
    let token = access_token(&client, &app.base_url).await;
    let tag = etag(&client, &app.base_url, &token).await;

    let response = client
        .patch(format!("{}/api/config", app.base_url))
        .bearer_auth(&token)
        .header(reqwest::header::IF_MATCH, &tag)
        .json(&serde_json::json!({
            "settings": { "general.port": 0 }
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body: serde_json::Value = response.json().await.unwrap();
    let text = body["human_readable"].as_str().unwrap_or("");
    assert!(text.contains("general.port"), "{text}");
}

#[tokio::test]
async fn sensitive_setting_requires_step_up() {
    let app = start_test_server_with_overrides(OverrideSet::default()).await;
    let client = reqwest::Client::new();
    let token = access_token(&client, &app.base_url).await;
    let tag = etag(&client, &app.base_url, &token).await;
    let before = AppConfig::load(&app.config_path)
        .unwrap()
        .general
        .data_dir
        .clone();

    let denied = client
        .patch(format!("{}/api/config", app.base_url))
        .bearer_auth(&token)
        .header(reqwest::header::IF_MATCH, &tag)
        .json(&serde_json::json!({
            "settings": { "general.data_dir": "/tmp/rustnzb-patch-data" }
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(denied.status(), StatusCode::FORBIDDEN);
    let body: serde_json::Value = denied.json().await.unwrap();
    assert_eq!(error_kind(&body), "step_up_required");
    assert_eq!(
        AppConfig::load(&app.config_path).unwrap().general.data_dir,
        before
    );

    let allowed = client
        .patch(format!("{}/api/config", app.base_url))
        .bearer_auth(&token)
        .header(reqwest::header::IF_MATCH, &tag)
        .header("X-Confirm-Password", "patch-test-password")
        .json(&serde_json::json!({
            "settings": { "general.data_dir": "/tmp/rustnzb-patch-data" }
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(allowed.status(), StatusCode::OK, "{allowed:?}");
    assert_eq!(
        AppConfig::load(&app.config_path).unwrap().general.data_dir,
        std::path::PathBuf::from("/tmp/rustnzb-patch-data")
    );
}

async fn api_key(client: &reqwest::Client, base_url: &str, token: &str) -> String {
    let minted = client
        .post(format!("{base_url}/api/config/sabnzbd-api-key/rotate"))
        .bearer_auth(token)
        .json(&serde_json::json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(
        minted.status(),
        StatusCode::FORBIDDEN,
        "rotate is sensitive and needs a session"
    );
    let minted = client
        .post(format!("{base_url}/api/config/sabnzbd-api-key/rotate"))
        .bearer_auth(token)
        .header("X-Confirm-Password", "patch-test-password")
        .json(&serde_json::json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(minted.status(), StatusCode::OK, "{minted:?}");
    minted.json::<serde_json::Value>().await.unwrap()["api_key"]
        .as_str()
        .unwrap()
        .to_string()
}

#[tokio::test]
async fn api_key_cannot_patch_registry_sensitive_keys() {
    let app = start_test_server_with_overrides(OverrideSet::default()).await;
    let client = reqwest::Client::new();
    let token = access_token(&client, &app.base_url).await;
    let key = api_key(&client, &app.base_url, &token).await;
    let before = AppConfig::load(&app.config_path).unwrap();
    let cases = [
        ("general.trusted_proxies", serde_json::json!(["10.0.0.0/8"])),
        (
            "general.local_import_dir",
            serde_json::json!("/tmp/rustnzb-patch-import"),
        ),
        ("general.step_up_ttl_secs", serde_json::json!(60)),
        (
            "notifications.webhook_url",
            serde_json::json!("https://example.invalid/hook"),
        ),
        (
            "notifications.apprise_api_url",
            serde_json::json!("https://example.invalid/apprise"),
        ),
        (
            "notifications.apprise_urls",
            serde_json::json!("https://example.invalid/notify"),
        ),
        ("notifications.ntfy_token", serde_json::json!("ntfy-token")),
    ];
    for (setting, value) in cases {
        let tag = etag(&client, &app.base_url, &token).await;
        let denied = client
            .patch(format!("{}/api/config", app.base_url))
            .header("X-Api-Key", &key)
            .header(reqwest::header::IF_MATCH, &tag)
            .header("X-Confirm-Password", "patch-test-password")
            .json(&serde_json::json!({ "settings": { setting: value } }))
            .send()
            .await
            .unwrap();
        assert_eq!(denied.status(), StatusCode::FORBIDDEN, "{setting}");
        let body: serde_json::Value = denied.json().await.unwrap();
        assert_eq!(error_kind(&body), "step_up_forbidden", "{setting}");
    }
    let after = AppConfig::load(&app.config_path).unwrap();
    assert_eq!(
        after.general.trusted_proxies,
        before.general.trusted_proxies
    );
    assert_eq!(
        after.general.local_import_dir,
        before.general.local_import_dir
    );
    assert_eq!(
        after.general.step_up_ttl_secs,
        before.general.step_up_ttl_secs
    );
    assert_eq!(
        after.notifications.webhook_url,
        before.notifications.webhook_url
    );
    assert_eq!(
        after.notifications.apprise_api_url,
        before.notifications.apprise_api_url
    );
    assert_eq!(
        after.notifications.apprise_urls,
        before.notifications.apprise_urls
    );
    assert_eq!(
        after.notifications.ntfy_token,
        before.notifications.ntfy_token
    );
}

#[tokio::test]
async fn loopback_port_change_does_not_need_exposure_confirm() {
    let app = start_test_server_with_overrides(OverrideSet::default()).await;
    let client = reqwest::Client::new();
    let token = access_token(&client, &app.base_url).await;

    let tag = etag(&client, &app.base_url, &token).await;
    let loopback = client
        .patch(format!("{}/api/config", app.base_url))
        .bearer_auth(&token)
        .header(reqwest::header::IF_MATCH, &tag)
        .header("X-Confirm-Password", "patch-test-password")
        .json(&serde_json::json!({
            "settings": { "general.listen_addr": "127.0.0.1" }
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        loopback.status(),
        StatusCode::OK,
        "{}",
        loopback.text().await.unwrap()
    );

    let tag = etag(&client, &app.base_url, &token).await;
    let current = AppConfig::load(&app.config_path).unwrap().general.port;
    let port_only = client
        .patch(format!("{}/api/config", app.base_url))
        .bearer_auth(&token)
        .header(reqwest::header::IF_MATCH, &tag)
        .header("X-Confirm-Password", "patch-test-password")
        .json(&serde_json::json!({
            "settings": { "general.port": current.wrapping_add(1) }
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        port_only.status(),
        StatusCode::OK,
        "{}",
        port_only.text().await.unwrap()
    );

    let tag = etag(&client, &app.base_url, &token).await;
    let exposed = client
        .patch(format!("{}/api/config", app.base_url))
        .bearer_auth(&token)
        .header(reqwest::header::IF_MATCH, &tag)
        .header("X-Confirm-Password", "patch-test-password")
        .json(&serde_json::json!({
            "settings": { "general.listen_addr": "0.0.0.0" }
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(exposed.status(), StatusCode::BAD_REQUEST);
    let body: serde_json::Value = exposed.json().await.unwrap();
    assert!(
        body["human_readable"]
            .as_str()
            .unwrap_or("")
            .contains("confirm_exposure"),
        "{body}"
    );

    let tag = etag(&client, &app.base_url, &token).await;
    let confirmed = client
        .patch(format!("{}/api/config", app.base_url))
        .bearer_auth(&token)
        .header(reqwest::header::IF_MATCH, &tag)
        .header("X-Confirm-Password", "patch-test-password")
        .json(&serde_json::json!({
            "confirm_exposure": true,
            "settings": { "general.listen_addr": "0.0.0.0" }
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        confirmed.status(),
        StatusCode::OK,
        "{}",
        confirmed.text().await.unwrap()
    );
    assert_eq!(
        AppConfig::load(&app.config_path)
            .unwrap()
            .general
            .listen_addr,
        "0.0.0.0"
    );
}

/// A server that records each connection's peer address (`ConnectInfo`), so
/// the step-up throttle sees a client and `X-Forwarded-For` is honoured.
/// Served through `into_make_service`, as the real server is.
struct PeerApp {
    base_url: String,
    state: Arc<AppState>,
    _tmp: tempfile::TempDir,
    handle: tokio::task::JoinHandle<()>,
}

impl Drop for PeerApp {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

async fn start_peer_app() -> PeerApp {
    let tmp = tempfile::tempdir().unwrap();
    let config_path = tmp.path().join("config.toml");
    let mut config = AppConfig::default();
    // The test client connects from 127.0.0.1, so trusting the loopback range
    // lets each request name its own client through `X-Forwarded-For`.
    config.general.trusted_proxies = vec!["127.0.0.0/8".into(), "::1".into()];
    config.save(&config_path).unwrap();
    let logs = LogBuffer::new();
    let queue = QueueManager::new(
        Vec::new(),
        Database::open_memory().unwrap(),
        tmp.path().join("incomplete"),
        tmp.path().join("complete"),
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
    let state = Arc::new(AppState::new(
        Arc::new(ArcSwap::from_pointee(config)),
        config_path,
        queue,
        logs,
        Arc::new(TokenStore::new()),
        Arc::new(CredentialStore::new(tmp.path().to_path_buf())),
    ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base_url = format!("http://{}", listener.local_addr().unwrap());
    let router_state = state.clone();
    let handle = tokio::spawn(async move {
        axum::serve(listener, into_make_service(build_router(router_state)))
            .await
            .unwrap();
    });
    PeerApp {
        base_url,
        state,
        _tmp: tmp,
        handle,
    }
}

/// One wrong-confirm-password PATCH of a sensitive setting from `client_ip`,
/// returning how long the answer was held.
async fn wrong_confirm(app: &PeerApp, token: &str, tag: &str, client_ip: &'static str) -> Duration {
    let started = Instant::now();
    let response = reqwest::Client::new()
        .patch(format!("{}/api/config", app.base_url))
        .bearer_auth(token)
        .header(reqwest::header::IF_MATCH, tag)
        .header("X-Forwarded-For", client_ip)
        .header("X-Confirm-Password", "not-the-password")
        .json(&serde_json::json!({
            "settings": { "general.data_dir": "/tmp/rustnzb-patch-per-client" }
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN, "{client_ip}");
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(error_kind(&body), "step_up_required");
    started.elapsed()
}

#[tokio::test]
async fn wrong_confirm_password_is_throttled_per_client_not_globally() {
    let app = start_peer_app().await;
    let client = reqwest::Client::new();
    let token = access_token(&client, &app.base_url).await;
    let tag = etag(&client, &app.base_url, &token).await;

    let a = std::net::IpAddr::from([203, 0, 113, 1]);
    let b = std::net::IpAddr::from([203, 0, 113, 2]);

    // Client A burns three attempts. Each failure doubles the hold from the
    // 500 ms base, so A is now firmly throttled.
    for _ in 0..3 {
        wrong_confirm(&app, &token, &tag, "203.0.113.1").await;
    }
    assert_eq!(
        app.state.credential_store.failed_attempts(a),
        3,
        "A's failures must be counted against A"
    );

    // Client B is a different address and has made no attempt: it must not
    // inherit A's count, and its first wrong password is held for the base
    // delay only, never for A's accumulated hold.
    assert_eq!(
        app.state.credential_store.failed_attempts(b),
        0,
        "B must start with no failures"
    );
    let held = wrong_confirm(&app, &token, &tag, "203.0.113.2").await;
    assert!(
        held < Duration::from_millis(1500),
        "B must not inherit A's throttle, held {held:?}"
    );
    assert_eq!(
        app.state.credential_store.failed_attempts(b),
        1,
        "B's own wrong password is counted against B"
    );
    assert_eq!(
        app.state.credential_store.failed_attempts(a),
        3,
        "B's attempt must not touch A's bucket"
    );

    // A's next attempt is still held far longer than B's first: the buckets
    // really are separate.
    let held = wrong_confirm(&app, &token, &tag, "203.0.113.1").await;
    assert!(
        held >= Duration::from_secs(2),
        "A must stay throttled after exhausting its attempts, held {held:?}"
    );
}
