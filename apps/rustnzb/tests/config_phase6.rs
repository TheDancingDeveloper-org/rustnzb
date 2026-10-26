//! Phase 6: a Live PATCH takes effect, generic env overrides, managed mode.

mod support;

use nzb_web::nzb_core::overrides::{OverrideSet, OverrideSource, generic_env_override};
use reqwest::StatusCode;
use serde_json::json;
use support::{start_test_server_managed, start_test_server_with_overrides};

async fn access_token(client: &reqwest::Client, base_url: &str) -> String {
    let setup = client
        .post(format!("{base_url}/api/auth/setup"))
        .json(&json!({
            "username": "phase6",
            "password": "phase6-password"
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

#[tokio::test]
async fn patch_speed_limit_takes_effect_without_restart() {
    let app = start_test_server_with_overrides(OverrideSet::default()).await;
    let client = reqwest::Client::new();
    let token = access_token(&client, &app.base_url).await;
    let tag = etag(&client, &app.base_url, &token).await;

    let patched = client
        .patch(format!("{}/api/config", app.base_url))
        .bearer_auth(&token)
        .header(reqwest::header::IF_MATCH, &tag)
        .json(&json!({"settings": {"general.speed_limit_bps": 4096}}))
        .send()
        .await
        .unwrap();
    assert_eq!(
        patched.status(),
        StatusCode::OK,
        "{}",
        patched.text().await.unwrap()
    );

    let live = client
        .get(format!("{}/api/config/speed-limit", app.base_url))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(live.status(), StatusCode::OK);
    let body: serde_json::Value = live.json().await.unwrap();
    assert_eq!(body["speed_limit_bps"], 4096);
}

#[tokio::test]
async fn managed_mode_refuses_patch_and_sab_set_config() {
    let app = start_test_server_managed().await;
    let client = reqwest::Client::new();
    let token = access_token(&client, &app.base_url).await;
    let tag = etag(&client, &app.base_url, &token).await;

    let patched = client
        .patch(format!("{}/api/config", app.base_url))
        .bearer_auth(&token)
        .header(reqwest::header::IF_MATCH, &tag)
        .json(&json!({"settings": {"general.speed_limit_bps": 2048}}))
        .send()
        .await
        .unwrap();
    assert_eq!(patched.status(), StatusCode::CONFLICT);
    let body = patched.text().await.unwrap();
    assert!(body.contains("managed"), "{body}");

    // No API key is configured on the test server, so set_config is reachable.
    let sab = client
        .get(format!(
            "{}/sabnzbd/api?mode=set_config&section=misc&keyword=bandwidth_max&value=1M",
            app.base_url
        ))
        .send()
        .await
        .unwrap();
    let sab_body = sab.text().await.unwrap();
    assert!(sab_body.contains("managed"), "{sab_body}");

    let effective = client
        .get(format!("{}/api/config/effective", app.base_url))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(effective.status(), StatusCode::OK);
    let entries: Vec<serde_json::Value> = effective.json().await.unwrap();
    assert!(!entries.is_empty());
    assert!(
        entries
            .iter()
            .all(|entry| entry["managed"] == true && entry["locked"] == true)
    );
}

#[tokio::test]
async fn generic_env_shows_as_locked_env_source() {
    let parsed = generic_env_override("RUSTNZB__GENERAL__SPEED_LIMIT_BPS", "8192")
        .unwrap()
        .expect("valid override");
    let mut overrides = OverrideSet::default();
    overrides.set(
        parsed,
        OverrideSource::Env("RUSTNZB__GENERAL__SPEED_LIMIT_BPS"),
    );
    let app = start_test_server_with_overrides(overrides).await;
    let client = reqwest::Client::new();
    let token = access_token(&client, &app.base_url).await;
    let effective = client
        .get(format!("{}/api/config/effective", app.base_url))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(effective.status(), StatusCode::OK);
    let entries: Vec<serde_json::Value> = effective.json().await.unwrap();
    let speed = entries
        .iter()
        .find(|entry| entry["key"] == "general.speed_limit_bps")
        .expect("speed limit row");
    assert_eq!(speed["source"], "env");
    assert_eq!(speed["locked"], true);
    assert_eq!(speed["value"], 8192);
}

#[test]
fn generic_env_rejects_invalid_and_unknown() {
    let invalid = generic_env_override("RUSTNZB__GENERAL__SPEED_LIMIT_BPS", "nope").unwrap_err();
    assert!(invalid.contains("ignoring"), "{invalid}");
    let unknown = generic_env_override("RUSTNZB__NOPE__NOPE", "1").unwrap_err();
    assert!(unknown.contains("ignoring"), "{unknown}");
    assert!(generic_env_override("NOT_OURS", "1").unwrap().is_none());
}
