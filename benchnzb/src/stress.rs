use crate::clients::StressClient;
use crate::config::{self, ARTICLE_SIZE};
use crate::metrics::MetricsCollector;
use crate::nzb;
use crate::{docker, stress_charts, stress_report};
use anyhow::Result;
use serde::Serialize;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

const RUSTNZB_API: &str = "http://rustnzb:9090";
const SABNZBD_API: &str = "http://sabnzbd:8080";
const SYNTH_NNTP_HEALTH: &str = "http://synth-nntp:8080/health";

pub struct StressConfig {
    pub client: String,
    pub duration: String,
    pub nzb_size: String,
    pub concurrency: usize,
    pub poll_interval_secs: u64,
    pub cleanup_interval_secs: u64,
    pub results_dir: PathBuf,
}

#[derive(Debug, Clone, Serialize)]
pub struct StressResult {
    pub config_summary: ConfigSummary,
    pub start_time: String,
    pub end_time: String,
    pub duration_secs: f64,
    pub total_nzbs_submitted: u64,
    pub total_nzbs_completed: u64,
    pub total_bytes_downloaded: u64,
    pub overall_avg_speed_mbps: f64,
    pub timeseries: Vec<StressSample>,
    pub windows: Vec<WindowStats>,
    pub degradation: DegradationAnalysis,
}

