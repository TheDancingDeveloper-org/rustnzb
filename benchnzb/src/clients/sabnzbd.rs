use super::{StageTiming, StatusSummary};
use crate::docker;
use anyhow::Result;

pub struct SabnzbdClient {
    url: String,
    http: reqwest::Client,
    api_key: String,
}

impl SabnzbdClient {
    pub fn new(url: &str, api_key: String) -> Self {
        Self {
            url: url.trim_end_matches('/').to_string(),
            http: reqwest::Client::new(),
            api_key,
        }
    }

    /// LinuxServer's image may create a runtime API key during first boot.
    /// Read it via the benchmark's Docker control channel; callers must never
    /// log this value.
    pub async fn from_runtime_config(url: &str, docker_client: &bollard::Docker) -> Result<Self> {
        let container_id = docker::get_container_id(docker_client, "sabnzbd")
            .await
            .ok_or_else(|| anyhow::anyhow!("SABnzbd container is not running"))?;
        let output = docker::exec_in_container(
            docker_client,
            &container_id,
            vec!["sh", "-c", "sed -n 's/^api_key = //p' /config/sabnzbd.ini"],
        )
        .await?;
        let api_key = output.trim();
        if !is_valid_api_key(api_key) {
            anyhow::bail!("SABnzbd did not expose a valid runtime API key");
        }
        Ok(Self::new(url, api_key.to_owned()))
    }

    async fn api_get(&self, params: &[(&str, &str)]) -> Result<serde_json::Value> {
        let mut all_params = vec![("apikey", self.api_key.as_str()), ("output", "json")];
        all_params.extend_from_slice(params);
        let resp = self
            .http
            .get(format!("{}/api", self.url))
            // SABnzbd's default host allow-list accepts localhost but not the
            // Docker service name used to reach it from the orchestrator.
            .header(reqwest::header::HOST, "localhost")
            .query(&all_params)
            .timeout(std::time::Duration::from_secs(30))
            .send()
            .await?;
        Ok(resp.json().await?)
    }

    pub async fn healthy(&self) -> bool {
        self.api_get(&[("mode", "version")]).await.is_ok()
    }

    pub async fn configure_mock_server(&self) -> Result<()> {
        let response = self
            .api_get(&[
                ("mode", "set_config"),
                ("section", "servers"),
                ("name", "benchmark-mock"),
                ("host", "mock-nntp"),
                ("port", "119"),
                ("username", "bench"),
                ("password", "bench"),
                ("connections", "20"),
                ("enable", "1"),
                ("ssl", "0"),
                ("required", "0"),
            ])
            .await?;
        if response["status"].as_bool() == Some(false) {
            anyhow::bail!("SABnzbd rejected benchmark NNTP configuration");
        }
        Ok(())
    }

    pub async fn add_nzb(&self, data: &[u8], filename: &str) -> Result<()> {
        let part = reqwest::multipart::Part::bytes(data.to_vec())
            .file_name(filename.to_string())
            .mime_str("application/x-nzb")?;
        let form = reqwest::multipart::Form::new()
            .text("mode", "addfile")
            .text("apikey", self.api_key.clone())
            .text("output", "json")
            .part("nzbfile", part);

        let resp = self
            .http
            .post(format!("{}/api", self.url))
            .header(reqwest::header::HOST, "localhost")
            .multipart(form)
            .timeout(std::time::Duration::from_secs(30))
            .send()
            .await?;
        let body: serde_json::Value = resp.json().await?;
        tracing::debug!("SABnzbd addfile response: {body}");
        Ok(())
    }

