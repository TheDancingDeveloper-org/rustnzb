use crate::clients::rustnzb::RustnzbClient;
use crate::clients::sabnzbd::SabnzbdClient;
use crate::config::{self, Scenario, GB, MB};
use crate::metrics::{MetricSample, MetricsCollector};
use crate::{charts, datagen, docker, report};
use anyhow::Result;
use serde::Serialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, serde::Deserialize, PartialEq, Eq)]
pub enum BenchmarkOutcome {
    Succeeded,
    Failed,
    TimedOut,
    SubmissionFailed,
}

#[derive(Debug, Clone, Serialize, serde::Deserialize, Default)]
pub struct FixtureMetrics {
    /// Bytes of decoded payload served by the controlled NNTP fixture. This is
    /// authoritative for fixture traffic, unlike client speed integration.
    pub payload_bytes_served: u64,
    /// yEnc-body bytes emitted by the fixture (excluding variable NNTP
    /// headers), providing a reproducible wire-volume comparison.
    pub wire_bytes_served: u64,
    /// BODY / ARTICLE requests including repeats and deterministic misses.
    pub article_requests: u64,
    pub articles_served: u64,
    /// NNTP 430 responses injected by the fixture. A non-zero count makes
    /// the run a fault/repair path, never a healthy-path comparison.
    pub article_not_found: u64,
}

fn classify_terminal_status(status: &str) -> BenchmarkOutcome {
    if status.eq_ignore_ascii_case("completed") {
        BenchmarkOutcome::Succeeded
    } else {
        BenchmarkOutcome::Failed
    }
}

#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct ClientResult {
    pub client: String,
    pub scenario: String,
    pub scenario_description: String,
    pub test_type: String,
    pub total_bytes: u64,
    pub outcome: BenchmarkOutcome,
    pub payload_verified: bool,
    pub peak_work_dir_bytes: u64,
    pub fixture_metrics: FixtureMetrics,
    pub download_sec: f64,
    pub par2_sec: f64,
    pub unpack_sec: f64,
    pub total_sec: f64,
    pub avg_speed_mbps: f64,
    pub peak_speed_mbps: f64,
    pub cpu_avg: f64,
    pub cpu_peak: f64,
    pub mem_avg_mb: f64,
    pub mem_peak_mb: f64,
    pub net_rx_avg_mbps: f64,
    pub net_rx_peak_mbps: f64,
    pub disk_write_avg_mbps: f64,
    pub disk_write_peak_mbps: f64,
    pub iowait_avg: f64,
    pub iowait_peak: f64,
    pub timeseries: Vec<MetricSample>,
    /// Internal metrics from the client's own API (rustnzb only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub internal_metrics: Option<InternalMetrics>,
}

/// Metrics captured from rustnzb's own REST API after job completion.
#[derive(Debug, Clone, Serialize, serde::Deserialize, Default)]
pub struct InternalMetrics {
    /// Per-server download statistics.
    pub server_stats: Vec<ServerStat>,
    /// Per-stage durations reported by the client's post-processing pipeline.
    pub stage_durations: Vec<StageDuration>,
    /// Download throughput reported by the download engine (MB/s).
    pub download_throughput_mbps: f64,
    /// Total articles downloaded.
    pub articles_downloaded: u64,
    /// Total articles failed.
    pub articles_failed: u64,
}

#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct ServerStat {
    pub server_name: String,
    pub articles_downloaded: u64,
    pub articles_failed: u64,
    pub bytes_downloaded: u64,
}

#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct StageDuration {
    pub name: String,
    pub status: String,
    pub duration_secs: f64,
    pub message: Option<String>,
}

