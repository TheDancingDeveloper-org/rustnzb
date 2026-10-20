//! `POST /api/history/bulk` deletes and retries several entries in one
//! request, reports an unknown id as a per-id failure, and can clear every
//! entry of a status (WI-1721).

mod support;

use chrono::Utc;
use nzb_web::nzb_core::models::{HistoryEntry, JobFailureCode, JobStatus};
use reqwest::StatusCode;
use support::{sample_nzb_bytes, start_test_server};

async fn setup_auth(client: &reqwest::Client, base_url: &str) -> String {
    let setup = client
        .post(format!("{base_url}/api/auth/setup"))
        .json(&serde_json::json!({
            "username": "bulk-test",
            "password": "bulk-test-password"
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

/// A history row that can be retried, or a failed one with no NZB stored.
fn entry(id: &str, status: JobStatus, with_nzb: bool) -> HistoryEntry {
    aged_entry(id, status, with_nzb, 0)
}

/// As [`entry`], completed `age_days` ago.
fn aged_entry(id: &str, status: JobStatus, with_nzb: bool, age_days: i64) -> HistoryEntry {
    let failed = status == JobStatus::Failed;
    HistoryEntry {
        id: id.into(),
        name: format!("Bulk {id}"),
        category: "Default".into(),
        status,
        total_bytes: 10,
        downloaded_bytes: if failed { 5 } else { 10 },
        added_at: Utc::now() - chrono::Duration::days(age_days),
        completed_at: Utc::now() - chrono::Duration::days(age_days),
        download_time_secs: None,
        output_dir: std::path::PathBuf::from(format!("/tmp/rustnzb-bulk-{id}")),
        stages: Vec::new(),
        error_message: failed.then(|| "missing articles".to_string()),
        failure_code: failed.then_some(JobFailureCode::ArticlesUnavailable),
        post_processing: None,
        delete_archives: None,
        server_stats: Vec::new(),
        nzb_data: with_nzb.then(sample_nzb_bytes),
        retry_data: None,
        par2_volumes_fetched: None,
        par2_volumes_available: None,
    }
}

fn seed(app: &support::TestApp, entries: &[HistoryEntry]) {
    app.state.queue_manager.with_db(|db| {
        for e in entries {
            db.history_insert(e).expect("insert history");
        }
    });
}

fn remaining_ids(app: &support::TestApp) -> Vec<String> {
    app.state.queue_manager.with_db(|db| {
        db.history_summaries(&nzb_web::nzb_core::HistoryFilter::default())
            .expect("list history")
            .into_iter()
            .map(|e| e.id)
            .collect()
    })
}

#[tokio::test]
async fn delete_removes_the_named_entries_and_reports_unknown_ids() {
    let app = start_test_server(Vec::new()).await;
    let client = reqwest::Client::new();
    let access = setup_auth(&client, &app.base_url).await;
    seed(
        &app,
        &[
            entry("a", JobStatus::Completed, false),
            entry("b", JobStatus::Failed, false),
            entry("c", JobStatus::Completed, false),
        ],
    );

    let body = client
        .post(format!("{}/api/history/bulk", app.base_url))
        .bearer_auth(&access)
        .json(&serde_json::json!({"action": "delete", "ids": ["a", "c", "gone"]}))
        .send()
        .await
        .unwrap();
    assert_eq!(body.status(), StatusCode::OK);
    let body = body.json::<serde_json::Value>().await.unwrap();
    assert_eq!(body["succeeded"], 2, "{body}");
    assert_eq!(body["failed"], 1, "{body}");
    assert_eq!(body["status"], false, "{body}");
    assert_eq!(body["failures"][0]["id"], "gone", "{body}");
    assert_eq!(body["failures"][0]["error"]["error_kind"], "not_found");

    assert_eq!(remaining_ids(&app), vec!["b"], "only the named entries go");
}

#[tokio::test]
async fn delete_by_status_clears_every_entry_of_that_status() {
    let app = start_test_server(Vec::new()).await;
    let client = reqwest::Client::new();
    let access = setup_auth(&client, &app.base_url).await;
    seed(
        &app,
        &[
            entry("ok1", JobStatus::Completed, false),
            entry("bad1", JobStatus::Failed, false),
            entry("bad2", JobStatus::Failed, false),
            entry("ok2", JobStatus::Completed, false),
        ],
    );

    let body = client
        .post(format!("{}/api/history/bulk", app.base_url))
        .bearer_auth(&access)
        .json(&serde_json::json!({"action": "delete", "status": "failed"}))
        .send()
        .await
        .unwrap();
    assert_eq!(body.status(), StatusCode::OK);
    let body = body.json::<serde_json::Value>().await.unwrap();
    assert_eq!(body["succeeded"], 2, "{body}");
    assert_eq!(body["failed"], 0, "{body}");
    assert_eq!(body["status"], true, "{body}");

    let mut left = remaining_ids(&app);
    left.sort();
    assert_eq!(left, vec!["ok1", "ok2"]);
}

#[tokio::test]
async fn retry_enqueues_each_retryable_entry_and_fails_the_rest() {
    let app = start_test_server(Vec::new()).await;
    let client = reqwest::Client::new();
    let access = setup_auth(&client, &app.base_url).await;
    // Pause first so the queued retries stay put while the test looks at them.
    client
        .post(format!("{}/api/queue/pause", app.base_url))
        .bearer_auth(&access)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    seed(
        &app,
        &[
            entry("retry-me", JobStatus::Failed, true),
            entry("no-nzb", JobStatus::Failed, false),
            entry("also-retry", JobStatus::Completed, true),
        ],
    );

    let body = client
        .post(format!("{}/api/history/bulk", app.base_url))
        .bearer_auth(&access)
        .json(&serde_json::json!({
            "action": "retry",
            "ids": ["retry-me", "no-nzb", "also-retry"]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(body.status(), StatusCode::OK);
    let body = body.json::<serde_json::Value>().await.unwrap();
    assert_eq!(body["succeeded"], 2, "{body}");
    assert_eq!(body["failed"], 1, "{body}");
    assert_eq!(body["failures"][0]["id"], "no-nzb", "{body}");
    assert_eq!(body["failures"][0]["error"]["status"], 409, "{body}");
    assert_eq!(
        app.state.queue_manager.get_jobs().len(),
        2,
        "one retried job per retryable entry"
    );
}

#[tokio::test]
async fn invalid_bulk_requests_are_400s_and_change_nothing() {
    let app = start_test_server(Vec::new()).await;
    let client = reqwest::Client::new();
    let access = setup_auth(&client, &app.base_url).await;
    seed(&app, &[entry("keep", JobStatus::Completed, false)]);

    for payload in [
        serde_json::json!({"action": "explode", "ids": ["keep"]}),
        serde_json::json!({"action": "delete"}),
        serde_json::json!({"action": "delete", "ids": ["keep"], "status": "bogus"}),
        serde_json::json!({"action": "retry", "ids": ["keep"], "status": "failed"}),
    ] {
        let response = client
            .post(format!("{}/api/history/bulk", app.base_url))
            .bearer_auth(&access)
            .json(&payload)
            .send()
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::BAD_REQUEST,
            "payload {payload} must be rejected"
        );
        let body = response.json::<serde_json::Value>().await.unwrap();
        assert_eq!(body["error_kind"], "bad_request", "{body}");
    }
    assert_eq!(remaining_ids(&app), vec!["keep"]);
}

#[tokio::test]
async fn delete_by_status_honours_the_window_category_and_search() {
    let app = start_test_server(Vec::new()).await;
    let client = reqwest::Client::new();
    let access = setup_auth(&client, &app.base_url).await;
    let mut old = aged_entry("old-fail", JobStatus::Failed, false, 60);
    old.category = "tv".into();
    let mut other = aged_entry("tv-fail", JobStatus::Failed, false, 0);
    other.category = "tv".into();
    let mut recent = aged_entry("recent-fail", JobStatus::Failed, false, 0);
    recent.category = "movies".into();
    seed(&app, &[old, other, recent]);

    // A 7-day window clears the recent failures in every category...
    let response = client
        .post(format!("{}/api/history/bulk", app.base_url))
        .bearer_auth(&access)
        .json(&serde_json::json!({"action": "delete", "status": "failed", "days": 7}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.json::<serde_json::Value>().await.unwrap();
    assert_eq!(body["succeeded"], 2, "{body}");
    assert_eq!(remaining_ids(&app), vec!["old-fail"]);

    // ...and category/search narrow what is left of it.
    let response = client
        .post(format!("{}/api/history/bulk", app.base_url))
        .bearer_auth(&access)
        .json(&serde_json::json!({
            "action": "delete",
            "status": "failed",
            "search": "no-such-name"
        }))
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap();
    assert_eq!(response["succeeded"], 0, "{response}");
    assert_eq!(remaining_ids(&app), vec!["old-fail"]);
}
