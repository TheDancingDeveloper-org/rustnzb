//! Non-Docker first-run setup: one call creates the admin login, paths and a
//! server, then locks. A container never enters it.

mod support;

use nzb_web::nzb_core::config::AppConfig;
use reqwest::StatusCode;
use support::start_test_server;

fn body(incomplete: &str, complete: &str) -> serde_json::Value {
    serde_json::json!({
        "username": "admin",
        "password": "first-run-password",
        "incomplete_dir": incomplete,
        "complete_dir": complete,
        "server_host": "news.example.com",
        "server_port": 563,
        "server_ssl": true
    })
}

#[tokio::test]
async fn first_run_writes_config_and_then_locks() {
    let app = start_test_server(Vec::new()).await;
    let client = reqwest::Client::new();
    let status = client
        .get(format!("{}/api/setup/status", app.base_url))
        .send()
        .await
        .unwrap();
    assert_eq!(status.status(), StatusCode::OK);
    let status: serde_json::Value = status.json().await.unwrap();
    assert_eq!(status["first_run"], true);

    let incomplete = app.complete_dir.parent().unwrap().join("incomplete");
    let created = client
        .post(format!("{}/api/setup/first-run", app.base_url))
        .json(&body(
            incomplete.to_str().unwrap(),
            app.complete_dir.to_str().unwrap(),
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(created.status(), StatusCode::OK);
    let tokens: serde_json::Value = created.json().await.unwrap();
    assert!(tokens["access_token"].as_str().unwrap().len() > 8);

    let on_disk = AppConfig::load(&app.config_path).unwrap();
    assert_eq!(on_disk.servers.len(), 1);
    assert_eq!(on_disk.servers[0].host, "news.example.com");
    assert_eq!(on_disk.general.complete_dir, app.complete_dir);
    assert!(
        app.config_path
            .parent()
            .unwrap()
            .join("credentials.json")
            .is_file()
    );

    // After credentials exist the route is authenticated. A valid session
    // still cannot re-enter setup.
    let token = tokens["access_token"].as_str().unwrap();
    let again = client
        .post(format!("{}/api/setup/first-run", app.base_url))
        .bearer_auth(token)
        .json(&body(
            incomplete.to_str().unwrap(),
            app.complete_dir.to_str().unwrap(),
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(again.status(), StatusCode::FORBIDDEN);
    let still = AppConfig::load(&app.config_path).unwrap();
    assert_eq!(still.servers.len(), 1);
    assert_eq!(still.servers[0].host, "news.example.com");
}

#[tokio::test]
async fn container_skips_first_run() {
    // SAFETY: rust tests in one binary share the process environment. This
    // sets the flag only around the requests below and clears it before
    // returning. A panic would leave it set; the flag only refuses first-run.
    unsafe { std::env::set_var("RUSTNZB_CONTAINER", "1") };
    let app = start_test_server(Vec::new()).await;
    let client = reqwest::Client::new();
    let status = client
        .get(format!("{}/api/setup/status", app.base_url))
        .send()
        .await
        .unwrap();
    let status: serde_json::Value = status.json().await.unwrap();
    assert_eq!(status["first_run"], false);
    assert_eq!(status["container"], true);

    let denied = client
        .post(format!("{}/api/setup/first-run", app.base_url))
        .json(&body("/tmp/incomplete", "/tmp/complete"))
        .send()
        .await
        .unwrap();
    assert_eq!(denied.status(), StatusCode::FORBIDDEN);
    assert!(
        !app.config_path
            .parent()
            .unwrap()
            .join("credentials.json")
            .exists()
    );
    unsafe { std::env::remove_var("RUSTNZB_CONTAINER") };
}
