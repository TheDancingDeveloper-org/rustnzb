//! BUG-62: `POST /api/queue/sort` and SABnzbd `mode=queue&name=sort` honour
//! the requested sort field and direction instead of always sorting by
//! remaining percentage.

use std::sync::Arc;

use arc_swap::ArcSwap;
use chrono::{Duration, Utc};
use nzb_web::auth::{CredentialStore, StoredCredentials, TokenStore};
use nzb_web::nzb_core::config::AppConfig;
use nzb_web::nzb_core::db::Database;
use nzb_web::nzb_core::models::{JobStatus, NzbJob, Priority};
use nzb_web::{AppState, LogBuffer, QueueManager};
use rustnzb::server::build_router;

const API_KEY: &str = "sort-test-key";

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
    let mut config = AppConfig::default();
    config.general.api_key = Some(API_KEY.into());
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
    // Keep every job queued: the test only exercises ordering.
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
    let now = Utc::now();
    // (id, name, total, downloaded, priority, minutes old)
    for (id, name, total, downloaded, priority, age) in [
        ("job-a", "charlie", 300u64, 0u64, Priority::Normal, 10i64),
        ("job-b", "Alpha", 100, 90, Priority::High, 30),
        ("job-c", "bravo", 200, 20, Priority::Normal, 20),
    ] {
        let job = NzbJob {
            id: id.into(),
            name: name.into(),
            category: "Default".into(),
            status: JobStatus::Queued,
            priority,
            total_bytes: total,
            downloaded_bytes: downloaded,
            file_count: 0,
            files_completed: 0,
            article_count: 0,
            articles_downloaded: 0,
            articles_failed: 0,
            added_at: now - Duration::minutes(age),
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

fn order(app: &App) -> Vec<String> {
    app.state
        .queue_manager
        .get_jobs()
        .into_iter()
        .map(|job| job.id)
        .collect()
}

async fn sort(
    app: &App,
    client: &reqwest::Client,
    access: &str,
    body: serde_json::Value,
) -> (u16, serde_json::Value) {
    let response = client
        .post(format!("{}/api/queue/sort", app.base))
        .bearer_auth(access)
        .json(&body)
        .send()
        .await
        .unwrap();
    let status = response.status().as_u16();
    let text = response.text().await.unwrap();
    (
        status,
        serde_json::from_str(&text).unwrap_or(serde_json::Value::Null),
    )
}

#[tokio::test]
async fn native_sort_honours_sort_by_and_direction() {
    let app = start_app().await;
    let client = reqwest::Client::new();
    let access = login(&app, &client).await;

    for (body, expected) in [
        (
            serde_json::json!({"sort_by": "name", "direction": "asc"}),
            ["job-b", "job-c", "job-a"],
        ),
        (
            serde_json::json!({"sort_by": "name", "direction": "desc"}),
            ["job-a", "job-c", "job-b"],
        ),
        (
            serde_json::json!({"sort_by": "size"}),
            ["job-b", "job-c", "job-a"],
        ),
        (
            serde_json::json!({"sort_by": "size", "ascending": false}),
            ["job-a", "job-c", "job-b"],
        ),
        // `direction` wins over `ascending` when both are sent.
        (
            serde_json::json!({"sort_by": "size", "ascending": false, "direction": "asc"}),
            ["job-b", "job-c", "job-a"],
        ),
        (
            serde_json::json!({"sort_by": "age", "direction": "asc"}),
            ["job-a", "job-c", "job-b"],
        ),
        (
            serde_json::json!({"sort_by": "priority", "direction": "desc"}),
            ["job-b", "job-a", "job-c"],
        ),
        // Legacy body: remaining percentage, ascending by default.
        (serde_json::json!({}), ["job-b", "job-c", "job-a"]),
        (
            serde_json::json!({"ascending": false}),
            ["job-a", "job-c", "job-b"],
        ),
    ] {
        let (status, response) = sort(&app, &client, &access, body.clone()).await;
        assert_eq!(status, 200, "{body}: {response}");
        assert_eq!(response["status"], true, "{body}");
        assert_eq!(order(&app), expected, "{body}");
    }
}

#[tokio::test]
async fn native_sort_rejects_unknown_field_or_direction() {
    let app = start_app().await;
    let client = reqwest::Client::new();
    let access = login(&app, &client).await;
    let before = order(&app);

    for body in [
        serde_json::json!({"sort_by": "colour"}),
        serde_json::json!({"sort_by": "name", "direction": "sideways"}),
    ] {
        let (status, response) = sort(&app, &client, &access, body.clone()).await;
        assert_eq!(status, 400, "{body}: {response}");
        assert_eq!(response["error_kind"], "bad_request", "{body}");
        assert_eq!(order(&app), before, "{body} must not reorder the queue");
    }
}

async fn sab(app: &App, client: &reqwest::Client, query: &str) -> serde_json::Value {
    client
        .get(format!(
            "{}/api?mode=queue&name=sort&output=json&apikey={API_KEY}&{query}",
            app.base
        ))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

#[tokio::test]
async fn sab_queue_sort_honours_sort_and_dir() {
    let app = start_app().await;
    let client = reqwest::Client::new();

    for (query, expected) in [
        ("sort=name&dir=asc", ["job-b", "job-c", "job-a"]),
        ("sort=name&dir=desc", ["job-a", "job-c", "job-b"]),
        ("sort=size&dir=desc", ["job-a", "job-c", "job-b"]),
        ("sort=avg_age&dir=asc", ["job-a", "job-c", "job-b"]),
        ("sort=remaining&dir=asc", ["job-b", "job-c", "job-a"]),
        ("sort=remaining&dir=desc", ["job-a", "job-c", "job-b"]),
    ] {
        let response = sab(&app, &client, query).await;
        assert_eq!(response["status"], true, "{query}: {response}");
        assert_eq!(order(&app), expected, "{query}");
    }

    let before = order(&app);
    let response = sab(&app, &client, "sort=colour&dir=asc").await;
    assert_eq!(response["status"], false, "{response}");
    assert_eq!(order(&app), before);
}
