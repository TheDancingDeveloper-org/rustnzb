//! BUG-64: `POST /api/history/{id}/retry` is idempotent per history entry.

mod support;

use chrono::Utc;
use nzb_web::nzb_core::models::{HistoryEntry, JobFailureCode, JobStatus};
use reqwest::StatusCode;
use support::{sample_nzb_bytes, start_test_server};

async fn setup_auth(client: &reqwest::Client, base_url: &str) -> String {
    let setup = client
        .post(format!("{base_url}/api/auth/setup"))
        .json(&serde_json::json!({
            "username": "retry-test",
            "password": "retry-test-password"
        }))
        .send()
        .await
        .expect("auth setup failed");
    assert_eq!(setup.status(), StatusCode::OK);
    setup.json::<serde_json::Value>().await.unwrap()["access_token"]
        .as_str()
        .expect("auth setup should return an access token")
        .to_string()
}

fn failed_history_entry(id: &str, complete_dir: &std::path::Path) -> HistoryEntry {
    HistoryEntry {
        client_removal: None,
        id: id.into(),
        name: "Retry Me".into(),
        category: "Default".into(),
        status: JobStatus::Failed,
        total_bytes: 10,
        downloaded_bytes: 5,
        added_at: Utc::now(),
        completed_at: Utc::now(),
        download_time_secs: None,
        output_dir: complete_dir.join("Retry Me"),
        stages: Vec::new(),
        error_message: Some("missing articles".into()),
        failure_code: Some(JobFailureCode::ArticlesUnavailable),
        post_processing: None,
        delete_archives: None,
        server_stats: Vec::new(),
        nzb_data: Some(sample_nzb_bytes()),
        retry_data: None,
        par2_volumes_fetched: None,
        par2_volumes_available: None,
    }
}

#[tokio::test]
async fn concurrent_history_retries_enqueue_one_job() {
    let app = start_test_server(Vec::new()).await;
    let client = reqwest::Client::new();
    let access = setup_auth(&client, &app.base_url).await;
    client
        .post(format!("{}/api/queue/pause", app.base_url))
        .bearer_auth(&access)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let entry = failed_history_entry("failed-history-entry", &app.complete_dir);
    app.state
        .queue_manager
        .with_db(|db| db.history_insert(&entry).expect("insert history"));

    let url = format!("{}/api/history/failed-history-entry/retry", app.base_url);
    let mut requests = tokio::task::JoinSet::new();
    for _ in 0..5 {
        requests.spawn(client.post(&url).bearer_auth(&access).send());
    }

    let mut job_ids = Vec::new();
    while let Some(response) = requests.join_next().await {
        let response = response.unwrap().unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = response.json::<serde_json::Value>().await.unwrap();
        assert_eq!(body["status"], true, "{body}");
        job_ids.push(body["nzo_ids"][0].as_str().unwrap().to_string());
    }

    let jobs = app.state.queue_manager.get_jobs();
    assert_eq!(jobs.len(), 1, "five retries must enqueue one job");
    assert!(
        job_ids.iter().all(|id| id == &jobs[0].id),
        "every caller gets the one retried job id: {job_ids:?}"
    );
}

/// The body is optional; a password in it becomes the new job's, and a bad
/// body is refused without echoing it.
#[tokio::test]
async fn history_retry_accepts_an_archive_password() {
    let app = start_test_server(Vec::new()).await;
    let client = reqwest::Client::new();
    let access = setup_auth(&client, &app.base_url).await;
    client
        .post(format!("{}/api/queue/pause", app.base_url))
        .bearer_auth(&access)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();

    for (id, body, expected) in [
        ("password", r#"{"password":"s3cr3t pw"}"#, Some("s3cr3t pw")),
        ("object", "{}", None),
        ("null", "null", None),
        ("empty", "", None),
    ] {
        let entry = failed_history_entry(id, &app.complete_dir);
        app.state
            .queue_manager
            .with_db(|db| db.history_insert(&entry).expect("insert history"));
        let response = client
            .post(format!("{}/api/history/{id}/retry", app.base_url))
            .bearer_auth(&access)
            .header("content-type", "application/json")
            .body(body)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "{id}");
        let job_id = response.json::<serde_json::Value>().await.unwrap()["nzo_ids"][0]
            .as_str()
            .unwrap()
            .to_string();
        let jobs = app.state.queue_manager.get_jobs();
        let job = jobs.iter().find(|job| job.id == job_id).unwrap();
        assert_eq!(job.password.as_deref(), expected, "{id}");
    }

    let before = app.state.queue_manager.get_jobs().len();
    let response = client
        .post(format!("{}/api/history/password/retry", app.base_url))
        .bearer_auth(&access)
        .body(r#"{"password":["s3cr3t"]}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(!response.text().await.unwrap().contains("s3cr3t"));
    assert_eq!(app.state.queue_manager.get_jobs().len(), before);
}

/// `PUT /api/queue/{id}/password` sets and clears a queued job's password,
/// and refuses the mask the queue shows in its place.
#[tokio::test]
async fn queued_job_password_can_be_set_and_cleared() {
    let app = start_test_server(Vec::new()).await;
    let client = reqwest::Client::new();
    let access = setup_auth(&client, &app.base_url).await;
    app.state.queue_manager.pause_all();
    let entry = failed_history_entry("queued-password", &app.complete_dir);
    app.state
        .queue_manager
        .with_db(|db| db.history_insert(&entry).expect("insert history"));
    app.state.queue_manager.retry_history_entry(&entry).unwrap();
    let id = app.state.queue_manager.get_jobs()[0].id.clone();
    let put = |id: String, password: &'static str| {
        client
            .put(format!("{}/api/queue/{id}/password", app.base_url))
            .bearer_auth(&access)
            .json(&serde_json::json!({ "password": password }))
            .send()
    };
    let current = || app.state.queue_manager.get_jobs()[0].password.clone();

    assert_eq!(
        put(id.clone(), "s3cr3t pw").await.unwrap().status(),
        StatusCode::OK
    );
    assert_eq!(current().as_deref(), Some("s3cr3t pw"));

    // The native queue shows the mask, which must not be stored.
    let queue: serde_json::Value = client
        .get(format!("{}/api/queue", app.base_url))
        .bearer_auth(&access)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let mask = queue["jobs"][0]["password"].as_str().unwrap().to_string();
    assert_ne!(mask, "s3cr3t pw");
    let refused = client
        .put(format!("{}/api/queue/{id}/password", app.base_url))
        .bearer_auth(&access)
        .json(&serde_json::json!({ "password": mask }))
        .send()
        .await
        .unwrap();
    assert_eq!(refused.status(), StatusCode::BAD_REQUEST);
    assert_eq!(current().as_deref(), Some("s3cr3t pw"));

    assert_eq!(put(id, "").await.unwrap().status(), StatusCode::OK);
    assert_eq!(current(), None);
    assert_eq!(
        put("missing-job".into(), "x").await.unwrap().status(),
        StatusCode::NOT_FOUND
    );
}
