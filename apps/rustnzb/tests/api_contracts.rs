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

/// Send a request and return (status, parsed JSON body or Null).
async fn call(request: reqwest::RequestBuilder) -> (u16, serde_json::Value) {
    let response = request.send().await.unwrap();
    let status = response.status().as_u16();
    let text = response.text().await.unwrap();
    (
        status,
        serde_json::from_str(&text).unwrap_or(serde_json::Value::Null),
    )
}

#[tokio::test]
async fn unknown_ids_return_404_and_invalid_input_returns_400() {
    let app = start_app(true).await;
    let client = reqwest::Client::new();
    let (access, _) = login(&app, &client).await;
    let base = &app.base_url;

    // BUG-51: retry of an unknown history entry.
    let (status, body) = call(
        client
            .post(format!("{base}/api/history/no-such-id/retry"))
            .bearer_auth(&access),
    )
    .await;
    assert_eq!(status, 404, "{body}");
    assert_eq!(body["error_kind"], "not_found");

    // BUG-52: out-of-range priority is a 400, unknown job a 404.
    let (status, body) = call(
        client
            .put(format!("{base}/api/queue/no-such-id/priority"))
            .bearer_auth(&access)
            .json(&serde_json::json!({"priority": 999})),
    )
    .await;
    assert_eq!(status, 400, "{body}");
    assert_eq!(body["error_kind"], "bad_request");
    let (status, body) = call(
        client
            .put(format!("{base}/api/queue/no-such-id/priority"))
            .bearer_auth(&access)
            .json(&serde_json::json!({"priority": 2})),
    )
    .await;
    assert_eq!(status, 404, "{body}");
    assert_eq!(body["error_kind"], "job_not_found");

    // BUG-55: move of an unknown job, logs of an unknown history entry.
    let (status, body) = call(
        client
            .post(format!("{base}/api/queue/no-such-id/move"))
            .bearer_auth(&access)
            .json(&serde_json::json!({"position": 0})),
    )
    .await;
    assert_eq!(status, 404, "{body}");
    let (status, body) = call(
        client
            .get(format!("{base}/api/history/no-such-id/logs"))
            .bearer_auth(&access),
    )
    .await;
    assert_eq!(status, 404, "{body}");

    // BUG-55: non-numeric group id is a JSON 400 without parser internals.
    let response = client
        .get(format!("{base}/api/groups/not-a-number"))
        .bearer_auth(&access)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 400);
    let text = response.text().await.unwrap();
    assert!(!text.contains("i64"), "{text}");
    let body: serde_json::Value = serde_json::from_str(&text).expect("JSON error body");
    assert_eq!(body["error_kind"], "bad_request");

    // BUG-56: invalid regex is a 400; updating an unknown rule is a 404;
    // deleting an unknown rule stays idempotent.
    let rule = |regex: &str| serde_json::json!({"name": "r", "feed_names": ["nope"], "match_regex": regex});
    let (status, body) = call(
        client
            .post(format!("{base}/api/rss/rules"))
            .bearer_auth(&access)
            .json(&rule("(unclosed")),
    )
    .await;
    assert_eq!(status, 400, "{body}");
    let (status, body) = call(
        client
            .put(format!("{base}/api/rss/rules/no-such-rule"))
            .bearer_auth(&access)
            .json(&rule(".*")),
    )
    .await;
    assert_eq!(status, 404, "{body}");
    assert!(app.state.queue_manager.rss_rule_list().unwrap().is_empty());
    let (status, _) = call(
        client
            .delete(format!("{base}/api/rss/rules/no-such-rule"))
            .bearer_auth(&access),
    )
    .await;
    assert_eq!(status, 200);

    // A rule whose feed does not exist is accepted with a warning.
    let (status, body) = call(
        client
            .post(format!("{base}/api/rss/rules"))
            .bearer_auth(&access)
            .json(&rule(".*")),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["status"], true);
    assert!(
        body["warnings"][0].as_str().unwrap().contains("nope"),
        "{body}"
    );
}

#[tokio::test]
async fn server_update_keeps_password_unless_a_new_one_is_sent() {
    let app = start_app(false).await;
    let client = reqwest::Client::new();
    let access = setup_access(&app, &client).await;
    let server = |password: serde_json::Value| {
        let mut server = serde_json::json!({
            "id":"srv", "name":"Primary", "host":"news.example.test", "port":563,
            "ssl":true, "ssl_verify":true, "username":"user", "connections":8,
            "priority":0, "enabled":true, "retention":0, "pipelining":1, "optional":false
        });
        if !password.is_null() {
            server["password"] = password;
        }
        server
    };
    let stored_password = || {
        AppConfig::load(&app.config_path).unwrap().servers[0]
            .password
            .clone()
    };
    let put = |body: serde_json::Value| {
        client
            .put(format!("{}/api/config/servers/srv", app.base_url))
            .bearer_auth(&access)
            .json(&body)
            .send()
    };

    let added = client
        .post(format!("{}/api/config/servers", app.base_url))
        .bearer_auth(&access)
        .json(&server("secret".into()))
        .send()
        .await
        .unwrap();
    assert_eq!(added.status(), reqwest::StatusCode::OK);
    assert_eq!(stored_password().as_deref(), Some("secret"));

    for unchanged in [
        serde_json::json!(""),
        serde_json::json!("********"),
        serde_json::Value::Null,
    ] {
        let response = put(server(unchanged.clone())).await.unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK, "{unchanged}");
        assert_eq!(
            stored_password().as_deref(),
            Some("secret"),
            "password {unchanged} must keep the stored password"
        );
        assert_eq!(
            app.state.config().servers[0].password.as_deref(),
            Some("secret")
        );
    }

    let response = put(server("rotated".into())).await.unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    assert_eq!(stored_password().as_deref(), Some("rotated"));

    for path in ["/api/config", "/api/config/servers"] {
        let body = client
            .get(format!("{}{path}", app.base_url))
            .bearer_auth(&access)
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert!(
            !body.contains("rotated"),
            "{path} must not echo the password"
        );
        assert!(body.contains("********"), "{path} must mask the password");
    }
}

