//! Config backup and restore through the HTTP API (WI-1721).
//!
//! A masked export must carry no credential, a masked restore must keep the
//! install's own secrets instead of writing `********`, and a refused file
//! must leave the configuration and `config.toml` untouched.

mod support;

use nzb_web::nzb_core::config::ServerConfig;
use nzb_web::nzb_core::config_backup;
use reqwest::StatusCode;
use support::{TestApp, start_test_server};

const API_KEY: &str = "backup-api-key-7a1";
const SERVER_PASSWORD: &str = "backup-server-password-7b2";
const PROXY_SECRET: &str = "backup-proxy-secret-7c3";
const RSS_KEY: &str = "backup-rss-key-7d4";
const NTFY_TOPIC: &str = "backup-ntfy-topic-7e5";
const OTEL_SECRET: &str = "backup-otel-secret-7f6";

const RSS_URL: &str = "https://indexer.example/rss?t=2000&apikey=backup-rss-key-7d4";

async fn setup(client: &reqwest::Client, base_url: &str) -> String {
    let response = client
        .post(format!("{base_url}/api/auth/setup"))
        .json(&serde_json::json!({
            "username": "backup-test",
            "password": "backup-test-password"
        }))
        .send()
        .await
        .expect("auth setup");
    assert_eq!(response.status(), StatusCode::OK);
    response.json::<serde_json::Value>().await.unwrap()["access_token"]
        .as_str()
        .expect("access token")
        .to_string()
}

/// Seed the install's own secrets, as a running install would hold them.
async fn seeded_app() -> TestApp {
    let mut server = ServerConfig::new("srv-1", "news.example.com");
    server.name = "Primary".into();
    server.password = Some(SERVER_PASSWORD.into());
    server.proxy_url = Some(format!(
        "socks5://user:{PROXY_SECRET}@proxy.example.net:1080"
    ));
    let app = start_test_server(vec![server]).await;
    app.state
        .update_config_with(|config| {
            config.general.api_key = Some(API_KEY.into());
            config.general.speed_limit_bps = 12_345;
            config
                .categories
                .push(nzb_web::nzb_core::config::CategoryConfig {
                    name: "TV".into(),
                    output_dir: Some("tv".into()),
                    ..Default::default()
                });
            config
                .rss_feeds
                .push(nzb_web::nzb_core::config::RssFeedConfig {
                    name: "daily".into(),
                    url: RSS_URL.into(),
                    poll_interval_secs: 900,
                    category: None,
                    filter_regex: None,
                    enabled: true,
                    auto_download: false,
                    max_age_days: None,
                });
            config.notifications.ntfy_topic = NTFY_TOPIC.into();
            config.otel.endpoint = format!("https://collector:{OTEL_SECRET}@otel.example:4317");
            Ok::<_, anyhow::Error>(())
        })
        .unwrap();
    app
}

async fn export(client: &reqwest::Client, base_url: &str, token: &str, secrets: bool) -> String {
    let url = if secrets {
        format!("{base_url}/api/config/backup?secrets=true")
    } else {
        format!("{base_url}/api/config/backup")
    };
    let mut request = client.get(&url).bearer_auth(token);
    if secrets {
        request = request.header("X-Confirm-Password", "backup-test-password");
    }
    let response = request.send().await.expect("export");
    assert_eq!(response.status(), StatusCode::OK);
    let headers = response.headers().clone();
    assert!(
        headers
            .get(reqwest::header::CONTENT_DISPOSITION)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.contains("attachment; filename=\"rustnzb-backup-")),
        "{headers:?}"
    );
    assert_eq!(
        headers
            .get(reqwest::header::CACHE_CONTROL)
            .and_then(|v| v.to_str().ok()),
        Some("no-store")
    );
    response.text().await.expect("export body")
}

fn assert_no_secret(text: &str) {
    // Name the field that leaked, never the value: a failing assertion must
    // not put a credential in the test log.
    for (field, secret) in [
        ("api_key", API_KEY),
        ("server_password", SERVER_PASSWORD),
        ("proxy_secret", PROXY_SECRET),
        ("rss_key", RSS_KEY),
        ("ntfy_topic", NTFY_TOPIC),
        ("otel_secret", OTEL_SECRET),
    ] {
        assert!(!text.contains(secret), "a masked export leaked {field}");
    }
}

