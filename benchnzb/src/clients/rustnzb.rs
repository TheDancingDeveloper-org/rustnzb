use super::StageTiming;
use anyhow::Result;

pub struct StatusSummary {
    pub speed_bps: u64,
    pub queue_size: usize,
    pub active_downloads: usize,
}

pub struct RustnzbClient {
    url: String,
    http: reqwest::Client,
}

impl RustnzbClient {
    pub fn new(url: &str) -> Self {
        Self {
            url: url.trim_end_matches('/').to_string(),
            http: reqwest::Client::new(),
        }
    }

    pub async fn add_nzb(&self, data: &[u8], filename: &str) -> Result<()> {
        let part = reqwest::multipart::Part::bytes(data.to_vec())
            .file_name(filename.to_string())
            .mime_str("application/x-nzb")?;
        let form = reqwest::multipart::Form::new().part("nzbfile", part);

        let resp = self
            .http
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
            .http
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
            .http
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
            .http
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
            .http
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
            .http
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
            .http
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
            .http
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
            .http
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
            http: self.http.clone(),
        }
    }

    /// Get queue size (number of jobs in queue).
    pub async fn queue_size(&self) -> Result<usize> {
        let queue: serde_json::Value = self
            .http
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
            .http
            .get(format!("{}/api/status", self.url))
            .timeout(std::time::Duration::from_secs(10))
            .send()
            .await?
            .json()
            .await?;

        let queue: serde_json::Value = self
            .http
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
            .http
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
