mod support;

use reqwest::StatusCode;
use rustnzb::admissions::payload_digest;
use support::{sample_nzb_bytes, sample_nzb_variant_bytes, start_test_server};

const ADMISSION_KEY: &str = "newsgroups-acquisition-018f";

/// Complete first-run auth setup and return a bearer access token. Main gates
/// the `/api` routes behind auth middleware, so every request below carries it.
async fn setup_auth(client: &reqwest::Client, base_url: &str) -> String {
    let setup = client
        .post(format!("{base_url}/api/auth/setup"))
        .json(&serde_json::json!({
            "username": "admission-test",
            "password": "admission-test-password"
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

async fn upload(
    client: &reqwest::Client,
    base_url: &str,
    access: &str,
    key: &str,
    name: &str,
    payload: Vec<u8>,
) -> reqwest::Response {
    let part = reqwest::multipart::Part::bytes(payload)
        .file_name(name.to_string())
        .mime_str("application/x-nzb")
        .unwrap();
    client
        .post(format!("{base_url}/api/queue/add"))
        .bearer_auth(access)
        .header("Idempotency-Key", key)
        .multipart(reqwest::multipart::Form::new().part("file", part))
        .send()
        .await
        .unwrap()
}

#[tokio::test]
async fn exact_replay_returns_one_job_and_conflicting_payload_fails_closed() {
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

    let payload = sample_nzb_bytes();
    let first = upload(
        &client,
        &app.base_url,
        &access,
        ADMISSION_KEY,
        "first.nzb",
        payload.clone(),
    )
    .await;
    assert_eq!(first.status(), StatusCode::OK);
    let first = first.json::<serde_json::Value>().await.unwrap();
    let first_job = first["nzo_ids"][0].as_str().unwrap();

    let replay = upload(
        &client,
        &app.base_url,
        &access,
        ADMISSION_KEY,
        "renamed.nzb",
        payload.clone(),
    )
    .await;
    assert_eq!(replay.status(), StatusCode::OK);
    let replay = replay.json::<serde_json::Value>().await.unwrap();
    assert_eq!(replay["nzo_ids"][0], first_job);

    let queue = client
        .get(format!("{}/api/queue", app.base_url))
        .bearer_auth(&access)
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap();
    assert_eq!(queue["total"], 1);

    let observation = client
        .get(format!(
            "{}/api/queue/admissions/{ADMISSION_KEY}",
            app.base_url
        ))
        .bearer_auth(&access)
        .send()
        .await
        .unwrap();
    assert_eq!(observation.status(), StatusCode::OK);
    let observation = observation.json::<serde_json::Value>().await.unwrap();
    assert_eq!(observation["admission"]["idempotency_key"], ADMISSION_KEY);
    assert_eq!(observation["admission"]["job_id"], first_job);
    assert_eq!(
        observation["admission"]["payload_digest"],
        payload_digest(&payload)
    );
    assert_eq!(observation["state"]["location"], "queue");
    assert_eq!(observation["state"]["status"], "queued");

    let conflict = upload(
        &client,
        &app.base_url,
        &access,
        ADMISSION_KEY,
        "different.nzb",
        sample_nzb_variant_bytes(),
    )
    .await;
    assert_eq!(conflict.status(), StatusCode::CONFLICT);
    let conflict = conflict.json::<serde_json::Value>().await.unwrap();
    assert_eq!(conflict["error_kind"], "admission_conflict");

    let queue = client
        .get(format!("{}/api/queue", app.base_url))
        .bearer_auth(&access)
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap();
    assert_eq!(queue["total"], 1);
}

#[tokio::test]
async fn keyed_multi_payload_upload_is_rejected_before_admission() {
    let app = start_test_server(Vec::new()).await;
    let client = reqwest::Client::new();
    let access = setup_auth(&client, &app.base_url).await;
    let form = reqwest::multipart::Form::new()
        .part(
            "first",
            reqwest::multipart::Part::bytes(sample_nzb_bytes()).file_name("first.nzb"),
        )
        .part(
            "second",
            reqwest::multipart::Part::bytes(sample_nzb_variant_bytes()).file_name("second.nzb"),
        );

    let response = client
        .post(format!("{}/api/queue/add", app.base_url))
        .bearer_auth(&access)
        .header("Idempotency-Key", ADMISSION_KEY)
        .multipart(form)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    let missing = client
        .get(format!(
            "{}/api/queue/admissions/{ADMISSION_KEY}",
            app.base_url
        ))
        .bearer_auth(&access)
        .send()
        .await
        .unwrap();
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
}