async fn post_backup(
    client: &reqwest::Client,
    base_url: &str,
    token: &str,
    path: &str,
    body: &str,
) -> reqwest::Response {
    let mut request = client
        .post(format!("{base_url}/api/config/backup/{path}"))
        .bearer_auth(token)
        .header(reqwest::header::CONTENT_TYPE, "application/toml")
        .body(body.to_string());
    if path == "restore" {
        request = request.header("X-Confirm-Password", "backup-test-password");
    }
    request.send().await.expect("post backup")
}

#[tokio::test]
async fn masked_export_carries_no_secret_and_says_which_it_is() {
    let app = seeded_app().await;
    let client = reqwest::Client::new();
    let token = setup(&client, &app.base_url).await;

    let body = export(&client, &app.base_url, &token, false).await;
    assert_no_secret(&body);
    let document = config_backup::parse(&body).expect("a valid backup");
    assert!(!document.backup.secrets_included);
    assert_eq!(document.backup.backup_version, 1);
    assert!(!document.backup.created_at.is_empty());
    assert_eq!(document.config.general.port, 9090);
    assert_eq!(document.config.servers[0].host, "news.example.com");

    // An explicit opt-in does carry them, and says so.
    let with_secrets = export(&client, &app.base_url, &token, true).await;
    assert!(with_secrets.contains(API_KEY));
    assert!(with_secrets.contains(SERVER_PASSWORD));
    assert!(with_secrets.contains(RSS_KEY));
    assert!(with_secrets.contains("secrets_included = true"));
}

#[tokio::test]
async fn a_masked_export_masks_indexer_keys_and_ntfy_topics() {
    let app = seeded_app().await;
    let client = reqwest::Client::new();
    let token = setup(&client, &app.base_url).await;

    let body = export(&client, &app.base_url, &token, false).await;
    assert_no_secret(&body);
    let document = config_backup::parse(&body).expect("a valid backup");
    assert_eq!(document.config.rss_feeds.len(), 1);
    assert_eq!(
        document.config.rss_feeds[0].url,
        "https://indexer.example/rss?********"
    );
    assert_eq!(
        document.config.notifications.ntfy_topic,
        config_backup::SECRET_MASK
    );
    assert_eq!(
        document.config.otel.endpoint,
        "https://********@otel.example:4317"
    );
    // Before a merge the masks are exactly what validate refuses to save;
    // the restore path merges them against the install first.
    assert!(
        config_backup::validate(&document.config)
            .iter()
            .any(|p| p.contains("rss_feeds[0]")),
        "a masked feed URL must not be saveable as-is"
    );
}

#[tokio::test]
async fn restoring_a_masked_export_keeps_feed_urls_and_topics() {
    let app = seeded_app().await;
    let client = reqwest::Client::new();
    let token = setup(&client, &app.base_url).await;
    let body = export(&client, &app.base_url, &token, false).await;

    let restored = post_backup(&client, &app.base_url, &token, "restore", &body).await;
    assert_eq!(restored.status(), StatusCode::OK);
    let restored: serde_json::Value = restored.json().await.unwrap();
    assert!(
        restored["summary"]["warnings"]
            .as_array()
            .unwrap()
            .is_empty(),
        "{restored}"
    );
    assert_eq!(restored["summary"]["rss_feed_count"], 1, "{restored}");

    let config = app.state.config();
    assert_eq!(config.rss_feeds[0].url, RSS_URL);
    assert_eq!(config.notifications.ntfy_topic, NTFY_TOPIC);
    assert_eq!(
        config.otel.endpoint,
        format!("https://collector:{OTEL_SECRET}@otel.example:4317")
    );
    let saved = std::fs::read_to_string(&app.config_path).unwrap();
    assert!(
        saved.contains(RSS_KEY),
        "the stored feed key was not restored"
    );
    assert!(!saved.contains("********"));
}

