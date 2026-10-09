//! Durable end-to-end coverage for RustNZB's SABnzbd-compatible API
//! (`/sabnzbd/api`), exercised over real HTTP against a live server instance
//! -- not just the in-process handler-function tests in
//! `nzb-web/src/sabnzbd_compat.rs`.
//!
//! This suite exists because rustnzb's SAB compatibility layer drifted from
//! the real protocol several times without any test catching it (see
//! TheDancingDeveloper-org/rustnzb#65, #71-#76): a real endpoint
//! (`mode=queue&name=priority`) was entirely unreachable, numeric priority
//! codes were shifted by one, `get_cats`/`get_scripts` didn't match what a
//! compliant client actually receives, and `addurl` didn't work over GET at
//! all. Each assertion below is paired with the exact upstream SABnzbd
//! (`sabnzbd/sabnzbd@5.1.x`) source it was verified against, so a future
//! change that reintroduces one of these regressions fails immediately
//! instead of silently shipping.

mod support;

use support::{sample_nzb_bytes, start_test_server, start_test_server_broken_storage};

/// Serves `body` once over a raw TCP listener, returning the URL to fetch it
/// from. Used to exercise `mode=addurl` (a real HTTP GET fetch) without
/// depending on an external network.
async fn spawn_nzb_server(body: Vec<u8>) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral test server");
    let addr = listener.local_addr().expect("test server local addr");

    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept test connection");
        let mut buf = [0u8; 1024];
        let _ = socket.read(&mut buf).await;
        let mut response = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: application/x-nzb\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .into_bytes();
        response.extend_from_slice(&body);
        let _ = socket.write_all(&response).await;
        let _ = socket.shutdown().await;
    });

    format!("http://{addr}/test.nzb")
}

/// `mode=version` -- baseline connectivity check every SAB-compatible client
/// performs first.
#[tokio::test]
async fn version_reports_a_string() {
    let app = start_test_server(Vec::new()).await;
    let client = reqwest::Client::new();

    let response: serde_json::Value = client
        .get(format!("{}/sabnzbd/api?mode=version", app.base_url))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    assert!(response["version"].as_str().is_some());
}

/// `mode=addfile` (POST multipart) must apply `cat`/`priority`, and the
/// resulting queue slot must report SABnzbd's real priority vocabulary
/// ("High", not "Normal") -- regression coverage for the numeric mapping fix
/// (rustnzb#71) verified against `sabnzbd/constants.py: HIGH_PRIORITY = 1`.
#[tokio::test]
async fn addfile_applies_category_and_priority_and_reports_them_in_queue() {
    // Like SABnzbd, an unknown `cat` falls back to the default category, so
    // the category under test must be configured.
    let app = support::start_test_server_with_categories(&["tv"]).await;
    let client = reqwest::Client::new();

    let form = reqwest::multipart::Form::new()
        .text("mode", "addfile")
        .text("cat", "tv")
        .text("priority", "1")
        .part(
            "name",
            reqwest::multipart::Part::bytes(sample_nzb_bytes())
                .file_name("sample.nzb")
                .mime_str("application/x-nzb")
                .unwrap(),
        );

    let add_response: serde_json::Value = client
        .post(format!("{}/sabnzbd/api", app.base_url))
        .multipart(form)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        add_response["status"],
        serde_json::json!(true),
        "resp={add_response:?}"
    );
    let nzo_id = add_response["nzo_ids"][0].as_str().unwrap().to_string();

    let queue: serde_json::Value = client
        .get(format!("{}/sabnzbd/api?mode=queue", app.base_url))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let slot = queue["queue"]["slots"]
        .as_array()
        .unwrap()
        .iter()
        .find(|slot| slot["nzo_id"] == nzo_id)
        .expect("added job present in queue");
    assert_eq!(slot["cat"], "tv");
    // sabnzbd/constants.py: HIGH_PRIORITY = 1 -> "High", not "Normal".
    assert_eq!(slot["priority"], "High");
}

