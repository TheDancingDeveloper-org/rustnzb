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
        server_stats: Vec::new(),
        nzb_data: Some(sample_nzb_bytes()),
        retry_data: None,
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
