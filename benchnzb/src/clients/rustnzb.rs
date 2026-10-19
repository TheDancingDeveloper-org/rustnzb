use super::StageTiming;
use anyhow::Result;

pub struct StatusSummary {
    pub speed_bps: u64,
    pub queue_size: usize,
    pub active_downloads: usize,
}

pub struct RustnzbClient {
    url: String,
    access_token: Option<String>,
    http: reqwest::Client,
}

impl RustnzbClient {
    pub fn new(url: &str) -> Self {
        Self {
            url: url.trim_end_matches('/').to_string(),
            access_token: None,
            http: reqwest::Client::new(),
        }
    }

    pub fn get(&self, url: String) -> reqwest::RequestBuilder {
        self.authorize(self.http.get(url))
    }

    pub fn post(&self, url: String) -> reqwest::RequestBuilder {
        self.authorize(self.http.post(url))
    }

    pub fn put(&self, url: String) -> reqwest::RequestBuilder {
        self.authorize(self.http.put(url))
    }

    fn authorize(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.access_token {
            Some(token) => request.bearer_auth(token),
            None => request,
        }
    }

    /// Create an isolated first-boot account and retain its short-lived token.
    /// Benchmark state is disposable, so no credential or API key is stored in
    /// the repository or passed through the benchmark configuration.
    pub async fn initialize_auth(&mut self) -> Result<()> {
        let suffix = rand::random::<u128>();
        let response: serde_json::Value = self
            .http
            .post(format!("{}/api/auth/setup", self.url))
            .json(&serde_json::json!({
                "username": format!("benchmark-{suffix:x}"),
                "password": format!("benchmark-{suffix:032x}"),
            }))
            .timeout(std::time::Duration::from_secs(15))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        self.access_token = response["access_token"].as_str().map(str::to_string);
        if self.access_token.is_none() {
            anyhow::bail!("rustnzb auth setup did not return an access token");
        }
        Ok(())
    }

    pub async fn add_nzb(&self, data: &[u8], filename: &str) -> Result<()> {
        let part = reqwest::multipart::Part::bytes(data.to_vec())
            .file_name(filename.to_string())
            .mime_str("application/x-nzb")?;
        let form = reqwest::multipart::Form::new().part("nzbfile", part);

        let resp = self
            .post(format!("{}/api/queue/add", self.url))
            .multipart(form)
            .timeout(std::time::Duration::from_secs(30))
            .send()
            .await?;
        resp.error_for_status_ref()?;
        let body: serde_json::Value = resp.json().await?;
        tracing::debug!("rustnzb add response: {body}");
        Ok(())
    }

    pub async fn all_finished(&self) -> Result<bool> {
        // Queue empty = download phase done, but check history for post-processing
        let queue: serde_json::Value = self
            .get(format!("{}/api/queue", self.url))
            .timeout(std::time::Duration::from_secs(10))
            .send()
            .await?
            .json()
            .await?;

        let jobs = queue["jobs"].as_array().cloned().unwrap_or_default();
        if !jobs.is_empty() {
            // Check if all jobs are "completed" status
            return Ok(jobs.iter().all(|j| {
                j["status"]
                    .as_str()
                    .is_some_and(|s| s == "completed" || s == "failed")
            }));
        }

        // Check history
        let history: serde_json::Value = self
            .get(format!("{}/api/history", self.url))
            .timeout(std::time::Duration::from_secs(10))
            .send()
            .await?
            .json()
            .await?;

        let entries = history["entries"].as_array().cloned().unwrap_or_default();
        if entries.is_empty() {
            return Ok(false);
        }
        Ok(entries.iter().all(|e| {
            e["status"]
                .as_str()
                .is_some_and(|s| s == "completed" || s == "failed")
        }))
    }

