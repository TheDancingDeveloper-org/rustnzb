//! The native add endpoints honour SABnzbd's inline job-password convention
//! (`name{{password}}`, `name/password`) and an explicit `password` field.
//! Passwords are read back through the SABnzbd queue, whose slot contract
//! includes the job password.

mod support;

use reqwest::StatusCode;
use support::{sample_nzb_bytes, start_test_server};

const META_PASSWORD_NZB: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<nzb xmlns="http://www.newzbin.com/DTD/2003/nzb">
  <head><meta type="password">meta-pw</meta></head>
  <file poster="test@example.com" date="1234567890" subject="test.rar (1/1)">
    <groups><group>alt.binaries.test</group></groups>
    <segments>
      <segment number="1" bytes="768000">article1@example.com</segment>
    </segments>
  </file>
</nzb>"#;

struct Harness {
    client: reqwest::Client,
    base_url: String,
    access: String,
    sab_key: String,
    _app: support::TestApp,
}

impl Harness {
    async fn start() -> Self {
        let app = start_test_server(Vec::new()).await;
        let client = reqwest::Client::new();
        let base_url = app.base_url.clone();
        let setup = client
            .post(format!("{base_url}/api/auth/setup"))
            .json(&serde_json::json!({
                "username": "inline-pw-test",
                "password": "inline-pw-test-password"
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(setup.status(), StatusCode::OK);
        let access = setup.json::<serde_json::Value>().await.unwrap()["access_token"]
            .as_str()
            .unwrap()
            .to_string();
        let sab_key = client
            .post(format!("{base_url}/api/config/sabnzbd-api-key/rotate"))
            .bearer_auth(&access)
            .json(&serde_json::json!({}))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json::<serde_json::Value>()
            .await
            .unwrap()["api_key"]
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
        Self {
            client,
            base_url,
            access,
            sab_key,
            _app: app,
        }
    }

    async fn upload(&self, query: &str, file_name: &str, nzb: Vec<u8>, password: Option<&str>) {
        let part = reqwest::multipart::Part::bytes(nzb)
            .file_name(file_name.to_string())
            .mime_str("application/x-nzb")
            .unwrap();
        let mut form = reqwest::multipart::Form::new().part("file", part);
        if let Some(password) = password {
            form = form.text("password", password.to_string());
        }
        let response = self
            .client
            .post(format!("{}/api/queue/add{query}", self.base_url))
            .bearer_auth(&self.access)
            .multipart(form)
            .send()
            .await
            .unwrap();
        let status = response.status();
        let body = response.text().await.unwrap();
        assert_eq!(status, StatusCode::OK, "{file_name}: {body}");
    }

    /// (job name, password) of the only queued job, via the SABnzbd queue.
    async fn only_job(&self) -> (String, String) {
        let queue = self
            .client
            .get(format!(
                "{}/sabnzbd/api?mode=queue&output=json&apikey={}",
                self.base_url, self.sab_key
            ))
            .send()
            .await
            .unwrap()
            .json::<serde_json::Value>()
            .await
            .unwrap();
        let slots = queue["queue"]["slots"].as_array().expect("slots");
        assert_eq!(slots.len(), 1, "{queue}");
        (
            slots[0]["filename"].as_str().unwrap().to_string(),
            slots[0]["password"].as_str().unwrap().to_string(),
        )
    }
}

#[tokio::test]
async fn native_add_takes_inline_password_from_file_name() {
    let h = Harness::start().await;
    h.upload("", "My.Release{{filepw}}.nzb", sample_nzb_bytes(), None)
        .await;
    assert_eq!(
        h.only_job().await,
        ("My.Release".to_string(), "filepw".to_string())
    );
}

#[tokio::test]
async fn native_add_takes_inline_password_from_name_override() {
    let h = Harness::start().await;
    h.upload(
        "?name=Chosen%2Fnamepw",
        "upload.nzb",
        sample_nzb_bytes(),
        None,
    )
    .await;
    assert_eq!(
        h.only_job().await,
        ("Chosen".to_string(), "namepw".to_string())
    );
}

#[tokio::test]
async fn native_add_explicit_password_field_wins() {
    let h = Harness::start().await;
    h.upload(
        "",
        "Show{{inline-pw}}.nzb",
        META_PASSWORD_NZB.as_bytes().to_vec(),
        Some("explicit-pw"),
    )
    .await;
    assert_eq!(
        h.only_job().await,
        ("Show".to_string(), "explicit-pw".to_string())
    );
}

#[tokio::test]
async fn native_add_inline_password_beats_nzb_meta() {
    let h = Harness::start().await;
    h.upload(
        "",
        "Show{{inline-pw}}.nzb",
        META_PASSWORD_NZB.as_bytes().to_vec(),
        None,
    )
    .await;
    assert_eq!(
        h.only_job().await,
        ("Show".to_string(), "inline-pw".to_string())
    );
}