#[tokio::test]
async fn server_add_and_update_accept_partial_bodies() {
    let app = start_app(false).await;
    let client = reqwest::Client::new();
    let access = setup_access(&app, &client).await;

    let added = client
        .post(format!("{}/api/config/servers", app.base_url))
        .bearer_auth(&access)
        .json(&serde_json::json!({
            "name": "Primary", "host": "news.example.test", "username": "user",
            "password": "secret", "retention": 3000
        }))
        .send()
        .await
        .unwrap();
    let status = added.status();
    assert_eq!(
        status,
        reqwest::StatusCode::OK,
        "{}",
        added.text().await.unwrap()
    );
    let server = app.state.config().servers[0].clone();
    assert!(!server.id.is_empty(), "server id must be generated");
    assert!(server.ssl_verify);
    assert!(server.enabled);
    assert!(!server.optional);
    assert_eq!(server.port, 563);

    let updated = client
        .put(format!("{}/api/config/servers/{}", app.base_url, server.id))
        .bearer_auth(&access)
        .json(&serde_json::json!({ "connections": 4 }))
        .send()
        .await
        .unwrap();
    let status = updated.status();
    assert_eq!(
        status,
        reqwest::StatusCode::OK,
        "{}",
        updated.text().await.unwrap()
    );
    let saved = AppConfig::load(&app.config_path).unwrap().servers[0].clone();
    assert_eq!(saved.id, server.id);
    assert_eq!(saved.connections, 4);
    assert_eq!(saved.host, "news.example.test");
    assert_eq!(saved.retention, 3000);
    assert_eq!(saved.password.as_deref(), Some("secret"));

    let bad = client
        .put(format!("{}/api/config/servers/{}", app.base_url, server.id))
        .bearer_auth(&access)
        .json(&serde_json::json!({ "port": "not-a-port" }))
        .send()
        .await
        .unwrap();
    assert_eq!(bad.status(), reqwest::StatusCode::BAD_REQUEST);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_config_writes_do_not_lose_updates() {
    let app = start_app(true).await;
    let client = reqwest::Client::new();
    let tokens = client
        .post(format!("{}/api/auth/login", app.base_url))
        .json(&serde_json::json!({"username":"admin","password":"password"}))
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap();
    let access = tokens["access_token"].as_str().unwrap().to_string();

    const WRITERS: usize = 24;
    let mut tasks = Vec::new();
    for i in 0..WRITERS {
        let client = client.clone();
        let access = access.clone();
        let base_url = app.base_url.clone();
        tasks.push(tokio::spawn(async move {
            let (path, body) = if i % 2 == 0 {
                (
                    "categories",
                    serde_json::json!({"name": format!("cat-{i}"), "post_processing": 3}),
                )
            } else {
                (
                    "rss-feeds",
                    serde_json::json!({"name": format!("feed-{i}"), "url": "https://feed.invalid/rss"}),
                )
            };
            client
                .post(format!("{base_url}/api/config/{path}"))
                .bearer_auth(&access)
                .json(&body)
                .send()
                .await
                .unwrap()
                .status()
        }));
    }
    for task in tasks {
        assert_eq!(task.await.unwrap(), reqwest::StatusCode::OK);
    }

    for config in [
        (*app.state.config()).clone(),
        AppConfig::load(&app.config_path).unwrap(),
    ] {
        for i in 0..WRITERS {
            if i % 2 == 0 {
                let name = format!("cat-{i}");
                assert!(
                    config.categories.iter().any(|c| c.name == name),
                    "lost category {name}"
                );
            } else {
                let name = format!("feed-{i}");
                assert!(
                    config.rss_feeds.iter().any(|f| f.name == name),
                    "lost feed {name}"
                );
            }
        }
    }
}

#[tokio::test]
async fn missing_article_returns_404() {
    use nzb_nntp::testutil::{MockConfig, MockNntpServer, test_config};

    let server = MockNntpServer::start(MockConfig::default()).await;
    let app = start_app(true).await;
    app.state
        .queue_manager
        .update_servers(vec![test_config(server.port())]);
    let client = reqwest::Client::new();
    let (access, _) = login(&app, &client).await;

    let (status, body) = call(
        client
            .get(format!(
                "{}/api/articles/missing@example.test",
                app.base_url
            ))
            .bearer_auth(&access),
    )
    .await;
    assert_eq!(status, 404, "{body}");
}