async fn wait_for_service(name: &str, url: &str, timeout_secs: u64) -> Result<()> {
    let client = reqwest::Client::new();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
    loop {
        if tokio::time::Instant::now() > deadline {
            anyhow::bail!("{name} not ready after {timeout_secs}s");
        }
        if let Ok(resp) = client
            .get(url)
            .timeout(std::time::Duration::from_secs(5))
            .send()
            .await
        {
            if resp.status().is_success() {
                tracing::info!("  {name}: ready");
                return Ok(());
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
}

async fn wait_for_sabnzbd(client: &SabnzbdClient, timeout_secs: u64) -> Result<()> {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
    loop {
        if tokio::time::Instant::now() > deadline {
            anyhow::bail!("sabnzbd not ready after {timeout_secs}s");
        }
        if client.healthy().await {
            tracing::info!("  sabnzbd: ready");
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
}

async fn trigger_mock_nntp_reload() -> Result<()> {
    let client = reqwest::Client::new();
    let resp = client
        .get("http://mock-nntp:8080/reload")
        .timeout(std::time::Duration::from_secs(30))
        .send()
        .await?;
    let body: serde_json::Value = resp.json().await?;
    tracing::info!("Mock NNTP reloaded: {body}");
    Ok(())
}

async fn clean_download_dir(docker_client: &bollard::Docker, service: &str, dir: &str) {
    if let Some(cid) = docker::get_container_id(docker_client, service).await {
        match docker::exec_in_container(
            docker_client,
            &cid,
            vec![
                "sh",
                "-c",
                &format!("rm -rf {dir}/* {dir}/.[!.]* 2>/dev/null; echo ok"),
            ],
        )
        .await
        {
            Ok(_) => tracing::info!("  [{service}] Cleaned {dir}"),
            Err(e) => tracing::warn!("  [{service}] Failed to clean {dir}: {e}"),
        }
    }
}

pub async fn run(scenario_selector: String, data_dir: PathBuf, results_dir: PathBuf) -> Result<()> {
    tracing::info!("============================================================");
    tracing::info!("  Usenet Client Benchmark: SABnzbd vs rustnzb");
    tracing::info!("============================================================");

    let docker_client = docker::connect()?;
    let mut metrics = MetricsCollector::new(docker::connect()?);

    // Wait for services
    tracing::info!("Waiting for services...");
    let rnzb = RustnzbClient::new(config::RUSTNZB_API);

    wait_for_service("mock-nntp", "http://mock-nntp:8080/health", 120).await?;
    let sab = SabnzbdClient::from_runtime_config(config::SABNZBD_API, &docker_client).await?;
    wait_for_sabnzbd(&sab, 180).await?;
    sab.configure_mock_server().await?;
    wait_for_service(
        "rustnzb",
        &format!("{}/api/status", config::RUSTNZB_API),
        120,
    )
    .await?;
    bootstrap_rustnzb_mock_server().await?;

    // Resolve container IDs for metrics and log capture
    metrics.resolve_container_id("sabnzbd").await;
    metrics.resolve_container_id("rustnzb").await;

    let sab_container_id = docker::get_container_id(&docker_client, "sabnzbd").await;
    let rnzb_container_id = docker::get_container_id(&docker_client, "rustnzb").await;

    // Resolve scenarios
    let scenarios = config::resolve_scenarios(&scenario_selector);
    if scenarios.is_empty() {
        return Ok(());
    }

    let total_data: u64 = scenarios.iter().map(|s| s.total_size).sum();
    tracing::info!(
        "Running {} scenario(s), {:.1} GB total raw data",
        scenarios.len(),
        total_data as f64 / GB as f64
    );
    for s in &scenarios {
        tracing::info!(
            "  {:25} {:>5} GB  {:>6}  timeout={}s",
            s.name,
            s.total_size / GB,
            s.test_type,
            s.timeout_secs,
        );
    }

    // Generate test data
    tracing::info!("Generating test data...");
    datagen::prepare_data(&scenarios, &data_dir).await?;

    // Reload mock NNTP index
    trigger_mock_nntp_reload().await?;

    // Clear any stale history before starting
    sab.clear_all().await;
    rnzb.clear_all().await;

    let mut all_results: Vec<(ClientResult, ClientResult)> = Vec::new();
    let mut scenario_logs: Vec<(String, String, String)> = Vec::new(); // (scenario_name, sab_logs, rnzb_logs)

    for sc in &scenarios {
        tracing::info!("============================================================");
        tracing::info!("SCENARIO: {} — {}", sc.name, sc.description);
        tracing::info!("============================================================");

        let nzb_path = datagen::nzb_path(sc, &data_dir);

        // Clean download directories
        clean_download_dir(&docker_client, "sabnzbd", "/config/Downloads").await;
        clean_download_dir(&docker_client, "rustnzb", "/downloads").await;
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;

        // Run SABnzbd
        let sab_start = chrono::Utc::now().to_rfc3339();
        let sab_result = run_client("sabnzbd", sc, &nzb_path, &sab, &rnzb, &metrics).await;
        let sab_logs = if let Some(ref cid) = sab_container_id {
            docker::get_container_logs(&docker_client, cid, &sab_start)
                .await
                .unwrap_or_default()
        } else {
            String::new()
        };
        sab.clear_all().await;
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;

        // Clean between runs
        clean_download_dir(&docker_client, "sabnzbd", "/config/Downloads").await;
        clean_download_dir(&docker_client, "rustnzb", "/downloads").await;
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;

        // Run rustnzb
        let rnzb_start = chrono::Utc::now().to_rfc3339();
        let rnzb_result = run_client("rustnzb", sc, &nzb_path, &sab, &rnzb, &metrics).await;
        let rnzb_logs = if let Some(ref cid) = rnzb_container_id {
            docker::get_container_logs(&docker_client, cid, &rnzb_start)
                .await
                .unwrap_or_default()
        } else {
            String::new()
        };
        rnzb.clear_all().await;
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;

        scenario_logs.push((sc.name.clone(), sab_logs, rnzb_logs));
        all_results.push((sab_result, rnzb_result));
    }

    // Reports
    tokio::fs::create_dir_all(&results_dir).await?;
    let timestamp = chrono::Local::now().format("%Y%m%d_%H%M%S").to_string();

    report::write_json(&all_results, &results_dir, &timestamp)?;
    report::write_csv(&all_results, &results_dir, &timestamp)?;
    let summary = report::build_summary(&all_results);
    println!("\n{summary}");
    report::write_summary(&summary, &results_dir, &timestamp)?;

    let charts_dir = results_dir.join(format!("charts_{timestamp}"));
    std::fs::create_dir_all(&charts_dir)?;
    charts::generate_all(&all_results, &charts_dir)?;

    // Write per-scenario container logs for tuning analysis
    let logs_dir = results_dir.join(format!("logs_{timestamp}"));
    std::fs::create_dir_all(&logs_dir)?;
    for (scenario_name, sab_logs, rnzb_logs) in &scenario_logs {
        if !sab_logs.is_empty() {
            let path = logs_dir.join(format!("{scenario_name}_sabnzbd.log"));
            std::fs::write(&path, sab_logs)?;
        }
        if !rnzb_logs.is_empty() {
            let path = logs_dir.join(format!("{scenario_name}_rustnzb.log"));
            std::fs::write(&path, rnzb_logs)?;
        }
    }
    tracing::info!(
        "Logs: {} ({} scenario(s))",
        logs_dir.display(),
        scenario_logs.len()
    );

    tracing::info!("Results: {}", results_dir.display());
    validate_verification_results(&all_results)?;
    Ok(())
}

/// The compact fixtures are regression gates, rather than best-effort report
/// generators. Keep their result files for diagnosis, then fail the command
/// when rustnzb cannot prove the stated healthy/fault behaviour.
fn validate_verification_results(results: &[(ClientResult, ClientResult)]) -> Result<()> {
    for (_, rustnzb) in results {
        validate_rustnzb_verification(
            &rustnzb.scenario,
            &rustnzb.outcome,
            rustnzb.payload_verified,
            rustnzb.fixture_metrics.article_not_found,
        )?;
    }
    Ok(())
}

fn validate_rustnzb_verification(
    scenario: &str,
    outcome: &BenchmarkOutcome,
    payload_verified: bool,
    article_not_found: u64,
) -> Result<()> {
    match scenario {
        "verify_32mb_unpack" if *outcome != BenchmarkOutcome::Succeeded || !payload_verified => {
            anyhow::bail!(
                "rustnzb failed verified unpack fixture: outcome={outcome:?}, payload_verified={payload_verified}"
            );
        }
        "verify_fault_32mb_par2"
            if *outcome != BenchmarkOutcome::Succeeded
                || !payload_verified
                || article_not_found == 0 =>
        {
            anyhow::bail!(
                    "rustnzb failed verified fault fixture: outcome={outcome:?}, payload_verified={payload_verified}, article_not_found={article_not_found}"
                );
        }
        _ => {}
    }
    Ok(())
}

async fn run_client(
    client_name: &str,
    sc: &Scenario,
    nzb_path: &Path,
    sab: &SabnzbdClient,
    rnzb: &RustnzbClient,
    metrics: &MetricsCollector,
) -> ClientResult {
    let mut result = ClientResult {
        client: client_name.to_string(),
        scenario: sc.name.clone(),
        scenario_description: sc.description.clone(),
        test_type: sc.test_type.to_string(),
        total_bytes: sc.total_size,
        outcome: BenchmarkOutcome::SubmissionFailed,
        payload_verified: false,
        peak_work_dir_bytes: 0,
        fixture_metrics: FixtureMetrics::default(),
        download_sec: 0.0,
        par2_sec: 0.0,
        unpack_sec: 0.0,
        total_sec: 0.0,
        avg_speed_mbps: 0.0,
        peak_speed_mbps: 0.0,
        cpu_avg: 0.0,
        cpu_peak: 0.0,
        mem_avg_mb: 0.0,
        mem_peak_mb: 0.0,
        net_rx_avg_mbps: 0.0,
        net_rx_peak_mbps: 0.0,
        disk_write_avg_mbps: 0.0,
        disk_write_peak_mbps: 0.0,
        iowait_avg: 0.0,
        iowait_peak: 0.0,
        timeseries: vec![],
        internal_metrics: None,
    };

    tracing::info!("  [{client_name}] Adding NZB...");
    let nzb_data = match tokio::fs::read(nzb_path).await {
        Ok(d) => d,
        Err(e) => {
            tracing::error!("  [{client_name}] Failed to read NZB: {e}");
            return result;
        }
    };
    let nzb_filename = nzb_path.file_name().unwrap().to_string_lossy().to_string();

    if let Err(error) = reset_fixture_stats().await {
        tracing::warn!("  [{client_name}] Failed to reset fixture counters: {error}");
    }

    let add_result = if client_name == "sabnzbd" {
        sab.add_nzb(&nzb_data, &nzb_filename).await
    } else {
        rnzb.add_nzb(&nzb_data, &nzb_filename).await
    };
    if let Err(e) = add_result {
        tracing::error!("  [{client_name}] Failed to add NZB: {e}");
        return result;
    }
    result.outcome = BenchmarkOutcome::TimedOut;

    let stats_handle = metrics.start_collecting(client_name);
    let start = tokio::time::Instant::now();
    let mut peak_speed: f64 = 0.0;
    let mut speeds = Vec::new();
    let deadline = start + std::time::Duration::from_secs(sc.timeout_secs);

    tracing::info!(
        "  [{client_name}] Downloading (timeout {}s)...",
        sc.timeout_secs
    );

    loop {
        if tokio::time::Instant::now() > deadline {
            tracing::warn!("  [{client_name}] TIMEOUT");
            break;
        }

        let (terminal_status, progress, speed) = if client_name == "sabnzbd" {
            let terminal = sab.terminal_status().await.unwrap_or(None);
            let prog = sab.progress_fraction().await;
            let spd = sab.download_speed().await;
            (terminal, prog, spd)
        } else {
            let terminal = rnzb.terminal_status().await.unwrap_or(None);
            let prog = rnzb.progress_fraction().await;
            let spd = rnzb.download_speed().await;
            (terminal, prog, spd)
        };

        if speed > 0.0 {
            peak_speed = peak_speed.max(speed);
            speeds.push(speed);
        }

        if let Some(status) = terminal_status {
            result.outcome = classify_terminal_status(&status);
            tracing::info!("  [{client_name}] Terminal status: {status}");
            break;
        }

        result.peak_work_dir_bytes = result
            .peak_work_dir_bytes
            .max(sample_work_dir_bytes(metrics, client_name).await);

        let bar_len = 30;
        let filled = (bar_len as f64 * progress) as usize;
        let bar: String = "#".repeat(filled) + &"-".repeat(bar_len - filled);
        let speed_mb = speed / MB as f64;
        eprint!(
            "\r  [{client_name}] [{bar}] {:5.1}% @ {:.1} MB/s",
            progress * 100.0,
            speed_mb
        );

        tokio::time::sleep(std::time::Duration::from_millis(config::POLL_INTERVAL_MS)).await;
    }
    eprintln!();

    result.total_sec = start.elapsed().as_secs_f64();
    // Use actual throughput (bytes/time) for avg speed — self-reported speeds aren't comparable
    if result.total_sec > 0.0 {
        result.avg_speed_mbps = sc.total_size as f64 * 8.0 / result.total_sec / 1_000_000.0;
    }
    result.peak_speed_mbps = peak_speed * 8.0 / 1_000_000.0;
    result.peak_work_dir_bytes = result
        .peak_work_dir_bytes
        .max(sample_work_dir_bytes(metrics, client_name).await);
    result.fixture_metrics = fixture_metrics().await.unwrap_or_else(|error| {
        tracing::warn!("  [{client_name}] Failed to read fixture counters: {error}");
        FixtureMetrics::default()
    });
    result.payload_verified = result.outcome == BenchmarkOutcome::Succeeded
        && verify_payload(metrics, client_name, sc)
            .await
            .unwrap_or(false);
    if result.outcome == BenchmarkOutcome::Succeeded && !result.payload_verified {
        tracing::error!("  [{client_name}] Completed without a verified payload");
        result.outcome = BenchmarkOutcome::Failed;
    }

    // Stage timing from history
    let stage_result = if client_name == "sabnzbd" {
        sab.get_stage_timing().await
    } else {
        rnzb.get_stage_timing().await
    };
    if let Ok(stages) = stage_result {
        result.par2_sec = stages.par2_sec;
        result.unpack_sec = stages.unpack_sec;
        // Derive download time from harness-measured total minus post-processing
        // stages.  Client-reported download_time is integer-second granularity
        // (SABnzbd API limitation), while total_sec has full precision.
        result.download_sec = (result.total_sec - stages.par2_sec - stages.unpack_sec).max(0.0);
    }
    if result.download_sec == 0.0 {
        result.download_sec = result.total_sec;
    }

    // Internal metrics (rustnzb only)
    if client_name == "rustnzb" {
        match rnzb.get_internal_metrics().await {
            Ok(metrics) => {
                tracing::info!(
                    "  [{client_name}] Internal: {} server(s), {} stage(s), {:.1} MB/s download throughput",
                    metrics.server_stats.len(),
                    metrics.stage_durations.len(),
                    metrics.download_throughput_mbps,
                );
                result.internal_metrics = Some(metrics);
            }
            Err(e) => {
                tracing::warn!("  [{client_name}] Failed to fetch internal metrics: {e}");
            }
        }
    }

    // Docker stats
    let samples = if let Some(handle) = stats_handle {
        handle.stop().await
    } else {
        vec![]
    };

    if !samples.is_empty() {
        let cpus: Vec<f64> = samples.iter().map(|s| s.cpu_pct).collect();
        let mems: Vec<u64> = samples.iter().map(|s| s.mem_bytes).collect();
        let rxs: Vec<f64> = samples.iter().map(|s| s.net_rx_bps).collect();
        let dws: Vec<f64> = samples.iter().map(|s| s.disk_write_bps).collect();
        let iow: Vec<f64> = samples.iter().map(|s| s.iowait_pct).collect();

        let avg = |v: &[f64]| v.iter().sum::<f64>() / v.len().max(1) as f64;
        let max_f = |v: &[f64]| v.iter().cloned().fold(0.0f64, f64::max);
        let avg_u = |v: &[u64]| v.iter().sum::<u64>() as f64 / v.len().max(1) as f64;
        let max_u = |v: &[u64]| v.iter().cloned().max().unwrap_or(0);

        result.cpu_avg = avg(&cpus);
        result.cpu_peak = max_f(&cpus);
        result.mem_avg_mb = avg_u(&mems) / MB as f64;
        result.mem_peak_mb = max_u(&mems) as f64 / MB as f64;
        result.net_rx_avg_mbps = avg(&rxs) * 8.0 / 1e6;
        result.net_rx_peak_mbps = max_f(&rxs) * 8.0 / 1e6;
        result.disk_write_avg_mbps = avg(&dws) / MB as f64;
        result.disk_write_peak_mbps = max_f(&dws) / MB as f64;
        result.iowait_avg = avg(&iow);
        result.iowait_peak = max_f(&iow);
        result.timeseries = samples;
    }

    tracing::info!(
        "  [{client_name}] Done: {:.1}s total, {:.1} Mbps avg",
        result.total_sec,
        result.avg_speed_mbps
    );
    result
}

async fn reset_fixture_stats() -> Result<()> {
    reqwest::Client::new()
        .post("http://mock-nntp:8080/reset-stats")
        .send()
        .await?
        .error_for_status()?;
    Ok(())
}

/// Provision the generated NNTP fixture through the public rustnzb API after
/// the service is ready. This avoids relying on image/bootstrap timing for a
/// bind-mounted TOML file and verifies the workload has a real server before
/// timing any job.
async fn bootstrap_rustnzb_mock_server() -> Result<()> {
    let http = reqwest::Client::new();
    let endpoint = format!("{}/api/config/servers", config::RUSTNZB_API);
    let existing: Vec<serde_json::Value> = http
        .get(&endpoint)
        .timeout(std::time::Duration::from_secs(15))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    if !existing
        .iter()
        .any(|server| server["id"].as_str() == Some("benchmark-mock"))
    {
        let server = serde_json::json!({
            "id": "benchmark-mock",
            "name": "Benchmark mock NNTP",
            "host": "mock-nntp",
            "port": 119,
            "ssl": false,
            "ssl_verify": false,
            "username": "bench",
            "password": "bench",
            "connections": 20,
            "priority": 0,
            "enabled": true,
            "retention": 0,
            "pipelining": 1,
            "optional": false,
            "compress": false,
            "ramp_up_delay_ms": 0,
            "recv_buffer_size": 0,
            "proxy_url": null,
            "trusted_fingerprint": null,
            "connect_timeout_secs": 30,
        });
        http.post(&endpoint)
            .json(&server)
            .timeout(std::time::Duration::from_secs(15))
            .send()
            .await?
            .error_for_status()?;
    }

    let configured: Vec<serde_json::Value> = http
        .get(&endpoint)
        .timeout(std::time::Duration::from_secs(15))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    if !configured.iter().any(|server| {
        server["id"].as_str() == Some("benchmark-mock")
            && server["host"].as_str() == Some("mock-nntp")
            && server["enabled"].as_bool() == Some(true)
    }) {
        anyhow::bail!("rustnzb benchmark mock NNTP server was not configured");
    }
    tracing::info!("rustnzb mock NNTP server configured");
    Ok(())
}

async fn fixture_metrics() -> Result<FixtureMetrics> {
    Ok(reqwest::Client::new()
        .get("http://mock-nntp:8080/status")
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?)
}

async fn sample_work_dir_bytes(metrics: &MetricsCollector, client_name: &str) -> u64 {
    let Some(container_id) = metrics.container_id(client_name) else {
        return 0;
    };
    let docker = match docker::connect() {
        Ok(docker) => docker,
        Err(_) => return 0,
    };
    let work_dir = if client_name == "sabnzbd" {
        "/config/Downloads"
    } else {
        "/downloads"
    };
    let command = format!(
        "du -sb {work_dir}/incomplete {work_dir}/complete 2>/dev/null | awk '{{total += $1}} END {{print total + 0}}'"
    );
    docker::exec_in_container(&docker, container_id, vec!["sh", "-c", &command])
        .await
        .ok()
        .and_then(|output| output.trim().parse().ok())
        .unwrap_or(0)
}

async fn verify_payload(
    metrics: &MetricsCollector,
    client_name: &str,
    scenario: &Scenario,
) -> Result<bool> {
    if !requires_payload_verification(scenario.test_type) {
        return Ok(true);
    }
    let Some(container_id) = metrics.container_id(client_name) else {
        return Ok(false);
    };
    let docker = docker::connect()?;
    let expected_name = format!("bench_{}.bin", config::size_label(scenario.total_size));
    let complete_dir = if client_name == "sabnzbd" {
        "/config/Downloads/complete"
    } else {
        "/downloads/complete"
    };
    let output_command = format!(
        "set -eu; file=$(find {complete_dir} -type f \\( -name '{expected_name}' -o -name '*.bin' \\) -print -quit); test -n \"$file\"; sha256sum \"$file\" | awk '{{print $1}}'"
    );
    let output_digest =
        docker::exec_in_container(&docker, container_id, vec!["sh", "-c", &output_command]).await?;
    let Some(mock_container) = docker::get_container_id(&docker, "mock-nntp").await else {
        return Ok(false);
    };
    let source_command = format!("sha256sum /data/testdata/{expected_name} | awk '{{print $1}}'");
    let source_digest =
        docker::exec_in_container(&docker, &mock_container, vec!["sh", "-c", &source_command])
            .await?;
    Ok(output_digest.trim() == source_digest.trim() && !source_digest.trim().is_empty())
}

const fn requires_payload_verification(test_type: config::TestType) -> bool {
    matches!(test_type, config::TestType::Par2 | config::TestType::Unpack)
}

#[cfg(test)]
mod tests {
    use super::{
        classify_terminal_status, requires_payload_verification, validate_rustnzb_verification,
        BenchmarkOutcome, FixtureMetrics,
    };

    #[test]
    fn completed_is_the_only_successful_terminal_status() {
        assert_eq!(
            classify_terminal_status("completed"),
            BenchmarkOutcome::Succeeded
        );
        assert_eq!(
            classify_terminal_status("Completed"),
            BenchmarkOutcome::Succeeded
        );
    }

    #[test]
    fn failure_is_never_reported_as_a_successful_benchmark() {
        for status in ["failed", "timeout", "", "repair failed"] {
            assert_eq!(
                classify_terminal_status(status),
                BenchmarkOutcome::Failed,
                "{status} must remain a failed benchmark"
            );
        }
    }

    #[test]
    fn fixture_fault_metrics_are_machine_readable() {
        let metrics: FixtureMetrics = serde_json::from_str(
            r#"{"payload_bytes_served":33554432,"wire_bytes_served":33900000,"article_requests":47,"articles_served":45,"article_not_found":2}"#,
        )
        .unwrap();
        assert_eq!(metrics.payload_bytes_served, 33_554_432);
        assert_eq!(metrics.wire_bytes_served, 33_900_000);
        assert_eq!(metrics.article_requests, 47);
        assert_eq!(metrics.articles_served, 45);
        assert_eq!(metrics.article_not_found, 2);
    }

    #[test]
    fn verified_unpack_fixture_requires_success_and_matching_payload() {
        assert!(validate_rustnzb_verification(
            "verify_32mb_unpack",
            &BenchmarkOutcome::Succeeded,
            true,
            0,
        )
        .is_ok());
        assert!(validate_rustnzb_verification(
            "verify_32mb_unpack",
            &BenchmarkOutcome::Succeeded,
            false,
            0,
        )
        .is_err());
    }

    #[test]
    fn fault_fixture_requires_observable_injected_fault() {
        assert!(validate_rustnzb_verification(
            "verify_fault_32mb_par2",
            &BenchmarkOutcome::Succeeded,
            true,
            1,
        )
        .is_ok());
        assert!(validate_rustnzb_verification(
            "verify_fault_32mb_par2",
            &BenchmarkOutcome::Succeeded,
            true,
            0,
        )
        .is_err());
        assert!(validate_rustnzb_verification(
            "verify_fault_32mb_par2",
            &BenchmarkOutcome::Failed,
            true,
            1,
        )
        .is_err());
        assert!(validate_rustnzb_verification(
            "verify_fault_32mb_par2",
            &BenchmarkOutcome::Succeeded,
            false,
            1,
        )
        .is_err());
    }

    #[test]
    fn raw_scenarios_are_not_payload_verified_but_par2_and_unpack_are() {
        assert!(!requires_payload_verification(crate::config::TestType::Raw));
        assert!(requires_payload_verification(crate::config::TestType::Par2));
        assert!(requires_payload_verification(
            crate::config::TestType::Unpack
        ));
    }
}