#[tokio::test]
async fn restore_of_a_masked_export_keeps_this_installs_secrets() {
    let app = seeded_app().await;
    let client = reqwest::Client::new();
    let token = setup(&client, &app.base_url).await;
    let body = export(&client, &app.base_url, &token, false).await;

    // Change two non-secret settings, then restore: they come back from the file.
    let put = client
        .put(format!("{}/api/config/speed-limit", app.base_url))
        .bearer_auth(&token)
        .json(&serde_json::json!({ "speed_limit_bps": 999 }))
        .send()
        .await
        .unwrap();
    assert_eq!(put.status(), StatusCode::OK);
    assert_eq!(app.state.config().general.speed_limit_bps, 999);

    let preview = post_backup(&client, &app.base_url, &token, "preview", &body).await;
    assert_eq!(preview.status(), StatusCode::OK);
    let preview: serde_json::Value = preview.json().await.unwrap();
    assert_eq!(preview["restored"], false);
    assert_eq!(preview["summary"]["secrets_kept"], 6, "{preview}");
    assert!(
        preview["summary"]["warnings"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    // A preview changes nothing.
    assert_eq!(app.state.config().general.speed_limit_bps, 999);

    let restored = post_backup(&client, &app.base_url, &token, "restore", &body).await;
    assert_eq!(restored.status(), StatusCode::OK);
    let restored: serde_json::Value = restored.json().await.unwrap();
    assert_eq!(restored["restored"], true);
    assert_eq!(restored["summary"]["secrets_kept"], 6, "{restored}");
    assert_eq!(restored["summary"]["server_count"], 1);
    assert_eq!(restored["restored"], true);

    let config = app.state.config();
    assert_eq!(config.general.speed_limit_bps, 12_345);
    assert_eq!(config.general.api_key.as_deref(), Some(API_KEY));
    assert_eq!(config.servers[0].password.as_deref(), Some(SERVER_PASSWORD));
    assert_eq!(
        config.servers[0].proxy_url,
        Some(format!(
            "socks5://user:{PROXY_SECRET}@proxy.example.net:1080"
        ))
    );
    // Applied to the running queue manager, not only to the file.
    assert_eq!(app.state.queue_manager.get_speed_limit(), 12_345);

    // And the saved file holds the kept secrets, never the mask.
    let saved = std::fs::read_to_string(&app.config_path).unwrap();
    assert!(saved.contains(API_KEY));
    assert!(!saved.contains("********"));
}

#[tokio::test]
async fn restoring_an_export_with_secrets_replaces_them() {
    let app = seeded_app().await;
    let client = reqwest::Client::new();
    let token = setup(&client, &app.base_url).await;
    let body = export(&client, &app.base_url, &token, true)
        .await
        .replace(API_KEY, "rotated-key-123");

    let restored = post_backup(&client, &app.base_url, &token, "restore", &body).await;
    assert_eq!(restored.status(), StatusCode::OK);
    let restored: serde_json::Value = restored.json().await.unwrap();
    assert_eq!(restored["backup"]["secrets_included"], true);
    assert_eq!(restored["summary"]["secrets_kept"], 0, "{restored}");
    assert_eq!(
        app.state.config().general.api_key.as_deref(),
        Some("rotated-key-123")
    );
}

#[tokio::test]
async fn a_masked_restore_onto_a_fresh_install_reports_what_it_could_not_keep() {
    // The backup comes from an install that holds secrets; the target is a
    // fresh one. A mask must not be mistaken for a value in either direction.
    let source = seeded_app().await;
    let client = reqwest::Client::new();
    let source_token = setup(&client, &source.base_url).await;
    let body = export(&client, &source.base_url, &source_token, false).await;
    assert_no_secret(&body);

    let target = start_test_server(Vec::new()).await;
    let target_token = setup(&client, &target.base_url).await;
    let restored = post_backup(&client, &target.base_url, &target_token, "restore", &body).await;
    assert_eq!(restored.status(), StatusCode::OK);
    let restored: serde_json::Value = restored.json().await.unwrap();
    assert_eq!(restored["summary"]["secrets_kept"], 0, "{restored}");
    // general.api_key, the server password, its proxy URL, the RSS feed URL,
    // the ntfy topic and the OTel endpoint were masked.
    assert_eq!(
        restored["summary"]["warnings"].as_array().unwrap().len(),
        6,
        "{restored}"
    );
    let config = target.state.config();
    assert!(config.general.api_key.is_none());
    assert_eq!(config.servers.len(), 1);
    assert!(config.servers[0].password.is_none());
    assert!(config.servers[0].proxy_url.is_none());
    assert!(
        config.rss_feeds.is_empty(),
        "a feed with no URL to keep is dropped"
    );
    assert!(config.notifications.ntfy_topic.is_empty());
    assert_eq!(config.otel.endpoint, "http://localhost:4317");
    let saved = std::fs::read_to_string(&target.config_path).unwrap();
    assert!(!saved.contains(config_backup::SECRET_MASK));
}

#[tokio::test]
async fn a_refused_backup_changes_nothing_and_says_why() {
    let app = seeded_app().await;
    let client = reqwest::Client::new();
    let token = setup(&client, &app.base_url).await;
    let before = serde_json::to_value(app.state.config().as_ref()).unwrap();
    let before_file = std::fs::read_to_string(&app.config_path).unwrap();

    let valid = export(&client, &app.base_url, &token, false).await;

    // Unknown key: refused rather than silently dropped.
    let typo = valid.replace("port = 9090", "port = 9090\ntypo_setting = true");
    let refused = post_backup(&client, &app.base_url, &token, "restore", &typo).await;
    assert_eq!(refused.status(), StatusCode::BAD_REQUEST);
    let message = refused.text().await.unwrap();
    assert!(message.contains("typo_setting"), "{message}");

    // Out-of-range value: the config validator's message names the setting.
    let bad_port = valid.replace("port = 9090", "port = 0");
    let refused = post_backup(&client, &app.base_url, &token, "restore", &bad_port).await;
    assert_eq!(refused.status(), StatusCode::BAD_REQUEST);
    let message = refused.text().await.unwrap();
    assert!(message.contains("general.port"), "{message}");

    // Empty category list would leave enqueue without a category.
    let empty_categories = valid.replace(
        "[[config.categories]]\nname = \"Default\"",
        "[[config.categories]]\nname = \"\"",
    );
    assert_ne!(empty_categories, valid, "the empty-category edit applied");
    let refused = post_backup(&client, &app.base_url, &token, "restore", &empty_categories).await;
    assert_eq!(refused.status(), StatusCode::BAD_REQUEST);

    // Not TOML at all.
    let refused = post_backup(&client, &app.base_url, &token, "restore", "not toml [[[").await;
    assert_eq!(refused.status(), StatusCode::BAD_REQUEST);

    // A preview refuses the same documents, with the same status.
    let refused = post_backup(&client, &app.base_url, &token, "preview", &bad_port).await;
    assert_eq!(refused.status(), StatusCode::BAD_REQUEST);

    assert_eq!(
        serde_json::to_value(app.state.config().as_ref()).unwrap(),
        before
    );
    assert_eq!(
        std::fs::read_to_string(&app.config_path).unwrap(),
        before_file
    );
}

#[tokio::test]
async fn a_backup_from_a_newer_build_is_refused() {
    let app = seeded_app().await;
    let client = reqwest::Client::new();
    let token = setup(&client, &app.base_url).await;
    let body = export(&client, &app.base_url, &token, false)
        .await
        .replace("backup_version = 1", "backup_version = 2");

    let refused = post_backup(&client, &app.base_url, &token, "restore", &body).await;
    assert_eq!(refused.status(), StatusCode::BAD_REQUEST);
    let message = refused.text().await.unwrap();
    assert!(message.contains("newer rustnzb"), "{message}");
}

#[tokio::test]
async fn the_backup_routes_require_authentication() {
    let app = start_test_server(Vec::new()).await;
    let client = reqwest::Client::new();
    let status = client
        .get(format!("{}/api/config/backup", app.base_url))
        .send()
        .await
        .unwrap()
        .status();
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}
