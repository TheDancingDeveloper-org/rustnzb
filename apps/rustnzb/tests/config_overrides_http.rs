//! A settings-page round trip must never write an overridden value into
//! `config.toml`. The page reads the effective configuration and PUTs it
//! back, so the handlers receive override values they did not choose.

mod support;

use nzb_web::nzb_core::config::AppConfig;
use nzb_web::nzb_core::overrides::{OverrideSet, OverrideSource, OverrideValue};
use reqwest::StatusCode;
use support::start_test_server_with_overrides;

async fn access_token(client: &reqwest::Client, base_url: &str) -> String {
    let setup = client
        .post(format!("{base_url}/api/auth/setup"))
        .json(&serde_json::json!({
            "username": "overrides-test",
            "password": "overrides-test-password"
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

#[cfg(feature = "webdav")]
#[tokio::test]
async fn dav_env_override_is_not_saved_by_the_settings_round_trip() {
    let mut overrides = OverrideSet::default();
    overrides.set(
        OverrideValue::DavEnabled(true),
        OverrideSource::Env("RUSTNZB_DAV_ENABLED"),
    );
    let app = start_test_server_with_overrides(overrides).await;
    let client = reqwest::Client::new();
    let token = access_token(&client, &app.base_url).await;
    let dav_url = format!("{}/api/config/dav", app.base_url);

    // The page shows the effective value, which the env forced on...
    let shown: serde_json::Value = client
        .get(&dav_url)
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(shown["enabled"], true);

    // ...and writes the whole object back.
    let put = client
        .put(&dav_url)
        .bearer_auth(&token)
        .json(&shown)
        .send()
        .await
        .unwrap();
    assert_eq!(put.status(), StatusCode::OK);

    let on_disk = AppConfig::load(&app.config_path).unwrap();
    assert!(
        !on_disk.dav.enabled,
        "env-forced DAV was written to config.toml"
    );

    // The override still holds for this process.
    let after: serde_json::Value = client
        .get(&dav_url)
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(after["enabled"], true);
}

#[tokio::test]
async fn data_dir_override_is_not_saved_by_the_general_settings_put() {
    let mut overrides = OverrideSet::default();
    overrides.set(
        OverrideValue::DataDir("/override/data".into()),
        OverrideSource::Cli,
    );
    let app = start_test_server_with_overrides(overrides).await;
    let client = reqwest::Client::new();
    let token = access_token(&client, &app.base_url).await;
    let file_data_dir = AppConfig::load(&app.config_path).unwrap().general.data_dir;

    // The page sends the effective data_dir with an ordinary edit.
    let put = client
        .put(format!("{}/api/config/general", app.base_url))
        .bearer_auth(&token)
        .header("X-Confirm-Password", "overrides-test-password")
        .json(&serde_json::json!({
            "data_dir": "/override/data",
            "max_active_downloads": 3
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(put.status(), StatusCode::OK);

    let on_disk = AppConfig::load(&app.config_path).unwrap();
    assert_eq!(on_disk.general.data_dir, file_data_dir);
    assert_eq!(
        on_disk.general.max_active_downloads, 3,
        "the real edit was saved"
    );

    let shown: serde_json::Value = client
        .get(format!("{}/api/config", app.base_url))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(shown["general"]["data_dir"], "/override/data");
}

fn setting<'a>(body: &'a [serde_json::Value], key: &str) -> &'a serde_json::Value {
    body.iter()
        .find(|entry| entry["key"] == key)
        .unwrap_or_else(|| panic!("missing setting {key}"))
}

#[tokio::test]
async fn effective_reports_source_lock_etag_and_redacts_secrets() {
    let secret = "effective-secret-value-9f3a";
    let mut overrides = OverrideSet::default();
    overrides.set(OverrideValue::Port(7771), OverrideSource::Cli);
    overrides.set(
        OverrideValue::DavEnabled(true),
        OverrideSource::Env("RUSTNZB_DAV_ENABLED"),
    );
    let app = start_test_server_with_overrides(overrides).await;
    // The harness file layer is the process config. Put a secret on it the
    // same way a saved config would, without going through an HTTP write
    // that might redact it first.
    app.state
        .update_config_with(|config| {
            config.general.api_key = Some(secret.to_string());
            Ok::<(), nzb_web::error::ApiError>(())
        })
        .unwrap();

    let client = reqwest::Client::new();
    let token = access_token(&client, &app.base_url).await;
    let url = format!("{}/api/config/effective", app.base_url);

    let unauth = client.get(&url).send().await.unwrap();
    assert_eq!(unauth.status(), StatusCode::UNAUTHORIZED);

    let response = client.get(&url).bearer_auth(&token).send().await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let etag = response
        .headers()
        .get(reqwest::header::ETAG)
        .expect("ETag")
        .to_str()
        .unwrap()
        .to_string();
    assert!(etag.starts_with('"') && etag.ends_with('"') && etag.len() > 2);
    let body_text = response.text().await.unwrap();
    assert!(
        !body_text.contains(secret) && !body_text.contains("effective-secret"),
        "a secret value leaked into the effective config"
    );
    let body: Vec<serde_json::Value> = serde_json::from_str(&body_text).unwrap();

    let port = setting(&body, "general.port");
    assert_eq!(port["value"], 7771);
    assert_eq!(port["source"], "cli");
    assert_eq!(port["locked"], true);

    let dav = setting(&body, "dav.enabled");
    assert_eq!(dav["value"], true);
    assert_eq!(dav["source"], "env");
    assert_eq!(dav["locked"], true);

    let timeout = setting(&body, "general.article_timeout_secs");
    assert_eq!(timeout["value"], 10);
    assert_eq!(timeout["source"], "file");
    assert_eq!(timeout["locked"], false);

    let speed = setting(&body, "general.speed_limit_bps");
    assert_eq!(speed["value"], 0);
    assert_eq!(speed["source"], "default");
    assert_eq!(speed["locked"], false);

    let again = client.get(&url).bearer_auth(&token).send().await.unwrap();
    assert_eq!(
        again.headers().get(reqwest::header::ETAG).unwrap(),
        etag.as_str()
    );

    let put = client
        .put(format!("{}/api/config/general", app.base_url))
        .bearer_auth(&token)
        .header("X-Confirm-Password", "overrides-test-password")
        .json(&serde_json::json!({ "max_active_downloads": 4 }))
        .send()
        .await
        .unwrap();
    assert_eq!(put.status(), StatusCode::OK, "{put:?}");

    let changed = client.get(&url).bearer_auth(&token).send().await.unwrap();
    let changed_etag = changed
        .headers()
        .get(reqwest::header::ETAG)
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    assert_ne!(changed_etag, etag);
    let changed_body: Vec<serde_json::Value> = changed.json().await.unwrap();
    let downloads = setting(&changed_body, "general.max_active_downloads");
    assert_eq!(downloads["value"], 4);
    assert_eq!(downloads["source"], "file");
    assert_eq!(downloads["locked"], false);
}
