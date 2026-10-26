//! Step-up re-auth on sensitive config writes (WI-2497).

mod support;

use reqwest::StatusCode;
use support::start_test_server;

async fn setup(client: &reqwest::Client, base_url: &str) -> String {
    let setup = client
        .post(format!("{base_url}/api/auth/setup"))
        .json(&serde_json::json!({
            "username": "step-up-test",
            "password": "step-up-test-password"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(setup.status(), StatusCode::OK);
    setup.json::<serde_json::Value>().await.unwrap()["access_token"]
        .as_str()
        .unwrap()
        .to_string()
}

#[tokio::test]
async fn sensitive_write_requires_then_accepts_the_confirm_header() {
    let app = start_test_server(Vec::new()).await;
    let client = reqwest::Client::new();
    let token = setup(&client, &app.base_url).await;
    let url = format!("{}/api/config/general", app.base_url);

    let missing = client
        .put(&url)
        .bearer_auth(&token)
        .json(&serde_json::json!({"port": 9191}))
        .send()
        .await
        .unwrap();
    assert_eq!(missing.status(), StatusCode::FORBIDDEN);
    let body: serde_json::Value = missing.json().await.unwrap();
    assert_eq!(body["error_kind"], "step_up_required");

    let wrong = client
        .put(&url)
        .bearer_auth(&token)
        .header("X-Confirm-Password", "not-the-password")
        .json(&serde_json::json!({"port": 9191}))
        .send()
        .await
        .unwrap();
    assert_eq!(wrong.status(), StatusCode::FORBIDDEN);
    let body: serde_json::Value = wrong.json().await.unwrap();
    assert_eq!(body["error_kind"], "step_up_required");

    let ok = client
        .put(&url)
        .bearer_auth(&token)
        .header("X-Confirm-Password", "step-up-test-password")
        .json(&serde_json::json!({"port": 9191}))
        .send()
        .await
        .unwrap();
    assert_eq!(ok.status(), StatusCode::OK, "{}", ok.text().await.unwrap());

    // Window still open: no header needed for the next sensitive write.
    let again = client
        .put(&url)
        .bearer_auth(&token)
        .json(&serde_json::json!({"port": 9192}))
        .send()
        .await
        .unwrap();
    assert_eq!(again.status(), StatusCode::OK);

    let rotate = format!("{}/api/config/sabnzbd-api-key/rotate", app.base_url);
    let by_key = client
        .post(&rotate)
        .header("X-Api-Key", "unused")
        .header("X-Confirm-Password", "step-up-test-password")
        .json(&serde_json::json!({}))
        .send()
        .await
        .unwrap();
    // No API key yet, or if one exists after rotate above: either 401 or 403
    // forbidden. Rotate with the bearer to mint a key, then retry.
    let minted = client
        .post(&rotate)
        .bearer_auth(&token)
        .json(&serde_json::json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(minted.status(), StatusCode::OK);
    let key = minted.json::<serde_json::Value>().await.unwrap()["api_key"]
        .as_str()
        .unwrap()
        .to_string();
    let bypass = client
        .post(&rotate)
        .header("X-Api-Key", &key)
        .header("X-Confirm-Password", "step-up-test-password")
        .json(&serde_json::json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(bypass.status(), StatusCode::FORBIDDEN);
    let body: serde_json::Value = bypass.json().await.unwrap();
    assert_eq!(body["error_kind"], "step_up_forbidden");
    let _ = by_key;
}

#[tokio::test]
async fn ordinary_general_edits_do_not_need_step_up() {
    let app = start_test_server(Vec::new()).await;
    let client = reqwest::Client::new();
    let token = setup(&client, &app.base_url).await;
    let put = client
        .put(format!("{}/api/config/general", app.base_url))
        .bearer_auth(&token)
        .json(&serde_json::json!({"max_active_downloads": 2}))
        .send()
        .await
        .unwrap();
    assert_eq!(
        put.status(),
        StatusCode::OK,
        "{}",
        put.text().await.unwrap()
    );
}