/// `mode=addfile` must report a failed enqueue as SABnzbd's
/// `{"status": false, "error": ...}` on HTTP 200 -- never a 5xx. Real
/// SABnzbd's `api.py` always returns 200 with a `status` field, and Sonarr
/// treats a 500 from a SAB download client as a hard, non-retryable grab
/// failure even though the NZB parsed fine. Regression coverage for
/// TheDancingDeveloper-org/rustnzb#129, where the queue insert failed after
/// the handler had already logged success, and the error surfaced to the
/// client as an opaque HTTP 500.
#[tokio::test]
async fn addfile_reports_enqueue_failure_as_status_false_not_500() {
    let app = start_test_server_broken_storage().await;
    let client = reqwest::Client::new();

    let form = reqwest::multipart::Form::new()
        .text("mode", "addfile")
        .part(
            "name",
            reqwest::multipart::Part::bytes(sample_nzb_bytes())
                .file_name("sample.nzb")
                .mime_str("application/x-nzb")
                .unwrap(),
        );

    let response = client
        .post(format!("{}/sabnzbd/api", app.base_url))
        .multipart(form)
        .send()
        .await
        .unwrap();

    assert_eq!(
        response.status(),
        reqwest::StatusCode::OK,
        "SAB addfile must return HTTP 200 even when the enqueue fails, not a 5xx"
    );
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(
        body["status"],
        serde_json::json!(false),
        "failed enqueue must report status:false, resp={body:?}"
    );
    assert!(
        body["error"].as_str().is_some(),
        "failed enqueue must include an error message, resp={body:?}"
    );
}

/// `mode=addurl` fetches a remote NZB and has no file body to upload, so
/// real SABnzbd (and clients like NZB360/Sonarr/Radarr) issue it as a plain
/// GET. This exercises two things at once:
///
///  - Dispatch (rustnzb#65/PR#70): a GET `mode=addurl` must reach the addurl
///    handler and return a structured SABnzbd `status` field, not fall through
///    to "Unknown mode".
///  - SSRF guard (rustnzb#129 review): the caller-supplied URL is validated
///    before fetching, so a private/loopback target is refused with
///    `{status:false}` on HTTP 200 (never fetched, never enqueued). A local
///    server standing by proves the guard blocks *before* the request.
#[tokio::test]
async fn addurl_over_get_is_dispatched_and_ssrf_guard_blocks_loopback() {
    let app = start_test_server(Vec::new()).await;
    let client = reqwest::Client::new();
    let nzb_url = spawn_nzb_server(sample_nzb_bytes()).await; // http://127.0.0.1:PORT/...

    let add_response: serde_json::Value = client
        .get(format!(
            "{}/sabnzbd/api?mode=addurl&name={}&cat=movies",
            app.base_url, nzb_url
        ))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    // Dispatched to the addurl handler (structured status), and refused by the
    // SSRF guard rather than fetched.
    assert_eq!(
        add_response["status"],
        serde_json::json!(false),
        "loopback addurl must be refused by the SSRF guard, resp={add_response:?}"
    );
    assert!(
        add_response["error"]
            .as_str()
            .unwrap_or_default()
            .contains("private/reserved"),
        "expected SSRF rejection reason, resp={add_response:?}"
    );

    // Nothing should have been enqueued.
    let queue: serde_json::Value = client
        .get(format!("{}/sabnzbd/api?mode=queue", app.base_url))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        queue["queue"]["slots"]
            .as_array()
            .map(|s| s.is_empty())
            .unwrap_or(true),
        "SSRF-refused addurl must not enqueue anything, resp={queue:?}"
    );
}

/// `mode=get_cats` must report the default category as the literal `"*"`
/// sentinel, matching `sabnzbd/api.py::list_cats(default=False)` -- not a
/// display name like "Default". Regression coverage for rustnzb#73.
#[tokio::test]
async fn get_cats_reports_sabnzbd_default_sentinel() {
    let app = start_test_server(Vec::new()).await;
    let client = reqwest::Client::new();

    let response: serde_json::Value = client
        .get(format!("{}/sabnzbd/api?mode=get_cats", app.base_url))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    assert_eq!(response["categories"], serde_json::json!(["*"]));
}

/// `mode=get_scripts` is a real top-level SABnzbd mode
/// (`sabnzbd/api.py::_api_table["get_scripts"]`) that used to fall through
/// to "Unknown mode". Regression coverage for rustnzb#74.
#[tokio::test]
async fn get_scripts_reports_none() {
    let app = start_test_server(Vec::new()).await;
    let client = reqwest::Client::new();

    let response: serde_json::Value = client
        .get(format!("{}/sabnzbd/api?mode=get_scripts", app.base_url))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    assert_eq!(response["scripts"], serde_json::json!(["None"]));
}

async fn add_queued_job(app: &support::TestApp, client: &reqwest::Client) -> String {
    let form = reqwest::multipart::Form::new()
        .text("mode", "addfile")
        .part(
            "name",
            reqwest::multipart::Part::bytes(sample_nzb_bytes())
                .file_name("sample.nzb")
                .mime_str("application/x-nzb")
                .unwrap(),
        );
    let add_response: serde_json::Value = client
        .post(format!("{}/sabnzbd/api", app.base_url))
        .multipart(form)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(add_response["status"], serde_json::json!(true));
    add_response["nzo_ids"][0].as_str().unwrap().to_string()
}

