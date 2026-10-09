//! `POST /api/queue/add` accepts its options as multipart text fields as well
//! as query parameters (the README's `-F "category=tv"` example). Only parts
//! that carry a file name are NZB uploads; form fields override the query.

mod support;

use reqwest::StatusCode;
use reqwest::multipart::{Form, Part};
use support::{sample_nzb_bytes, start_test_server};

struct Harness {
    client: reqwest::Client,
    base_url: String,
    access: String,
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
                "username": "form-fields-test",
                "password": "form-fields-test-password"
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(setup.status(), StatusCode::OK);
        let access = setup.json::<serde_json::Value>().await.unwrap()["access_token"]
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
            _app: app,
        }
    }

    fn nzb_form() -> Form {
        Form::new().part(
            "file",
            Part::bytes(sample_nzb_bytes())
                .file_name("upload.nzb")
                .mime_str("application/x-nzb")
                .unwrap(),
        )
    }

    async fn add(&self, query: &str, form: Form) -> (StatusCode, serde_json::Value) {
        let response = self
            .client
            .post(format!("{}/api/queue/add{query}", self.base_url))
            .bearer_auth(&self.access)
            .multipart(form)
            .send()
            .await
            .unwrap();
        let status = response.status();
        let body = response.json().await.unwrap_or(serde_json::Value::Null);
        (status, body)
    }

    async fn jobs(&self) -> Vec<serde_json::Value> {
        self.client
            .get(format!("{}/api/queue", self.base_url))
            .bearer_auth(&self.access)
            .send()
            .await
            .unwrap()
            .json::<serde_json::Value>()
            .await
            .unwrap()["jobs"]
            .as_array()
            .expect("jobs")
            .clone()
    }

    /// (name, category, priority) of the only queued job.
    async fn only_job(&self) -> (String, String, u64) {
        let jobs = self.jobs().await;
        assert_eq!(jobs.len(), 1, "{jobs:?}");
        (
            jobs[0]["name"].as_str().unwrap().to_string(),
            jobs[0]["category"].as_str().unwrap().to_string(),
            jobs[0]["priority"].as_u64().unwrap(),
        )
    }
}

#[tokio::test]
async fn readme_category_form_field_is_applied() {
    let h = Harness::start().await;
    let (status, body) = h.add("", Harness::nzb_form().text("category", "tv")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["nzo_ids"].as_array().unwrap().len(), 1, "{body}");
    assert_eq!(
        h.only_job().await,
        ("upload".to_string(), "tv".to_string(), 1)
    );
}

#[tokio::test]
async fn text_fields_before_the_file_are_options_not_uploads() {
    let h = Harness::start().await;
    let form = Form::new()
        .text("cat", "movies")
        .text("priority", "2")
        .text("nzbname", "Chosen.Name")
        .text("unrelated", "ignored")
        .part(
            "file",
            Part::bytes(sample_nzb_bytes()).file_name("upload.nzb"),
        );
    let (status, body) = h.add("", form).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        h.only_job().await,
        ("Chosen.Name".to_string(), "movies".to_string(), 2)
    );
}

#[tokio::test]
async fn name_form_field_overrides_job_name() {
    let h = Harness::start().await;
    let (status, body) = h.add("", Harness::nzb_form().text("name", "Named")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(h.only_job().await.0, "Named");
}

#[tokio::test]
async fn query_params_still_apply() {
    let h = Harness::start().await;
    let (status, body) = h
        .add(
            "?category=tv&priority=0&name=FromQuery",
            Harness::nzb_form(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        h.only_job().await,
        ("FromQuery".to_string(), "tv".to_string(), 0)
    );
}

#[tokio::test]
async fn form_fields_override_query_params() {
    let h = Harness::start().await;
    let form = Harness::nzb_form()
        .text("category", "movies")
        .text("priority", "3")
        .text("name", "FromForm");
    let (status, body) = h.add("?category=tv&priority=0&name=FromQuery", form).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        h.only_job().await,
        ("FromForm".to_string(), "movies".to_string(), 3)
    );
}

#[tokio::test]
async fn invalid_priority_form_field_is_rejected() {
    let h = Harness::start().await;
    let (status, body) = h
        .add("", Harness::nzb_form().text("priority", "urgent"))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(h.jobs().await.is_empty());
}

#[tokio::test]
async fn bare_file_field_without_file_name_is_still_an_upload() {
    let h = Harness::start().await;
    let form = Form::new()
        .text("file", String::from_utf8(sample_nzb_bytes()).unwrap())
        .text("category", "tv");
    let (status, body) = h.add("", form).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        h.only_job().await,
        ("unknown".to_string(), "tv".to_string(), 1)
    );
}
