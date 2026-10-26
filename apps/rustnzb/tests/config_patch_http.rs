//! PATCH /api/config: registry validation, If-Match, locks and step-up.

mod support;

use nzb_web::nzb_core::config::AppConfig;
use nzb_web::nzb_core::overrides::{OverrideSet, OverrideSource, OverrideValue};
use reqwest::StatusCode;
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