/// Real SABnzbd has no top-level `mode=priority`; priority changes are a
/// sub-command of `mode=queue` (`sabnzbd/api.py::_api_queue_table["priority"]`).
/// Regression coverage for rustnzb#72: this route used to silently fall
/// through to a plain queue listing instead of applying the change.
#[tokio::test]
async fn queue_priority_subcommand_changes_priority_over_http() {
    let app = start_test_server(Vec::new()).await;
    let client = reqwest::Client::new();
    let nzo_id = add_queued_job(&app, &client).await;

    let response: serde_json::Value = client
        .get(format!(
            "{}/sabnzbd/api?mode=queue&name=priority&value={nzo_id}&value2=2",
            app.base_url
        ))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(response["status"], serde_json::json!(true));

    let queue: serde_json::Value = client
        .get(format!("{}/sabnzbd/api?mode=queue", app.base_url))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let slot = queue["queue"]["slots"]
        .as_array()
        .unwrap()
        .iter()
        .find(|slot| slot["nzo_id"] == nzo_id)
        .expect("job present in queue");
    // sabnzbd/constants.py: FORCE_PRIORITY = 2 -> "Force".
    assert_eq!(slot["priority"], "Force");
}

/// Real SABnzbd's rename is also a `mode=queue` sub-command
/// (`_api_queue_table["rename"]`). Regression coverage for rustnzb#72.
#[tokio::test]
async fn queue_rename_subcommand_renames_over_http() {
    let app = start_test_server(Vec::new()).await;
    let client = reqwest::Client::new();
    let nzo_id = add_queued_job(&app, &client).await;

    let response: serde_json::Value = client
        .get(format!(
            "{}/sabnzbd/api?mode=queue&name=rename&value={nzo_id}&value2=Renamed%20Job",
            app.base_url
        ))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(response["status"], serde_json::json!(true));

    let queue: serde_json::Value = client
        .get(format!("{}/sabnzbd/api?mode=queue", app.base_url))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let slot = queue["queue"]["slots"]
        .as_array()
        .unwrap()
        .iter()
        .find(|slot| slot["nzo_id"] == nzo_id)
        .expect("job present in queue");
    assert_eq!(slot["filename"], "Renamed Job");
}

/// `mode=change_cat` must accept a comma-separated `value` list, applying
/// the category to every matching job in one call
/// (`sabnzbd/api.py::_api_change_cat` -> `clean_comma_separated_list`).
/// Regression coverage for rustnzb#76.
#[tokio::test]
async fn change_cat_applies_to_multiple_jobs_over_http() {
    let app = start_test_server(Vec::new()).await;
    let client = reqwest::Client::new();
    let first = add_queued_job(&app, &client).await;
    let second = add_queued_job(&app, &client).await;

    let response: serde_json::Value = client
        .get(format!(
            "{}/sabnzbd/api?mode=change_cat&value={first},{second}&value2=movies",
            app.base_url
        ))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(response["status"], serde_json::json!(true));

    let queue: serde_json::Value = client
        .get(format!("{}/sabnzbd/api?mode=queue", app.base_url))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let slots = queue["queue"]["slots"].as_array().unwrap();
    for nzo_id in [&first, &second] {
        let slot = slots
            .iter()
            .find(|slot| slot["nzo_id"] == *nzo_id)
            .expect("job present in queue");
        assert_eq!(slot["cat"], "movies");
    }
}

/// `mode=queue&name=delete` must accept a comma-separated `value` list,
/// removing every matching job in one call
/// (`sabnzbd/api.py::_api_queue_delete` -> `clean_comma_separated_list`).
/// Regression coverage for rustnzb#75.
#[tokio::test]
async fn queue_delete_removes_multiple_jobs_over_http() {
    let app = start_test_server(Vec::new()).await;
    let client = reqwest::Client::new();
    let first = add_queued_job(&app, &client).await;
    let second = add_queued_job(&app, &client).await;

    let response: serde_json::Value = client
        .get(format!(
            "{}/sabnzbd/api?mode=queue&name=delete&value={first},{second}",
            app.base_url
        ))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(response["status"], serde_json::json!(true));

    let queue: serde_json::Value = client
        .get(format!("{}/sabnzbd/api?mode=queue", app.base_url))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let slots = queue["queue"]["slots"].as_array().unwrap();
    assert!(
        slots
            .iter()
            .all(|slot| slot["nzo_id"] != first && slot["nzo_id"] != second)
    );
}