    /// Return the terminal state of the current run, if the queue has drained.
    /// A failed history entry is terminal but must never be reported as a
    /// successful benchmark result.
    pub async fn terminal_status(&self) -> Result<Option<String>> {
        let queue: serde_json::Value = self
            .get(format!("{}/api/queue", self.url))
            .timeout(std::time::Duration::from_secs(10))
            .send()
            .await?
            .json()
            .await?;
        if queue["jobs"]
            .as_array()
            .is_some_and(|jobs| !jobs.is_empty())
        {
            return Ok(None);
        }

        let history: serde_json::Value = self
            .get(format!("{}/api/history", self.url))
            .timeout(std::time::Duration::from_secs(10))
            .send()
            .await?
            .json()
            .await?;
        Ok(history["entries"]
            .as_array()
            .and_then(|entries| entries.first())
            .and_then(|entry| entry["status"].as_str())
            .map(str::to_string))
    }

    pub async fn progress_fraction(&self) -> f64 {
        let queue: serde_json::Value = match self
            .get(format!("{}/api/queue", self.url))
            .timeout(std::time::Duration::from_secs(10))
            .send()
            .await
        {
            Ok(r) => r.json().await.unwrap_or_default(),
            Err(_) => serde_json::Value::default(),
        };

        let jobs = queue["jobs"].as_array().cloned().unwrap_or_default();
        if jobs.is_empty() {
            if self.all_finished().await.unwrap_or(false) {
                return 1.0;
            }
            return 0.0;
        }

        let mut total_progress = 0.0;
        let mut count = 0;
        for job in &jobs {
            let total = job["total_bytes"].as_u64().unwrap_or(1) as f64;
            let downloaded = job["downloaded_bytes"].as_u64().unwrap_or(0) as f64;
            total_progress += downloaded / total;
            count += 1;
        }
        if count > 0 {
            total_progress / count as f64
        } else {
            0.0
        }
    }

    pub async fn download_speed(&self) -> f64 {
        let queue: serde_json::Value = match self
            .get(format!("{}/api/queue", self.url))
            .timeout(std::time::Duration::from_secs(10))
            .send()
            .await
        {
            Ok(r) => r.json().await.unwrap_or_default(),
            Err(_) => serde_json::Value::default(),
        };

        queue["speed_bps"]
            .as_u64()
            .map(|v| v as f64)
            .or_else(|| queue["speed_bps"].as_f64())
            .unwrap_or(0.0)
    }

    pub async fn get_stage_timing(&self) -> Result<StageTiming> {
        let history: serde_json::Value = self
            .get(format!("{}/api/history", self.url))
            .timeout(std::time::Duration::from_secs(10))
            .send()
            .await?
            .json()
            .await?;

        let mut timing = StageTiming::default();

        let entries = history["entries"].as_array().cloned().unwrap_or_default();
        for entry in &entries {
            // rustnzb provides stages with duration_secs
            if let Some(stages) = entry["stages"].as_array() {
                for stage in stages {
                    let name = stage["name"].as_str().unwrap_or("");
                    let duration = stage["duration_secs"].as_f64().unwrap_or(0.0);
                    if name == "Verify" {
                        if let Some((source, skipped)) =
                            stage["message"].as_str().and_then(parse_verify_tag)
                        {
                            timing.record_verify(source, skipped);
                        }
                    }
                    if name == "Repair" {
                        if let Some((path, read, secs)) =
                            stage["message"].as_str().and_then(parse_repair_tag)
                        {
                            timing.record_repair(path, read, secs);
                        }
                    }
                    match name {
                        "Verify" | "Repair" => timing.par2_sec += duration,
                        "Extract" => timing.unpack_sec += duration,
                        _ => {}
                    }
                }
            }

            // Download time from added_at -> first stage start or completed_at
            if let (Some(added), Some(completed)) =
                (entry["added_at"].as_str(), entry["completed_at"].as_str())
            {
                if let (Ok(a), Ok(c)) = (
                    chrono::DateTime::parse_from_rfc3339(added),
                    chrono::DateTime::parse_from_rfc3339(completed),
                ) {
                    let total = (c - a).num_milliseconds() as f64 / 1000.0;
                    timing.download_sec = total - timing.par2_sec - timing.unpack_sec;
                    if timing.download_sec < 0.0 {
                        timing.download_sec = 0.0;
                    }
                }
            }
        }

        Ok(timing)
    }

