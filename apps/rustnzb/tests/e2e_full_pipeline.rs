//! End-to-end integration test: HTTP server -> upload NZB -> verify in queue.
//!
//! This test starts the HTTP server on a random port, uploads fixture NZBs
//! via the native API, and verifies they appear in the queue.
//! It also exercises the SABnzbd compatibility API endpoints.

#![allow(clippy::uninlined_format_args)]

mod support;

use support::{sample_nzb_bytes, sample_nzb_variant_bytes, start_test_server};

#[tokio::test]
async fn test_upload_nzb_and_verify_queue() {
    let app = start_test_server(Vec::new()).await;
    let client = reqwest::Client::new();
    let base_url = &app.base_url;
    let setup = client
        .post(format!("{}/api/auth/setup", base_url))
        .json(&serde_json::json!({
            "username": "pipeline-test",
            "password": "pipeline-test-password"
        }))
        .send()
        .await
        .expect("auth setup failed");
    assert_eq!(setup.status(), 200);
    let access = setup.json::<serde_json::Value>().await.unwrap()["access_token"]
        .as_str()
        .expect("auth setup should return an access token")
        .to_string();

    // 1. Verify server is up: GET /api/status
    let resp = client
        .get(format!("{}/api/status", base_url))
        .bearer_auth(&access)
        .send()
        .await
        .expect("Failed to reach server");
    assert_eq!(resp.status(), 200);
    let status: serde_json::Value = resp.json().await.expect("Bad JSON");
    assert!(status["version"].is_string());
    eprintln!("Server version: {}", status["version"]);

    // 2. Verify queue is initially empty
    let resp = client
        .get(format!("{}/api/queue", base_url))
        .bearer_auth(&access)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let queue: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(queue["jobs"].as_array().unwrap().len(), 0);
    eprintln!("Queue is empty (expected)");

    // 2b. Pause the queue so jobs stay in queue (no servers configured = immediate fail otherwise)
    let resp = client
        .get(format!("{}/sabnzbd/api?mode=pause", base_url))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    eprintln!("Queue paused for testing");

    // 3. Upload NZB file via native API
    let nzb_bytes = sample_nzb_bytes();
    let nzb_filename = "sample.nzb".to_string();

    let part = reqwest::multipart::Part::bytes(nzb_bytes.clone())
        .file_name(nzb_filename.clone())
        .mime_str("application/x-nzb")
        .unwrap();
    let form = reqwest::multipart::Form::new().part("file", part);

    let resp = client
        .post(format!(
            "{}/api/queue/add?category=test&priority=1",
            base_url
        ))
        .bearer_auth(&access)
        .multipart(form)
        .send()
        .await
        .expect("Upload failed");
    assert_eq!(resp.status(), 200);
    let add_result: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(add_result["status"], true);
    assert!(!add_result["nzo_ids"].as_array().unwrap().is_empty());
    let nzo_id = add_result["nzo_ids"][0].as_str().unwrap().to_string();
    eprintln!("Uploaded NZB, got ID: {}", nzo_id);

    // 4. Verify job appears in queue
    let resp = client
        .get(format!("{}/api/queue", base_url))
        .bearer_auth(&access)
        .send()
        .await
        .unwrap();
    let queue: serde_json::Value = resp.json().await.unwrap();
    let jobs = queue["jobs"].as_array().unwrap();
    assert_eq!(jobs.len(), 1, "Expected 1 job in queue, got {}", jobs.len());
    let job = &jobs[0];
    assert_eq!(job["category"], "test");
    assert!(job["total_bytes"].as_u64().unwrap() > 0);
    assert!(job["article_count"].as_u64().unwrap() > 0);
    assert!(job["file_count"].as_u64().unwrap() > 0);
    eprintln!(
        "Queue job: {} ({} files, {} articles, {} bytes)",
        job["name"], job["file_count"], job["article_count"], job["total_bytes"]
    );

    // 5. Test SABnzbd compatibility API -- version
    let resp = client
        .get(format!("{}/sabnzbd/api?mode=version", base_url))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let ver: serde_json::Value = resp.json().await.unwrap();
    assert!(ver["version"].is_string());
    eprintln!("SABnzbd version: {}", ver["version"]);

    // 6. Test SABnzbd API -- fullstatus
    let resp = client
        .get(format!("{}/sabnzbd/api?mode=fullstatus", base_url))
        .send()
        .await
        .unwrap();
    let fullstatus: serde_json::Value = resp.json().await.unwrap();
    // paused should be a boolean
    assert!(fullstatus["status"]["paused"].is_boolean());
    eprintln!(
        "SABnzbd fullstatus paused: {}",
        fullstatus["status"]["paused"]
    );

    // 7. Test SABnzbd API -- get_config
    let resp = client
        .get(format!("{}/sabnzbd/api?mode=get_config", base_url))
        .send()
        .await
        .unwrap();
    let config: serde_json::Value = resp.json().await.unwrap();
    assert!(config["config"]["misc"]["complete_dir"].is_string());
    assert!(config["config"]["categories"].is_array());
    eprintln!(
        "SABnzbd config complete_dir: {}",
        config["config"]["misc"]["complete_dir"]
    );

    // 8. Test SABnzbd API -- queue
    let resp = client
        .get(format!("{}/sabnzbd/api?mode=queue", base_url))
        .send()
        .await
        .unwrap();
    let sab_queue: serde_json::Value = resp.json().await.unwrap();
    let sab_slots = sab_queue["queue"]["slots"].as_array().unwrap();
    assert_eq!(sab_slots.len(), 1);
    assert!(
        sab_slots[0]["nzo_id"]
            .as_str()
            .unwrap()
            .starts_with("SABnzbd_nzo_")
    );
    assert!(sab_slots[0]["filename"].is_string());
    assert!(sab_slots[0]["percentage"].is_string());
    eprintln!(
        "SABnzbd queue slot: {} ({}%)",
        sab_slots[0]["filename"], sab_slots[0]["percentage"]
    );

    // 9. Test SABnzbd API -- history (should be empty)
    let resp = client
        .get(format!("{}/sabnzbd/api?mode=history", base_url))
        .send()
        .await
        .unwrap();
    let sab_history: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(sab_history["history"]["slots"].as_array().unwrap().len(), 0);

    // 10. Test SABnzbd addfile via multipart POST.
    // Use a DIFFERENT NZB — nzb-web ≥0.4.6 rejects duplicate content-hashes,
    // and the first NZB is already in the queue from step 3.
    let alt_nzb_bytes = sample_nzb_variant_bytes();
    let alt_nzb_filename = "sample-alt.nzb".to_string();
    let part2 = reqwest::multipart::Part::bytes(alt_nzb_bytes)
        .file_name(alt_nzb_filename)
        .mime_str("application/x-nzb")
        .unwrap();
    let form2 = reqwest::multipart::Form::new()
        .text("mode", "addfile")
        .text("cat", "movies")
        .text("priority", "2")
        .part("nzbfile", part2);

    let resp = client
        .post(format!("{}/sabnzbd/api", base_url))
        .multipart(form2)
        .send()
        .await
        .expect("SABnzbd addfile failed");
    assert_eq!(resp.status(), 200);
    let sab_add: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(sab_add["status"], true);
    assert!(!sab_add["nzo_ids"].as_array().unwrap().is_empty());
    let sab_nzo = sab_add["nzo_ids"][0].as_str().unwrap();
    assert!(
        sab_nzo.starts_with("SABnzbd_nzo_"),
        "Expected SABnzbd_nzo_ prefix, got: {}",
        sab_nzo
    );
    eprintln!("SABnzbd addfile: {}", sab_nzo);

    // 11. Verify now 2 jobs in queue
    let resp = client
        .get(format!("{}/api/queue", base_url))
        .bearer_auth(&access)
        .send()
        .await
        .unwrap();
    let queue: serde_json::Value = resp.json().await.unwrap();
    let job_count = queue["jobs"].as_array().unwrap().len();
    assert_eq!(job_count, 2, "Expected 2 jobs, got {}", job_count);
    eprintln!("Queue now has {} jobs", job_count);

    // 12. Test SABnzbd pause mode (already paused, but test the endpoint)
    let resp = client
        .get(format!("{}/sabnzbd/api?mode=pause", base_url))
        .send()
        .await
        .unwrap();
    let pause_result: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(pause_result["status"], true);
    eprintln!("SABnzbd pause: success");

    // 13. Test SABnzbd resume mode, then immediately re-pause to keep jobs in queue
    let resp = client
        .get(format!("{}/sabnzbd/api?mode=resume", base_url))
        .send()
        .await
        .unwrap();
    let resume_result: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(resume_result["status"], true);
    eprintln!("SABnzbd resume: success");

    // Re-pause immediately to prevent jobs from being downloaded and moved to history
    let resp = client
        .get(format!("{}/sabnzbd/api?mode=pause", base_url))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.json::<serde_json::Value>().await.unwrap()["status"],
        true
    );
    eprintln!("SABnzbd re-pause: success");

    // 14. Test native delete
    let first_job_id = queue["jobs"][0]["id"].as_str().unwrap();
    let resp = client
        .delete(format!("{}/api/queue/{}", base_url, first_job_id))
        .bearer_auth(&access)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    eprintln!("Deleted job: {}", first_job_id);

    // 15. Verify the remaining queue (may have 1 job or 0 depending on timing)
    let resp = client
        .get(format!("{}/api/queue", base_url))
        .bearer_auth(&access)
        .send()
        .await
        .unwrap();
    let queue: serde_json::Value = resp.json().await.unwrap();
    let remaining = queue["jobs"].as_array().unwrap().len();
    eprintln!("Queue has {} job(s) remaining", remaining);
    // The remaining job may have been moved to history if download engine ran
    assert!(remaining <= 1, "Expected at most 1 job, got {}", remaining);

    // 16. Test static file serving (index.html)
    let resp = client.get(format!("{}/", base_url)).send().await.unwrap();
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    assert!(
        body.contains("rustnzb"),
        "Index page should contain 'rustnzb'"
    );
    // build.rs embeds a placeholder page when npm is unavailable; that must
    // not pass for the real Angular frontend.
    assert!(
        !body.contains("Frontend not built") && body.contains("<app-root"),
        "Index page is the build.rs placeholder, not the Angular frontend; \
         build apps/rustnzb/frontend before running this test"
    );
    eprintln!("Static file serving: OK");

    eprintln!("\nAll integration tests passed!");
}