    pub async fn all_finished(&self) -> Result<bool> {
        // Queue must be empty
        let queue = self.api_get(&[("mode", "queue")]).await?;
        if let Some(slots) = queue["queue"]["slots"].as_array() {
            if !slots.is_empty() {
                return Ok(false);
            }
        }

        // History must have completed entries
        let history = self.api_get(&[("mode", "history")]).await?;
        let slots = history["history"]["slots"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        if slots.is_empty() {
            return Ok(false);
        }
        Ok(slots.iter().all(|s| {
            s["status"]
                .as_str()
                .is_some_and(|st| st == "Completed" || st == "Failed")
        }))
    }

    /// Return the terminal state of the current run, if the queue has drained.
    pub async fn terminal_status(&self) -> Result<Option<String>> {
        let queue = self.api_get(&[("mode", "queue")]).await?;
        if queue["queue"]["slots"]
            .as_array()
            .is_some_and(|slots| !slots.is_empty())
        {
            return Ok(None);
        }
        let history = self.api_get(&[("mode", "history"), ("limit", "1")]).await?;
        Ok(history["history"]["slots"]
            .as_array()
            .and_then(|slots| slots.first())
            .and_then(|slot| slot["status"].as_str())
            .map(str::to_string))
    }

    pub async fn progress_fraction(&self) -> f64 {
        let queue = self.api_get(&[("mode", "queue")]).await.unwrap_or_default();
        if let Some(slots) = queue["queue"]["slots"].as_array() {
            if slots.is_empty() {
                if self.all_finished().await.unwrap_or(false) {
                    return 1.0;
                }
                return 0.0;
            }
            let total: f64 = slots
                .iter()
                .filter_map(|s| s["percentage"].as_str().and_then(|p| p.parse::<f64>().ok()))
                .sum();
            return total / 100.0 / slots.len() as f64;
        }
        0.0
    }

    pub async fn download_speed(&self) -> f64 {
        let queue = self.api_get(&[("mode", "queue")]).await.unwrap_or_default();
        queue["queue"]["kbpersec"]
            .as_str()
            .and_then(|s| s.parse::<f64>().ok())
            .map(|kbps| kbps * 1024.0)
            .unwrap_or(0.0)
    }

    pub async fn get_stage_timing(&self) -> Result<StageTiming> {
        let history = self.api_get(&[("mode", "history"), ("limit", "1")]).await?;
        let mut timing = StageTiming::default();

        if let Some(slots) = history["history"]["slots"].as_array() {
            // Only read the most recent entry to avoid stale history contamination
            if let Some(slot) = slots.first() {
                timing.download_sec = slot["download_time"]
                    .as_f64()
                    .or_else(|| slot["download_time"].as_u64().map(|v| v as f64))
                    .unwrap_or(0.0);
                let pp = slot["postproc_time"]
                    .as_f64()
                    .or_else(|| slot["postproc_time"].as_u64().map(|v| v as f64))
                    .unwrap_or(0.0);
                if let Some(stages) = slot["stage_log"].as_array() {
                    for stage in stages {
                        let name = stage["name"].as_str().unwrap_or("");
                        if let Some(actions) = stage["actions"].as_array() {
                            for action in actions {
                                let text = action.as_str().unwrap_or("");
                                if name.contains("Repair") || name.contains("Verif") {
                                    if let Some(secs) = parse_duration(text) {
                                        timing.par2_sec += secs;
                                    }
                                }
                                if name.contains("Unpack") {
                                    if let Some(secs) = parse_duration(text) {
                                        timing.unpack_sec += secs;
                                    }
                                }
                            }
                        }
                    }
                }
                // Fallback: if we couldn't parse stages, assign all pp time to par2
                if timing.par2_sec == 0.0 && timing.unpack_sec == 0.0 && pp > 0.0 {
                    timing.par2_sec = pp;
                }
            }
        }
        Ok(timing)
    }

    pub async fn clear_all(&self) {
        let _ = self
            .api_get(&[("mode", "queue"), ("name", "delete"), ("value", "all")])
            .await;
        let _ = self
            .api_get(&[("mode", "history"), ("name", "delete"), ("value", "all")])
            .await;
    }

    /// Get queue size (number of active/queued slots).
    pub async fn queue_size(&self) -> Result<usize> {
        let queue = self.api_get(&[("mode", "queue")]).await?;
        Ok(queue["queue"]["slots"]
            .as_array()
            .map(|a| a.len())
            .unwrap_or(0))
    }

    /// Get status summary compatible with stress test metrics.
    pub async fn get_status(&self) -> Result<StatusSummary> {
        let queue = self.api_get(&[("mode", "queue")]).await?;
        let speed_bps = queue["queue"]["kbpersec"]
            .as_str()
            .and_then(|s| s.parse::<f64>().ok())
            .map(|kbps| (kbps * 1024.0) as u64)
            .unwrap_or(0);
        let slots = queue["queue"]["slots"].as_array();
        let queue_size = slots.map(|a| a.len()).unwrap_or(0);
        let active_downloads = slots
            .map(|a| {
                a.iter()
                    .filter(|s| s["status"].as_str() == Some("Downloading"))
                    .count()
            })
            .unwrap_or(0);

        Ok(StatusSummary {
            speed_bps,
            queue_size,
            active_downloads,
        })
    }

    /// Get count of history entries.
    pub async fn history_count(&self) -> Result<u64> {
        let history = self.api_get(&[("mode", "history")]).await?;
        Ok(history["history"]["slots"]
            .as_array()
            .map(|a| a.len() as u64)
            .unwrap_or(0))
    }

    /// Clear history only (not queue).
    pub async fn clear_history(&self) -> Result<()> {
        let _ = self
            .api_get(&[("mode", "history"), ("name", "delete"), ("value", "all")])
            .await;
        Ok(())
    }

    /// Clone client for use in spawned tasks.
    pub fn clone_client(&self) -> Self {
        Self {
            url: self.url.clone(),
            http: self.http.clone(),
            api_key: self.api_key.clone(),
        }
    }
}

fn parse_duration(text: &str) -> Option<f64> {
    let text = text.to_lowercase();
    let mut total: f64 = 0.0;
    if let Some(idx) = text.find("min") {
        if let Some(num_str) = text[..idx].split_whitespace().last() {
            if let Ok(m) = num_str.parse::<f64>() {
                total += m * 60.0;
            }
        }
    }
    if let Some(idx) = text.find("sec") {
        if let Some(num_str) = text[..idx].split_whitespace().last() {
            if let Ok(s) = num_str.parse::<f64>() {
                total += s;
            }
        }
    }
    if total > 0.0 {
        Some(total)
    } else {
        None
    }
}

fn is_valid_api_key(value: &str) -> bool {
    value.len() == 32 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::is_valid_api_key;

    #[test]
    fn accepts_a_sabnzbd_runtime_api_key() {
        assert!(is_valid_api_key("0123456789abcdef0123456789abcdef"));
    }

    #[test]
    fn rejects_missing_or_malformed_runtime_api_keys() {
        for value in ["", "not-a-key", "0123456789abcdef0123456789abcdeg"] {
            assert!(!is_valid_api_key(value), "{value} must not be accepted");
        }
    }
}