    /// Fetch internal metrics from the history API after job completion.
    pub async fn get_internal_metrics(&self) -> Result<crate::runner::InternalMetrics> {
        let history: serde_json::Value = self
            .get(format!("{}/api/history", self.url))
            .timeout(std::time::Duration::from_secs(10))
            .send()
            .await?
            .json()
            .await?;

        let mut metrics = crate::runner::InternalMetrics::default();

        let entries = history["entries"].as_array().cloned().unwrap_or_default();
        for entry in &entries {
            // Server stats
            if let Some(stats) = entry["server_stats"].as_array() {
                for ss in stats {
                    metrics.server_stats.push(crate::runner::ServerStat {
                        server_name: ss["server_name"].as_str().unwrap_or("").to_string(),
                        articles_downloaded: ss["articles_downloaded"].as_u64().unwrap_or(0),
                        articles_failed: ss["articles_failed"].as_u64().unwrap_or(0),
                        bytes_downloaded: ss["bytes_downloaded"].as_u64().unwrap_or(0),
                    });
                }
            }

            // Stage durations
            if let Some(stages) = entry["stages"].as_array() {
                for stage in stages {
                    metrics.stage_durations.push(crate::runner::StageDuration {
                        name: stage["name"].as_str().unwrap_or("").to_string(),
                        status: stage["status"].as_str().unwrap_or("").to_string(),
                        duration_secs: stage["duration_secs"].as_f64().unwrap_or(0.0),
                        message: stage["message"].as_str().map(|s| s.to_string()),
                    });
                }
            }

            // Article totals from job data
            let downloaded = entry["downloaded_bytes"].as_u64().unwrap_or(0);
            let total_bytes = entry["total_bytes"].as_u64().unwrap_or(0);
            metrics.articles_downloaded += downloaded;

            // Calculate download throughput from timestamps
            if let (Some(added), Some(completed)) =
                (entry["added_at"].as_str(), entry["completed_at"].as_str())
            {
                if let (Ok(a), Ok(c)) = (
                    chrono::DateTime::parse_from_rfc3339(added),
                    chrono::DateTime::parse_from_rfc3339(completed),
                ) {
                    // Use stage timing to isolate download phase
                    let pp_time: f64 = metrics
                        .stage_durations
                        .iter()
                        .map(|s| s.duration_secs)
                        .sum();
                    let total_secs = (c - a).num_milliseconds() as f64 / 1000.0;
                    let dl_secs = (total_secs - pp_time).max(0.1);
                    metrics.download_throughput_mbps =
                        total_bytes as f64 / dl_secs / (1024.0 * 1024.0);
                }
            }
        }

        Ok(metrics)
    }

    /// Clone client for use in spawned tasks.
    pub fn clone_client(&self) -> Self {
        Self {
            url: self.url.clone(),
            access_token: self.access_token.clone(),
            http: self.http.clone(),
        }
    }

    /// Get queue size (number of jobs in queue).
    pub async fn queue_size(&self) -> Result<usize> {
        let queue: serde_json::Value = self
            .get(format!("{}/api/queue", self.url))
            .timeout(std::time::Duration::from_secs(10))
            .send()
            .await?
            .json()
            .await?;

        Ok(queue["jobs"].as_array().map(|a| a.len()).unwrap_or(0))
    }

    /// Get status summary from the API.
    pub async fn get_status(&self) -> Result<StatusSummary> {
        let status: serde_json::Value = self
            .get(format!("{}/api/status", self.url))
            .timeout(std::time::Duration::from_secs(10))
            .send()
            .await?
            .json()
            .await?;

        let queue: serde_json::Value = self
            .get(format!("{}/api/queue", self.url))
            .timeout(std::time::Duration::from_secs(10))
            .send()
            .await?
            .json()
            .await?;

        let jobs = queue["jobs"].as_array().cloned().unwrap_or_default();
        let active = jobs
            .iter()
            .filter(|j| j["status"].as_str() == Some("downloading"))
            .count();

        Ok(StatusSummary {
            speed_bps: status["speed_bps"].as_u64().unwrap_or(0),
            queue_size: jobs.len(),
            active_downloads: active,
        })
    }

