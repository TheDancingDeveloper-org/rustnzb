mod support;

use std::time::{Duration, Instant};

use nzb_nntp::testutil::{MockConfig, MockNntpServer, test_config};
use support::{NzbFixture, start_test_server};

#[tokio::test]
async fn upload_nzb_downloads_via_mock_server_and_reaches_history() {
    let fixture = NzbFixture::new()
        .add_file(
            "mock-success.bin",
            &[("mock-article-001@test", b"hello from mock nntp")],
        )
        .build();

    let server = MockNntpServer::start(MockConfig {
        articles: fixture.encoded_articles(),
        response_delay: Some(Duration::from_millis(100)),
        ..MockConfig::default()
    })
    .await;

    let mut config = test_config(server.port());
    config.id = "mock-primary".into();
    config.name = "Mock Primary".into();
    config.connections = 4;

    let app = start_test_server(vec![config]).await;
    let client = reqwest::Client::new();
    let setup = client
        .post(format!("{}/api/auth/setup", app.base_url))
        .json(&serde_json::json!({
            "username": "mock-test",
            "password": "mock-test-password"
        }))
        .send()
        .await
        .expect("auth setup failed");
    assert_eq!(setup.status(), 200);
    let access = setup.json::<serde_json::Value>().await.unwrap()["access_token"]
        .as_str()
        .expect("auth setup should return an access token")
        .to_string();

    let part = reqwest::multipart::Part::bytes(fixture.xml.clone())
        .file_name("mock-download.nzb")
        .mime_str("application/x-nzb")
        .unwrap();
    let form = reqwest::multipart::Form::new().part("file", part);

    let resp = client
        .post(format!("{}/api/queue/add", app.base_url))
        .bearer_auth(&access)
        .multipart(form)
        .send()
        .await
        .expect("Upload failed");
    assert_eq!(resp.status(), 200);
    let add_result: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(add_result["status"], true);

    let deadline = Instant::now() + Duration::from_secs(10);
    let mut observed_transfer = false;
    let mut observed_open_socket = false;
    let history_entry = loop {
        let status: serde_json::Value = client
            .get(format!("{}/api/status", app.base_url))
            .bearer_auth(&access)
            .send()
            .await
            .expect("status request failed")
            .json()
            .await
            .expect("status response should be JSON");
        let connections = status["nntp_connections"]
            .as_array()
            .expect("nntp_connections should be an array");
        for connection in connections {
            let connected = connection["connected"].as_u64().unwrap_or(0);
            let active = connection["active"].as_u64().unwrap_or(0);
            let limit = connection["limit"].as_u64().unwrap_or(0);
            assert!(
                active <= connected && connected <= limit,
                "status row out of range: {connection}"
            );
            observed_transfer |= active > 0;
            observed_open_socket |= connected > 0;
        }

        let history: serde_json::Value = client
            .get(format!("{}/api/history?limit=10", app.base_url))
            .bearer_auth(&access)
            .send()
            .await
            .expect("history request failed")
            .json()
            .await
            .expect("history response should be JSON");

        if let Some(entry) = history["entries"]
            .as_array()
            .and_then(|entries| entries.first())
        {
            break entry.clone();
        }

        assert!(
            Instant::now() < deadline,
            "job did not reach history within timeout"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    };

    assert_eq!(history_entry["status"], "completed");
    assert_eq!(history_entry["name"], "mock-download");
    assert!(
        observed_transfer,
        "API never exposed the in-flight transfer"
    );
    assert!(
        observed_open_socket,
        "API never exposed an open NNTP socket"
    );

    // WI-2086: once the job is done nothing is transferring, but the
    // workers keep their sockets open for the next job, and the pool panel
    // reports them as connected rather than pretending the pool is empty.
    let idle_status: serde_json::Value = client
        .get(format!("{}/api/status", app.base_url))
        .bearer_auth(&access)
        .send()
        .await
        .expect("idle status request failed")
        .json()
        .await
        .expect("idle status response should be JSON");
    let idle_rows = idle_status["nntp_connections"].as_array().unwrap();
    assert!(
        idle_rows.iter().all(|connection| connection["active"] == 0),
        "idle API status still reports transfers in flight: {idle_status}"
    );
    assert!(
        idle_rows
            .iter()
            .any(|connection| connection["connected"].as_u64().unwrap_or(0) > 0),
        "idle API status hides the open sockets the workers still hold: {idle_status}"
    );

    let queue_after_history: serde_json::Value = client
        .get(format!("{}/api/queue", app.base_url))
        .bearer_auth(&access)
        .send()
        .await
        .expect("queue request after history failed")
        .json()
        .await
        .expect("queue response after history should be JSON");
    assert_eq!(
        queue_after_history["jobs"].as_array().map(Vec::len),
        Some(0),
        "a terminal job persisted to history must not remain in the active queue: {queue_after_history}"
    );

    let downloaded = app
        .complete_dir
        .join("Default")
        .join("mock-download")
        .join("mock-success.bin");
    assert!(
        downloaded.exists(),
        "downloaded file missing: {}",
        downloaded.display()
    );
    assert_eq!(
        std::fs::read(&downloaded).expect("downloaded file should be readable"),
        b"hello from mock nntp"
    );
}
