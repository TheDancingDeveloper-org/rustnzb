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