    /// Get count of history entries.
    pub async fn history_count(&self) -> Result<u64> {
        let history: serde_json::Value = self
            .get(format!("{}/api/history", self.url))
            .timeout(std::time::Duration::from_secs(10))
            .send()
            .await?
            .json()
            .await?;

        Ok(history["entries"]
            .as_array()
            .map(|a| a.len() as u64)
            .unwrap_or(0))
    }

    /// Clear history only (not queue).
    pub async fn clear_history(&self) -> Result<()> {
        self.http
            .delete(format!("{}/api/history", self.url))
            .send()
            .await?;
        Ok(())
    }

    pub async fn clear_all(&self) {
        // Delete queue
        let queue: serde_json::Value = match self
            .http
            .get(format!("{}/api/queue", self.url))
            .send()
            .await
        {
            Ok(r) => r.json().await.unwrap_or_default(),
            Err(_) => serde_json::Value::default(),
        };

        if let Some(jobs) = queue["jobs"].as_array() {
            for job in jobs {
                if let Some(id) = job["id"].as_str() {
                    let _ = self
                        .http
                        .delete(format!("{}/api/queue/{id}", self.url))
                        .send()
                        .await;
                }
            }
        }

        // Clear history
        let _ = self
            .http
            .delete(format!("{}/api/history", self.url))
            .send()
            .await;
    }
}

/// Parse the `[verify: <source>, <n> bytes not re-read, ...]` suffix rustnzb
/// appends to a Verify stage message (WI-147).
pub fn parse_verify_tag(message: &str) -> Option<(&str, u64)> {
    let tag = message.rsplit_once("[verify: ")?.1;
    let (source, rest) = tag.split_once(", ")?;
    let skipped = rest.split_once(" bytes not re-read")?.0.parse().ok()?;
    Some((source, skipped))
}

/// Parse the `[repair: <path>, <n> slices read, <secs>s...]` suffix rustnzb
/// appends to a Repair stage message (WI-148).
pub fn parse_repair_tag(message: &str) -> Option<(&str, u64, f64)> {
    let tag = message.rsplit_once("[repair: ")?.1;
    let (path, rest) = tag.split_once(", ")?;
    let (read, rest) = rest.split_once(" slices read, ")?;
    let secs = rest.split_once('s')?.0.parse().ok()?;
    Some((path, read.parse().ok()?, secs))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_repair_tag() {
        let msg = "Recovery set 1/1: All files repaired from the in-flight fold (3 blocks) [repair: fold, 0 slices read, 0.012s]";
        assert_eq!(parse_repair_tag(msg), Some(("fold", 0, 0.012)));
        let msg = "Recovery set 1/1: All files repaired [repair: disk, 6820 slices read, 4.500s, fold refused: decode matrix is singular]";
        assert_eq!(parse_repair_tag(msg), Some(("disk", 6820, 4.5)));
        assert_eq!(parse_repair_tag("Recovery set 1/1: done"), None);
    }

    #[test]
    fn any_disk_repair_marks_the_run_disk() {
        let mut timing = StageTiming::default();
        timing.record_repair("fold", 0, 0.5);
        timing.record_repair("disk", 10, 1.0);
        timing.record_repair("fold", 2, 0.25);
        assert_eq!(timing.repair_path.as_deref(), Some("disk"));
        assert_eq!(timing.repair_slices_read, 12);
        assert!((timing.repair_sec - 1.75).abs() < 1e-9);
    }

    #[test]
    fn parses_the_verify_tag() {
        let msg = "Recovery set 1/1: all 2 files correct [verify: in-flight, 5368709120 bytes not re-read, 0.004s]";
        assert_eq!(parse_verify_tag(msg), Some(("in-flight", 5_368_709_120)));
        assert_eq!(
            parse_verify_tag("Recovery set 1/1: all 2 files correct"),
            None
        );
    }

    #[test]
    fn least_trusting_source_wins() {
        let mut timing = StageTiming::default();
        timing.record_verify("in-flight", 10);
        timing.record_verify("disk", 0);
        timing.record_verify("partial", 5);
        assert_eq!(timing.verify_source.as_deref(), Some("disk"));
        assert_eq!(timing.verify_bytes_skipped, 15);
    }
}