#[derive(Debug, Clone, Serialize)]
pub struct ConfigSummary {
    pub client: String,
    pub duration: String,
    pub nzb_size: String,
    pub nzb_size_bytes: u64,
    pub concurrency: usize,
    pub poll_interval_secs: u64,
    pub cleanup_interval_secs: u64,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct StressSample {
    pub ts: f64,
    pub elapsed_secs: f64,
    pub speed_bps: u64,
    pub cpu_pct: f64,
    pub mem_bytes: u64,
    pub net_rx_bps: f64,
    pub disk_write_bps: f64,
    pub queue_size: usize,
    pub active_downloads: usize,
    pub nzbs_completed_total: u64,
    pub bytes_downloaded_total: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct WindowStats {
    pub window_start_secs: f64,
    pub window_end_secs: f64,
    pub avg_speed_mbps: f64,
    pub avg_cpu_pct: f64,
    pub avg_mem_mb: f64,
    pub peak_mem_mb: f64,
    pub nzbs_completed: u64,
    pub bytes_downloaded: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct DegradationAnalysis {
    pub speed_trend_pct_per_hour: f64,
    pub memory_trend_mb_per_hour: f64,
    pub speed_first_window_mbps: f64,
    pub speed_last_window_mbps: f64,
    pub memory_first_window_mb: f64,
    pub memory_last_window_mb: f64,
    pub degradation_detected: bool,
    pub notes: Vec<String>,
}

/// Shared state between the orchestrator tasks.
struct SharedState {
    nzbs_submitted: AtomicU64,
    nzbs_completed: AtomicU64,
    bytes_downloaded: AtomicU64,
    stop: AtomicBool,
    samples: Mutex<Vec<StressSample>>,
    start_ts: f64,
}

impl SharedState {
    fn new() -> Self {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs_f64();
        Self {
            nzbs_submitted: AtomicU64::new(0),
            nzbs_completed: AtomicU64::new(0),
            bytes_downloaded: AtomicU64::new(0),
            stop: AtomicBool::new(false),
            samples: Mutex::new(Vec::new()),
            start_ts: now,
        }
    }
}

pub async fn run(cfg: StressConfig) -> Result<()> {
    let client_name = cfg.client.to_lowercase();
    let is_sabnzbd = client_name == "sabnzbd";
    let target_label = if is_sabnzbd { "SABnzbd" } else { "RustNZB" };
    let container_name = if is_sabnzbd { "sabnzbd" } else { "rustnzb" };

    tracing::info!("============================================================");
    tracing::info!("  benchnzb v2: Stress Test ({target_label})");
    tracing::info!("============================================================");

    let duration = config::parse_duration(&cfg.duration)?;
    let nzb_size = config::parse_size(&cfg.nzb_size)?;

    tracing::info!("  Client:      {target_label}");
    tracing::info!("  Duration:    {}", config::format_duration(duration));
    tracing::info!("  NZB size:    {}", config::format_size(nzb_size));
    tracing::info!("  Concurrency: {} NZBs queued", cfg.concurrency);
    tracing::info!("  Poll:        {}s", cfg.poll_interval_secs);
    tracing::info!("  Cleanup:     {}s", cfg.cleanup_interval_secs);

    let articles_per_nzb = nzb_size.div_ceil(ARTICLE_SIZE) as u32;
    tracing::info!(
        "  Articles/NZB: {} ({} bytes each)",
        articles_per_nzb,
        ARTICLE_SIZE
    );

    // Wait for services
    tracing::info!("Waiting for services...");
    wait_for_service("synth-nntp", SYNTH_NNTP_HEALTH, 120).await?;

    let client: StressClient = if is_sabnzbd {
        let docker_client = docker::connect()?;
        let sab = crate::clients::sabnzbd::SabnzbdClient::from_runtime_config(
            SABNZBD_API,
            &docker_client,
        )
        .await?;
        wait_for_sabnzbd(&sab, 120).await?;
        StressClient::Sabnzbd(sab)
    } else {
        wait_for_service("rustnzb", &format!("{RUSTNZB_API}/api/status"), 120).await?;
        StressClient::Rustnzb(crate::clients::rustnzb::RustnzbClient::new(RUSTNZB_API))
    };

    // Clear any stale state
    client.clear_all().await;

    // Set up Docker metrics collector
    let docker_client = docker::connect()?;
    let mut metrics_collector = MetricsCollector::new(docker::connect()?);
    metrics_collector.resolve_container_id(container_name).await;

    let target_container_id = docker::get_container_id(&docker_client, container_name).await;

    // Start Docker stats collection
    let stats_handle = metrics_collector.start_collecting(container_name);

    let state = Arc::new(SharedState::new());
    let config_summary = ConfigSummary {
        client: target_label.to_string(),
        duration: cfg.duration.clone(),
        nzb_size: cfg.nzb_size.clone(),
        nzb_size_bytes: nzb_size,
        concurrency: cfg.concurrency,
        poll_interval_secs: cfg.poll_interval_secs,
        cleanup_interval_secs: cfg.cleanup_interval_secs,
    };

    let start_instant = tokio::time::Instant::now();
    let start_time = chrono::Utc::now().to_rfc3339();

    tracing::info!("============================================================");
    tracing::info!("  STRESS TEST STARTED");
    tracing::info!("============================================================");

    // Spawn feeder task
    let feeder_state = state.clone();
    let feeder_handle = tokio::spawn(feeder_loop(
        feeder_state,
        client.clone_client(),
        nzb_size,
        cfg.concurrency,
    ));

    // Spawn metrics task
    let metrics_state = state.clone();
    let metrics_handle = tokio::spawn(metrics_loop(
        metrics_state,
        client.clone_client(),
        cfg.poll_interval_secs,
    ));

    // Spawn cleanup task
    let cleanup_state = state.clone();
    let cleanup_docker = docker::connect()?;
    let cleanup_container = container_name.to_string();
    let cleanup_handle = tokio::spawn(cleanup_loop(
        cleanup_state,
        client.clone_client(),
        cleanup_docker,
        cfg.cleanup_interval_secs,
        cleanup_container,
        is_sabnzbd,
    ));

    // Main loop — run for the specified duration
    let deadline = start_instant + duration;
    let mut last_status = tokio::time::Instant::now();

    loop {
        if tokio::time::Instant::now() >= deadline {
            tracing::info!("Duration reached, stopping...");
            break;
        }

        // Print status every 30 seconds
        if last_status.elapsed() >= std::time::Duration::from_secs(30) {
            let elapsed = start_instant.elapsed().as_secs_f64();
            let submitted = state.nzbs_submitted.load(Ordering::Relaxed);
            let completed = state.nzbs_completed.load(Ordering::Relaxed);
            let bytes = state.bytes_downloaded.load(Ordering::Relaxed);

            let speed = if elapsed > 0.0 {
                bytes as f64 / elapsed / (1024.0 * 1024.0)
            } else {
                0.0
            };

            let remaining = (deadline - tokio::time::Instant::now()).as_secs();
            tracing::info!(
                "[{} elapsed, {} remaining] NZBs: {submitted} submitted / {completed} completed | {:.1} MB/s avg | {:.1} GB total",
                config::format_duration(std::time::Duration::from_secs(elapsed as u64)),
                config::format_duration(std::time::Duration::from_secs(remaining)),
                speed,
                bytes as f64 / config::GB as f64,
            );
            last_status = tokio::time::Instant::now();
        }

        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }

    // Signal all tasks to stop
    state.stop.store(true, Ordering::Relaxed);

    // Wait for tasks to finish (with timeout)
    let _ = tokio::time::timeout(std::time::Duration::from_secs(30), async {
        let _ = feeder_handle.await;
        let _ = metrics_handle.await;
        let _ = cleanup_handle.await;
    })
    .await;

    let end_time = chrono::Utc::now().to_rfc3339();
    let total_elapsed = start_instant.elapsed().as_secs_f64();

    // Stop Docker stats collection
    let docker_samples = if let Some(handle) = stats_handle {
        handle.stop().await
    } else {
        vec![]
    };

    // Get container logs
    let target_logs = if let Some(ref cid) = target_container_id {
        docker::get_container_logs(&docker_client, cid, &start_time)
            .await
            .unwrap_or_default()
    } else {
        String::new()
    };

    // Build results
    let total_nzbs_submitted = state.nzbs_submitted.load(Ordering::Relaxed);
    let total_nzbs_completed = state.nzbs_completed.load(Ordering::Relaxed);
    let total_bytes = state.bytes_downloaded.load(Ordering::Relaxed);

    let samples = state.samples.lock().unwrap().clone();

    // Merge Docker stats into stress samples
    let merged_samples = merge_docker_stats(&samples, &docker_samples, state.start_ts);

    let windows = compute_windows(&merged_samples, config::STRESS_WINDOW_SECS);
    let degradation = analyze_degradation(&windows);

    let overall_avg_speed = if total_elapsed > 0.0 {
        total_bytes as f64 * 8.0 / total_elapsed / 1_000_000.0
    } else {
        0.0
    };

    let result = StressResult {
        config_summary,
        start_time,
        end_time,
        duration_secs: total_elapsed,
        total_nzbs_submitted,
        total_nzbs_completed,
        total_bytes_downloaded: total_bytes,
        overall_avg_speed_mbps: overall_avg_speed,
        timeseries: merged_samples,
        windows,
        degradation,
    };

    // Write reports
    tokio::fs::create_dir_all(&cfg.results_dir).await?;
    let timestamp = chrono::Local::now().format("%Y%m%d_%H%M%S").to_string();

    stress_report::write_json(&result, &cfg.results_dir, &timestamp)?;
    stress_report::write_csv(&result, &cfg.results_dir, &timestamp)?;
    let summary = stress_report::build_summary(&result);
    println!("\n{summary}");
    stress_report::write_summary(&summary, &cfg.results_dir, &timestamp)?;

    let charts_dir = cfg.results_dir.join(format!("stress_charts_{timestamp}"));
    std::fs::create_dir_all(&charts_dir)?;
    stress_charts::generate_all(&result, &charts_dir)?;

    // Write logs
    if !target_logs.is_empty() {
        let log_path = cfg
            .results_dir
            .join(format!("stress_{container_name}_{timestamp}.log"));
        std::fs::write(&log_path, &target_logs)?;
        tracing::info!("Logs: {}", log_path.display());
    }

    tracing::info!("Results: {}", cfg.results_dir.display());
    Ok(())
}

/// Generate an NZB in memory with synthetic message-IDs.
fn generate_stress_nzb(nzb_size: u64, seq: u64) -> Vec<u8> {
    let msg_prefix = format!("stress-{nzb_size}-f{seq:06}");
    let segments = nzb::build_segments(&msg_prefix, nzb_size);
    let filename = format!("stress_f{seq:06}.bin");

    let files = vec![nzb::NzbFile { filename, segments }];

    nzb::generate_nzb(&files, "bench@benchnzb").into_bytes()
}

/// Continuously submit NZBs to keep the queue at the target depth.
async fn feeder_loop(
    state: Arc<SharedState>,
    client: StressClient,
    nzb_size: u64,
    target_depth: usize,
) {
    let mut seq: u64 = 0;

    loop {
        if state.stop.load(Ordering::Relaxed) {
            break;
        }

        // Check current queue depth
        let queue_size = match client.queue_size().await {
            Ok(n) => n,
            Err(e) => {
                tracing::warn!("Failed to check queue: {e}");
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                continue;
            }
        };

        if queue_size < target_depth {
            let to_add = target_depth - queue_size;
            for _ in 0..to_add {
                seq += 1;
                let nzb_data = generate_stress_nzb(nzb_size, seq);
                let filename = format!("stress_{seq:06}.nzb");

                match client.add_nzb(&nzb_data, &filename).await {
                    Ok(()) => {
                        state.nzbs_submitted.fetch_add(1, Ordering::Relaxed);
                        tracing::debug!("Submitted NZB #{seq}");
                    }
                    Err(e) => {
                        tracing::warn!("Failed to submit NZB #{seq}: {e}");
                    }
                }
            }
        }

        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }

    tracing::debug!("Feeder loop stopped");
}

/// Collect metrics from the target client API at regular intervals.
async fn metrics_loop(state: Arc<SharedState>, client: StressClient, interval_secs: u64) {
    let interval = std::time::Duration::from_secs(interval_secs);
    let mut last_history_count: u64 = 0;

    loop {
        if state.stop.load(Ordering::Relaxed) {
            break;
        }

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs_f64();
        let elapsed = now - state.start_ts;

        // Get client status
        let (speed_bps, queue_size, active) = match client.get_status().await {
            Ok(status) => (status.speed_bps, status.queue_size, status.active_downloads),
            Err(_) => (0, 0, 0),
        };

        // Track completed NZBs via history count.
        // History is periodically cleared by the cleanup loop, so we accumulate
        // the delta since the last poll rather than using a high-water-mark.
        if let Ok(history) = client.history_count().await {
            if history > last_history_count {
                let delta = history - last_history_count;
                state.nzbs_completed.fetch_add(delta, Ordering::Relaxed);
            }
            last_history_count = history;
        }

        // Update bytes based on speed integration (rough but continuous)
        if speed_bps > 0 {
            let delta_bytes = speed_bps * interval_secs;
            state
                .bytes_downloaded
                .fetch_add(delta_bytes, Ordering::Relaxed);
        }

        let sample = StressSample {
            ts: now,
            elapsed_secs: elapsed,
            speed_bps,
            cpu_pct: 0.0,        // Filled from Docker stats merge
            mem_bytes: 0,        // Filled from Docker stats merge
            net_rx_bps: 0.0,     // Filled from Docker stats merge
            disk_write_bps: 0.0, // Filled from Docker stats merge
            queue_size,
            active_downloads: active,
            nzbs_completed_total: state.nzbs_completed.load(Ordering::Relaxed),
            bytes_downloaded_total: state.bytes_downloaded.load(Ordering::Relaxed),
        };

        if let Ok(mut vec) = state.samples.lock() {
            vec.push(sample);
        }

        tokio::time::sleep(interval).await;
    }

    tracing::debug!("Metrics loop stopped");
}

/// Periodically clean up completed downloads to prevent disk exhaustion.
async fn cleanup_loop(
    state: Arc<SharedState>,
    client: StressClient,
    docker: bollard::Docker,
    interval_secs: u64,
    container_name: String,
    is_sabnzbd: bool,
) {
    let interval = std::time::Duration::from_secs(interval_secs);

    // Wait a bit before first cleanup
    tokio::time::sleep(std::time::Duration::from_secs(10)).await;

    loop {
        if state.stop.load(Ordering::Relaxed) {
            break;
        }

        // Clean completed downloads from disk to prevent volume exhaustion.
        // For RustNZB we also clean /downloads/incomplete since rustnzb
        // recreates working dirs on demand. For SABnzbd we must NOT touch
        // /downloads/incomplete — it stores in-progress article data and
        // __ADMIN__ metadata there; deleting it mid-download stalls jobs.
        let clean_cmd = if is_sabnzbd {
            "rm -rf /downloads/complete/* /downloads/complete/.[!.]* 2>/dev/null; echo ok"
        } else {
            "rm -rf /config/Downloads/complete/* /config/Downloads/complete/.[!.]* /config/Downloads/incomplete/* /config/Downloads/incomplete/.[!.]* 2>/dev/null; echo ok"
        };
        if let Some(cid) = docker::get_container_id(&docker, &container_name).await {
            match docker::exec_in_container(&docker, &cid, vec!["sh", "-c", clean_cmd]).await {
                Ok(_) => tracing::debug!("Cleaned completed downloads"),
                Err(e) => tracing::warn!("Cleanup failed: {e}"),
            }
        }

        // Clear history to prevent unbounded growth
        let _ = client.clear_history().await;

        tokio::time::sleep(interval).await;
    }

    tracing::debug!("Cleanup loop stopped");
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
            if resp.status().as_u16() < 500 {
                tracing::info!("  {name}: ready");
                return Ok(());
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
}

async fn wait_for_sabnzbd(
    client: &crate::clients::sabnzbd::SabnzbdClient,
    timeout_secs: u64,
) -> Result<()> {
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

/// Merge Docker container stats into the stress samples by matching timestamps.
fn merge_docker_stats(
    stress_samples: &[StressSample],
    docker_samples: &[crate::metrics::MetricSample],
    _start_ts: f64,
) -> Vec<StressSample> {
    if docker_samples.is_empty() {
        return stress_samples.to_vec();
    }

    let mut merged = stress_samples.to_vec();

    for sample in &mut merged {
        // Find closest Docker sample by timestamp
        let closest = docker_samples.iter().min_by(|a, b| {
            let da = (a.ts - sample.ts).abs();
            let db = (b.ts - sample.ts).abs();
            da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)
        });

        if let Some(ds) = closest {
            // Only merge if within 10 seconds
            if (ds.ts - sample.ts).abs() < 10.0 {
                sample.cpu_pct = ds.cpu_pct;
                sample.mem_bytes = ds.mem_bytes;
                sample.net_rx_bps = ds.net_rx_bps;
                sample.disk_write_bps = ds.disk_write_bps;
            }
        }
    }

    merged
}

/// Compute windowed statistics over the timeseries.
fn compute_windows(samples: &[StressSample], window_secs: u64) -> Vec<WindowStats> {
    if samples.is_empty() {
        return vec![];
    }

    let total_elapsed = samples.last().unwrap().elapsed_secs;
    let mut windows = Vec::new();
    let mut window_start = 0.0;

    while window_start < total_elapsed {
        let window_end = window_start + window_secs as f64;

        let in_window: Vec<&StressSample> = samples
            .iter()
            .filter(|s| s.elapsed_secs >= window_start && s.elapsed_secs < window_end)
            .collect();

        if !in_window.is_empty() {
            let n = in_window.len() as f64;

            let avg_speed_mbps =
                in_window.iter().map(|s| s.speed_bps as f64).sum::<f64>() / n * 8.0 / 1_000_000.0;
            let avg_cpu = in_window.iter().map(|s| s.cpu_pct).sum::<f64>() / n;
            let avg_mem_mb =
                in_window.iter().map(|s| s.mem_bytes as f64).sum::<f64>() / n / 1_048_576.0;
            let peak_mem_mb =
                in_window.iter().map(|s| s.mem_bytes).max().unwrap_or(0) as f64 / 1_048_576.0;

            let first_completed = in_window.first().unwrap().nzbs_completed_total;
            let last_completed = in_window.last().unwrap().nzbs_completed_total;
            let first_bytes = in_window.first().unwrap().bytes_downloaded_total;
            let last_bytes = in_window.last().unwrap().bytes_downloaded_total;

            windows.push(WindowStats {
                window_start_secs: window_start,
                window_end_secs: window_end,
                avg_speed_mbps,
                avg_cpu_pct: avg_cpu,
                avg_mem_mb,
                peak_mem_mb,
                nzbs_completed: last_completed - first_completed,
                bytes_downloaded: last_bytes - first_bytes,
            });
        }

        window_start = window_end;
    }

    windows
}

/// Analyze windowed data for performance degradation.
fn analyze_degradation(windows: &[WindowStats]) -> DegradationAnalysis {
    let mut analysis = DegradationAnalysis {
        speed_trend_pct_per_hour: 0.0,
        memory_trend_mb_per_hour: 0.0,
        speed_first_window_mbps: 0.0,
        speed_last_window_mbps: 0.0,
        memory_first_window_mb: 0.0,
        memory_last_window_mb: 0.0,
        degradation_detected: false,
        notes: Vec::new(),
    };

    if windows.len() < 3 {
        analysis
            .notes
            .push("Insufficient data for degradation analysis (need >=3 windows)".into());
        return analysis;
    }

    // Skip the first window (warmup) for trend analysis
    let analysis_windows = &windows[1..];
    if analysis_windows.is_empty() {
        return analysis;
    }

    analysis.speed_first_window_mbps = analysis_windows.first().unwrap().avg_speed_mbps;
    analysis.speed_last_window_mbps = analysis_windows.last().unwrap().avg_speed_mbps;
    analysis.memory_first_window_mb = analysis_windows.first().unwrap().avg_mem_mb;
    analysis.memory_last_window_mb = analysis_windows.last().unwrap().avg_mem_mb;

    // Simple linear regression for speed trend
    let n = analysis_windows.len() as f64;
    let speeds: Vec<f64> = analysis_windows.iter().map(|w| w.avg_speed_mbps).collect();
    let mems: Vec<f64> = analysis_windows.iter().map(|w| w.avg_mem_mb).collect();
    let hours: Vec<f64> = analysis_windows
        .iter()
        .map(|w| w.window_start_secs / 3600.0)
        .collect();

    // Speed regression
    let speed_slope = linear_regression_slope(&hours, &speeds);
    let avg_speed = speeds.iter().sum::<f64>() / n;
    if avg_speed > 0.0 {
        analysis.speed_trend_pct_per_hour = speed_slope / avg_speed * 100.0;
    }

    // Memory regression
    analysis.memory_trend_mb_per_hour = linear_regression_slope(&hours, &mems);

    // Detection thresholds
    if analysis.speed_trend_pct_per_hour < -5.0 {
        analysis.degradation_detected = true;
        analysis.notes.push(format!(
            "Speed declining at {:.1}%/hour",
            analysis.speed_trend_pct_per_hour
        ));
    }

    if analysis.memory_trend_mb_per_hour > 100.0 {
        analysis.degradation_detected = true;
        analysis.notes.push(format!(
            "Memory growing at {:.0} MB/hour (possible leak)",
            analysis.memory_trend_mb_per_hour
        ));
    }

    if !analysis.degradation_detected {
        analysis
            .notes
            .push("No significant degradation detected".into());
    }

    analysis
}

/// Compute the slope of a linear regression (y = mx + b, returns m).
fn linear_regression_slope(x: &[f64], y: &[f64]) -> f64 {
    let n = x.len() as f64;
    if n < 2.0 {
        return 0.0;
    }

    let sum_x: f64 = x.iter().sum();
    let sum_y: f64 = y.iter().sum();
    let sum_xy: f64 = x.iter().zip(y.iter()).map(|(a, b)| a * b).sum();
    let sum_x2: f64 = x.iter().map(|a| a * a).sum();

    let denom = n * sum_x2 - sum_x * sum_x;
    if denom.abs() < 1e-10 {
        return 0.0;
    }

    (n * sum_xy - sum_x * sum_y) / denom
}
