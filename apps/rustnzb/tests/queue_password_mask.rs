//! The native queue API masks the job archive password the way the config
//! API masks NNTP server passwords, while the SABnzbd queue keeps returning
//! it (the SAB slot contract includes `password`).

mod support;

use reqwest::StatusCode;
use support::{sample_nzb_bytes, start_test_server};

const META_PASSWORD_NZB: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<nzb xmlns="http://www.newzbin.com/DTD/2003/nzb">
  <head><meta type="password">archive-secret</meta></head>
  <file poster="test@example.com" date="1234567890" subject="test.rar (1/1)">
    <groups><group>alt.binaries.test</group></groups>
    <segments>
      <segment number="1" bytes="768000">article1@example.com</segment>
    </segments>
  </file>
</nzb>"#;

async fn upload(client: &reqwest::Client, base_url: &str, access: &str, name: &str, nzb: Vec<u8>) {
    let part = reqwest::multipart::Part::bytes(nzb)
        .file_name(name.to_string())
        .mime_str("application/x-nzb")
        .unwrap();
    let response = client
        .post(format!("{base_url}/api/queue/add"))
        .bearer_auth(access)
        .multipart(reqwest::multipart::Form::new().part("file", part))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn native_queue_masks_archive_password_but_sab_queue_keeps_it() {
    let app = start_test_server(Vec::new()).await;
    let base_url = app.base_url.clone();
    let client = reqwest::Client::new();
    let access = client
        .post(format!("{base_url}/api/auth/setup"))
        .json(&serde_json::json!({
            "username": "mask-test",
            "password": "mask-test-password"
        }))
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap()["access_token"]
        .as_str()
        .unwrap()
        .to_string();
    client
        .post(format!("{base_url}/api/queue/pause"))
        .bearer_auth(&access)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();

    upload(
        &client,
        &base_url,
        &access,
        "Secret.nzb",
        META_PASSWORD_NZB.as_bytes().to_vec(),
    )
    .await;
    upload(&client, &base_url, &access, "Open.nzb", sample_nzb_bytes()).await;

    let response = client
        .get(format!("{base_url}/api/queue"))
        .bearer_auth(&access)
        .send()
        .await
        .unwrap();
    let raw = response.text().await.unwrap();
    assert!(
        !raw.contains("archive-secret"),
        "native queue leaks the archive password: {raw}"
    );
    let queue: serde_json::Value = serde_json::from_str(&raw).unwrap();
    let password_of = |name: &str| {
        queue["jobs"]
            .as_array()
            .unwrap()
            .iter()
            .find(|job| job["name"] == name)
            .unwrap_or_else(|| panic!("{name} not queued: {queue}"))["password"]
            .clone()
    };
    assert_eq!(password_of("Secret"), "********");
    assert_eq!(password_of("Open"), "");

    // The SABnzbd queue still returns the real password.
    let sab_key = client
        .post(format!("{base_url}/api/config/sabnzbd-api-key/rotate"))
        .bearer_auth(&access)
        .json(&serde_json::json!({}))
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap()["api_key"]
        .as_str()
        .unwrap()
        .to_string();
    let sab = client
        .get(format!(
            "{base_url}/sabnzbd/api?mode=queue&output=json&apikey={sab_key}"
        ))
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap();
    let slot = sab["queue"]["slots"]
        .as_array()
        .unwrap()
        .iter()
        .find(|slot| slot["filename"] == "Secret")
        .unwrap_or_else(|| panic!("Secret not in SAB queue: {sab}"));
    assert_eq!(slot["password"], "archive-secret");
}
