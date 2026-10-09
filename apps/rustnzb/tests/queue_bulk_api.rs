//! BUG-63: `POST /api/queue/bulk` counts an id that matches no job as a
//! failure, and reports which ids failed and why.

use std::sync::Arc;

use arc_swap::ArcSwap;
use chrono::Utc;
use nzb_web::auth::{CredentialStore, StoredCredentials, TokenStore};
use nzb_web::nzb_core::config::AppConfig;
use nzb_web::nzb_core::db::Database;
use nzb_web::nzb_core::models::{JobStatus, NzbJob, Priority};
use nzb_web::{AppState, LogBuffer, QueueManager};
use rustnzb::server::build_router;

struct App {
    base: String,
    state: Arc<AppState>,
    _temp: tempfile::TempDir,
    handle: tokio::task::JoinHandle<()>,
}

impl Drop for App {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

async fn start_app() -> App {
    let temp = tempfile::tempdir().unwrap();
    let config = AppConfig::default();
    let logs = LogBuffer::new();
    let queue = QueueManager::new(
        Vec::new(),
        Database::open_memory().unwrap(),
        temp.path().join("incomplete"),
        temp.path().join("complete"),
        logs.clone(),
        config.general.max_active_downloads,
        config.categories.clone(),
        config.general.min_free_space_bytes,
        config.general.speed_limit_bps,
        false,
        config.general.max_nested_archive_depth,
        config.general.abort_hopeless,
        config.general.early_failure_check,
        config.general.required_completion_pct,
        config.general.article_timeout_secs,
    );
    // Keep every job queued: no servers are configured.
    queue.pause_all();
    let credentials = Arc::new(CredentialStore::new(temp.path().to_path_buf()));
    credentials
        .set_credentials(StoredCredentials {
            username: "admin".into(),
            password: "password".into(),
        })
        .unwrap();
    let state = Arc::new(AppState::new(
        Arc::new(ArcSwap::from_pointee(config)),
        temp.path().join("config.toml"),
        queue,
        logs,
        Arc::new(TokenStore::new()),
        credentials,
    ));
    let router = build_router(state.clone());
    #[cfg(feature = "webdav")]
    let router = router.layer(axum::Extension(None::<Arc<rustnzb::dav::DavHandle>>));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let handle = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });

    let root = temp.path().to_path_buf();
    for id in ["job-a", "job-b"] {
        let job = NzbJob {
            id: id.into(),
            name: id.into(),
            category: "Default".into(),
            status: JobStatus::Queued,
            priority: Priority::Normal,
            total_bytes: 100,
            downloaded_bytes: 0,
            file_count: 0,
            files_completed: 0,
            article_count: 0,
            articles_downloaded: 0,
            articles_failed: 0,
            added_at: Utc::now(),
            completed_at: None,
            work_dir: root.join("incomplete").join(id),
            output_dir: root.join("complete").join(id),
            password: None,
            error_message: None,
            speed_bps: 0,
            server_stats: Vec::new(),
            pp_override: None,
            files: Vec::new(),
        };
        state.queue_manager.add_job(job, None).unwrap();
    }

    App {
        base,
        state,
        _temp: temp,
        handle,
    }
}

async fn login(app: &App, client: &reqwest::Client) -> String {
    let tokens = client
        .post(format!("{}/api/auth/login", app.base))
        .json(&serde_json::json!({"username": "admin", "password": "password"}))
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap();
    tokens["access_token"].as_str().unwrap().to_string()
}

fn job_ids(app: &App) -> Vec<String> {
    app.state
        .queue_manager
        .get_jobs()
        .into_iter()
        .map(|job| job.id)
        .collect()
}

const MISSING: &str = "00000000-0000-0000-0000-000000000000";

async fn bulk(
    app: &App,
    client: &reqwest::Client,
    access: &str,
    body: serde_json::Value,
) -> serde_json::Value {
    let response = client
        .post(format!("{}/api/queue/bulk", app.base))
        .bearer_auth(access)
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 200);
    response.json().await.unwrap()
}

#[tokio::test]
async fn bulk_delete_counts_unknown_ids_as_failed() {
    let app = start_app().await;
    let client = reqwest::Client::new();
    let access = login(&app, &client).await;

    let response = bulk(
        &app,
        &client,
        &access,
        serde_json::json!({"action": "delete", "ids": ["job-a", "job-b", MISSING]}),
    )
    .await;
    assert_eq!(response["status"], false, "{response}");
    assert_eq!(response["succeeded"], 2, "{response}");
    assert_eq!(response["failed"], 1, "{response}");
    let failures = response["failures"].as_array().expect("failures array");
    assert_eq!(failures.len(), 1, "{response}");
    assert_eq!(failures[0]["id"], MISSING);
    assert_eq!(failures[0]["error"]["error_kind"], "job_not_found");
    assert!(job_ids(&app).is_empty());
}

#[tokio::test]
async fn every_bulk_action_counts_unknown_ids_as_failed() {
    let app = start_app().await;
    let client = reqwest::Client::new();
    let access = login(&app, &client).await;

    for (body, succeeded) in [
        (serde_json::json!({"action": "pause"}), 2),
        (serde_json::json!({"action": "priority", "value": 2}), 2),
        (serde_json::json!({"action": "category", "value": "tv"}), 2),
        // Individual resume is refused while downloads are globally paused,
        // so only the unknown id's outcome is checked here.
        (serde_json::json!({"action": "resume"}), 0),
    ] {
        let mut body = body;
        body["ids"] = serde_json::json!(["job-a", "job-b", MISSING]);
        let response = bulk(&app, &client, &access, body.clone()).await;
        assert_eq!(response["status"], false, "{body}: {response}");
        assert_eq!(response["succeeded"], succeeded, "{body}: {response}");
        assert_eq!(response["failed"], 3 - succeeded, "{body}: {response}");
        let failures = response["failures"].as_array().expect("failures array");
        let missing = failures
            .iter()
            .find(|failure| failure["id"] == MISSING)
            .unwrap_or_else(|| panic!("{body}: {response}"));
        if succeeded == 2 {
            assert_eq!(missing["error"]["error_kind"], "job_not_found", "{body}");
        }
    }

    // A fully successful request reports no failures.
    let response = bulk(
        &app,
        &client,
        &access,
        serde_json::json!({"action": "pause", "ids": ["job-a"]}),
    )
    .await;
    assert_eq!(response["status"], true, "{response}");
    assert_eq!(response["succeeded"], 1);
    assert_eq!(response["failures"], serde_json::json!([]));
}
