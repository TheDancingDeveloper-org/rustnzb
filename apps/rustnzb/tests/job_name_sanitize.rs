//! Job names derived from uploaded NZB filenames are sanitized into a single
//! portable directory name instead of being rejected or kept verbatim.

mod support;

use reqwest::StatusCode;
use support::{sample_nzb_bytes, start_test_server};

async fn setup_auth(client: &reqwest::Client, base_url: &str) -> String {
    let setup = client
        .post(format!("{base_url}/api/auth/setup"))
        .json(&serde_json::json!({
            "username": "job-name-test",
            "password": "job-name-test-password"
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
    file_name: &str,
) -> reqwest::Response {
    let part = reqwest::multipart::Part::bytes(sample_nzb_bytes())
        .file_name(file_name.to_string())
        .mime_str("application/x-nzb")
        .unwrap();
    client
        .post(format!("{base_url}/api/queue/add"))
        .bearer_auth(access)
        .multipart(reqwest::multipart::Form::new().part("file", part))
        .send()
        .await
        .unwrap()
}

#[tokio::test]
async fn uploaded_filenames_become_portable_job_names() {
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

    let long_name = format!("{}.nzb", "x".repeat(300));
    let cases: [(&str, &str); 7] = [
        ("a:b*c?d.nzb", "a-b_c_d"),
        ("Star Trek: Discovery.nzb", "Star Trek - Discovery"),
        ("..nzb", "unnamed"),
        ("Trailing.Dot..nzb", "Trailing.Dot"),
        (" lead and trail space .nzb", "lead and trail space"),
        ("CON.nzb", "_CON"),
        (long_name.as_str(), ""),
    ];

    for (file_name, expected) in cases {
        let response = upload(&client, &app.base_url, &access, file_name).await;
        let status = response.status();
        let body = response.json::<serde_json::Value>().await.unwrap();
        assert_eq!(status, StatusCode::OK, "{file_name:?}: {body}");
        let job_id = body["nzo_ids"][0].as_str().expect("job id").to_string();

        let queue = client
            .get(format!("{}/api/queue", app.base_url))
            .bearer_auth(&access)
            .send()
            .await
            .unwrap()
            .json::<serde_json::Value>()
            .await
            .unwrap();
        let job = queue["jobs"]
            .as_array()
            .expect("jobs array")
            .iter()
            .find(|job| job["id"] == job_id.as_str())
            .unwrap_or_else(|| panic!("{file_name:?} not queued: {queue}"));
        let name = job["name"].as_str().expect("job name");
        let output_dir = job["output_dir"].as_str().expect("output dir");
        if expected.is_empty() {
            assert_eq!(name, "x".repeat(240), "{file_name:?}");
        } else {
            assert_eq!(name, expected, "{file_name:?}");
        }
        assert!(
            output_dir.ends_with(&format!("/Default/{name}")),
            "{file_name:?}: {output_dir}"
        );
    }
}
