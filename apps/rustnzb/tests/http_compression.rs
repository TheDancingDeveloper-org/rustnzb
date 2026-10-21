//! Responses that carry bulk text are compressed when the client offers it.
//!
//! The history list is the largest JSON the GUI polls (tens of KB for a page
//! of full rows) and the Angular bundle is several hundred KB; neither is
//! useful uncompressed over a remote link. This asserts the transport
//! behaviour at the HTTP boundary rather than the layer configuration, so a
//! router rebuilt in a different order cannot silently drop it.

mod support;

use support::start_test_server;

/// A client that keeps the API's `Accept-Encoding` untouched, so the test can
/// see exactly what the server chose for the request it was given.
fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .no_gzip()
        .no_brotli()
        .no_deflate()
        .build()
        .expect("client")
}

#[tokio::test]
async fn history_json_is_gzip_encoded_when_offered_and_plain_otherwise() {
    let app = start_test_server(Vec::new()).await;
    let http = client();
    let setup = http
        .post(format!("{}/api/auth/setup", &app.base_url))
        .json(&serde_json::json!({
            "username": "compression-test",
            "password": "compression-test-password"
        }))
        .send()
        .await
        .expect("auth setup");
    assert_eq!(setup.status(), 200);
    let token = setup.json::<serde_json::Value>().await.unwrap()["access_token"]
        .as_str()
        .expect("access token")
        .to_string();

    // Seed enough rows that the JSON clears tower-http's small-body floor.
    let qm = app.state.queue_manager.clone();
    for i in 0..40 {
        let completed_at = chrono::Utc::now() - chrono::Duration::minutes(i);
        qm.with_db(|db| {
            db.history_insert(&nzb_web::nzb_core::models::HistoryEntry {
                client_removal: None,
                id: format!("compression-{i}"),
                name: format!("Some.Release.Name.2026.{i:03}.1080p.WEB-DL"),
                category: "tv".into(),
                status: nzb_web::nzb_core::models::JobStatus::Completed,
                total_bytes: 1_000_000,
                downloaded_bytes: 1_000_000,
                added_at: completed_at - chrono::Duration::minutes(5),
                completed_at,
                download_time_secs: Some(300.0),
                output_dir: "/tmp/out".into(),
                stages: Vec::new(),
                error_message: None,
                failure_code: None,
                post_processing: None,
                delete_archives: None,
                server_stats: Vec::new(),
                nzb_data: None,
                retry_data: None,
                par2_volumes_fetched: None,
                par2_volumes_available: None,
            })
        })
        .expect("seed history");
    }

    let url = format!("{}/api/history?limit=50", app.base_url);
    let compressed = http
        .get(&url)
        .bearer_auth(&token)
        .header(reqwest::header::ACCEPT_ENCODING, "gzip")
        .send()
        .await
        .expect("compressed history");
    assert_eq!(compressed.status(), 200);
    assert_eq!(
        compressed
            .headers()
            .get(reqwest::header::CONTENT_ENCODING)
            .and_then(|v| v.to_str().ok()),
        Some("gzip"),
        "the client offered gzip and the body is far above the small-body floor"
    );
    let encoded_len = compressed.bytes().await.expect("body").len();

    let plain = http
        .get(&url)
        .bearer_auth(&token)
        .header(reqwest::header::ACCEPT_ENCODING, "identity")
        .send()
        .await
        .expect("plain history");
    assert_eq!(plain.status(), 200);
    assert!(
        plain
            .headers()
            .get(reqwest::header::CONTENT_ENCODING)
            .is_none()
    );
    let plain_len = plain.bytes().await.expect("body").len();

    assert!(
        plain_len > 4 * encoded_len,
        "history JSON should shrink several-fold: {plain_len} -> {encoded_len}"
    );
}

#[tokio::test]
async fn the_spa_bundle_is_gzip_encoded_when_offered() {
    let app = start_test_server(Vec::new()).await;
    let http = client();
    let index = http
        .get(format!("{}/", app.base_url))
        .header(reqwest::header::ACCEPT_ENCODING, "gzip")
        .send()
        .await
        .expect("index");
    assert_eq!(index.status(), 200);
    let html = index.text().await.expect("index body");
    // The placeholder build used when the frontend was never built has no
    // hashed bundle; compression of a tiny page is below the layer's floor.
    let Some(script) = html
        .split("src=\"")
        .nth(1)
        .and_then(|rest| rest.split('"').next())
    else {
        return;
    };
    let asset = http
        .get(format!("{}/{}", app.base_url, script))
        .header(reqwest::header::ACCEPT_ENCODING, "gzip")
        .send()
        .await
        .expect("asset");
    assert_eq!(asset.status(), 200);
    assert_eq!(
        asset
            .headers()
            .get(reqwest::header::CONTENT_ENCODING)
            .and_then(|v| v.to_str().ok()),
        Some("gzip"),
        "SPA chunk {script} should be compressed"
    );
}
