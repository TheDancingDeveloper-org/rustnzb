use std::io::{Cursor, Read as _};
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use axum::Json;
use axum::extract::{Multipart, Path, Query, State};
use axum::response::IntoResponse;
use flate2::read::GzDecoder;
use http::StatusCode;
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Shared HTTP client — connection-pooling; created once, reused across requests
// ---------------------------------------------------------------------------

static HTTP_CLIENT: std::sync::LazyLock<reqwest::Client> = std::sync::LazyLock::new(|| {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .expect("Failed to build shared HTTP client")
});

const MAX_NZB_DECOMPRESSED_BYTES: u64 = 100 * 1024 * 1024;
const MAX_FETCH_BODY_BYTES: usize = 100 * 1024 * 1024;

// ---------------------------------------------------------------------------
// SSRF guard
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct FetchUrlPlan {
    url: reqwest::Url,
    resolved_addrs: Option<(String, Vec<SocketAddr>)>,
}

/// Returns `Err` if `raw_url` is not http/https or resolves to a private/reserved address.
async fn validate_fetch_url(raw_url: &str) -> Result<FetchUrlPlan, ApiError> {
    let url = reqwest::Url::parse(raw_url)
        .map_err(|e| ApiError::from(anyhow::anyhow!("Invalid URL: {e}")))?;

    match url.scheme() {
        "http" | "https" => {}
        s => {
            return Err(ApiError::from(anyhow::anyhow!(
                "URL scheme '{s}' not allowed (must be http or https)"
            )));
        }
    }

    let host = url
        .host_str()
        .ok_or_else(|| ApiError::from(anyhow::anyhow!("URL has no host")))?
        .to_string();

    // IP literal: validate directly without a DNS round-trip.
    if let Ok(ip) = host.parse::<IpAddr>() {
        if !is_globally_routable(ip) {
            return Err(ApiError::from(anyhow::anyhow!(
                "URL targets a private/reserved address"
            )));
        }
        return Ok(FetchUrlPlan {
            url,
            resolved_addrs: None,
        });
    }

    // Hostname: resolve and check every returned address.
    let port = url.port_or_known_default().unwrap_or(80);
    let addrs: Vec<_> = tokio::net::lookup_host(format!("{host}:{port}"))
        .await
        .map_err(|e| ApiError::from(anyhow::anyhow!("DNS resolution failed for '{host}': {e}")))?
        .collect();

    if addrs.is_empty() {
        return Err(ApiError::from(anyhow::anyhow!(
            "DNS resolution returned no addresses for '{host}'"
        )));
    }

    for addr in &addrs {
        if !is_globally_routable(addr.ip()) {
            return Err(ApiError::from(anyhow::anyhow!(
                "URL resolves to a private/reserved address"
            )));
        }
    }

    Ok(FetchUrlPlan {
        url,
        resolved_addrs: Some((host, addrs)),
    })
}

fn build_fetch_client(plan: &FetchUrlPlan) -> Result<reqwest::Client, ApiError> {
    let mut builder = reqwest::Client::builder().timeout(std::time::Duration::from_secs(30));
    if let Some((host, addrs)) = &plan.resolved_addrs {
        builder = builder.resolve_to_addrs(host, addrs.as_slice());
    }
    builder
        .build()
        .map_err(|e| ApiError::from(anyhow::anyhow!("Failed to build fetch client: {e}")))
}

async fn read_response_bytes_limited(
    mut response: reqwest::Response,
    max_bytes: usize,
) -> Result<Vec<u8>, ApiError> {
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| ApiError::from(anyhow::anyhow!("Failed to read response: {e}")))?
    {
        if body.len().saturating_add(chunk.len()) > max_bytes {
            return Err(ApiError::from(anyhow::anyhow!(
                "Fetched body exceeds the {} MB limit",
                max_bytes / 1024 / 1024
            )));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn is_globally_routable(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            !v4.is_loopback()
                && !v4.is_private()
                && !v4.is_link_local()
                && !v4.is_broadcast()
                && !v4.is_unspecified()
                && !v4.is_documentation()
        }
        IpAddr::V6(v6) => {
            !v6.is_loopback() && !v6.is_unspecified() && !v6.is_multicast() && !v6.is_unique_local()
        }
    }
}

#[cfg(feature = "webdav")]
use nzb_web::nzb_core::config::DavConfig;
use nzb_web::nzb_core::config::{CategoryConfig, RssFeedConfig, ServerConfig};
use nzb_web::nzb_core::models::*;
use nzb_web::nzb_core::nzb_parser;
use nzb_web::nzb_core::sabnzbd_import;

use nzb_web::error::ApiError;
use nzb_web::log_buffer::LogEntry;
use nzb_web::state::AppState;

// ---------------------------------------------------------------------------
// Priority helpers
// ---------------------------------------------------------------------------

/// Maps the integer priority wire format to the `Priority` enum.
/// 0 = Low, 1 = Normal, 2 = High, 3 = Force; anything else → Normal.
fn priority_from_i32(p: i32) -> Priority {
    match p {
        0 => Priority::Low,
        2 => Priority::High,
        3 => Priority::Force,
        _ => Priority::Normal,
    }
}

// ---------------------------------------------------------------------------
// Query parameters
// ---------------------------------------------------------------------------

#[derive(Deserialize, Default)]
pub struct QueueQuery {
    pub limit: Option<usize>,
    pub offset: Option<usize>,
}

#[derive(Deserialize, Default)]
pub struct HistoryQuery {
    pub limit: Option<usize>,
}

#[derive(Deserialize)]
pub struct AddNzbQuery {
    pub category: Option<String>,
    pub priority: Option<i32>,
    pub name: Option<String>,
}

#[derive(Deserialize, Default)]
pub struct LogQuery {
    pub job_id: Option<String>,
    pub after_seq: Option<u64>,
    pub level: Option<String>,
    pub limit: Option<usize>,
}

#[derive(Deserialize)]
pub struct PauseForQuery {
    pub duration_secs: u64,
}

#[derive(Deserialize)]
pub struct MoveJobBody {
    pub position: usize,
}

#[derive(Deserialize, Serialize)]
pub struct HistoryRetentionBody {
    pub retention: Option<usize>,
}

#[derive(Deserialize, Serialize)]
pub struct MaxActiveDownloadsBody {
    pub max_active_downloads: usize,
}

#[derive(Deserialize)]
pub struct SetPriorityBody {
    pub priority: i32,
}

// ---------------------------------------------------------------------------
// Response types
// ---------------------------------------------------------------------------

#[derive(Serialize)]
pub struct QueueResponse {
    pub jobs: Vec<NzbJob>,
    pub total: usize,
    pub speed_bps: u64,
    pub paused: bool,
}

#[derive(Serialize)]
pub struct HistoryResponse {
    pub entries: Vec<HistoryResponseEntry>,
    pub total: usize,
}

#[derive(Serialize)]
pub struct HistoryResponseEntry {
    pub id: String,
    pub name: String,
    pub category: String,
    pub status: JobStatus,
    pub total_bytes: u64,
    pub downloaded_bytes: u64,
    pub added_at: chrono::DateTime<chrono::Utc>,
    pub completed_at: chrono::DateTime<chrono::Utc>,
    pub output_dir: String,
    pub stages: Vec<StageResult>,
    pub error_message: Option<String>,
    pub server_stats: Vec<ServerArticleStats>,
    pub has_nzb_data: bool,
}

impl From<HistoryEntry> for HistoryResponseEntry {
    fn from(e: HistoryEntry) -> Self {
        let has_nzb = e.nzb_data.is_some();
        Self {
            id: e.id,
            name: e.name,
            category: e.category,
            status: e.status,
            total_bytes: e.total_bytes,
            downloaded_bytes: e.downloaded_bytes,
            added_at: e.added_at,
            completed_at: e.completed_at,
            output_dir: e.output_dir.to_string_lossy().to_string(),
            stages: e.stages,
            error_message: e.error_message,
            server_stats: e.server_stats,
            has_nzb_data: has_nzb,
        }
    }
}

#[derive(Serialize)]
pub struct AddNzbResponse {
    pub status: bool,
    pub nzo_ids: Vec<String>,
}

#[derive(Serialize)]
pub struct StatusResponse {
    pub version: &'static str,
    pub paused: bool,
    pub speed_bps: u64,
    pub speed_limit_bps: u64,
    pub queue_size: usize,
    pub disk_space_free: u64,
    pub min_free_space_bytes: u64,
    pub pause_remaining_secs: Option<i64>,
    pub webdav_enabled: bool,
}

#[derive(Serialize)]
pub struct SimpleResponse {
    pub status: bool,
}

#[derive(Serialize)]
pub struct LogResponse {
    pub entries: Vec<LogEntry>,
    pub latest_seq: u64,
}

// ---------------------------------------------------------------------------
// Queue handlers
// ---------------------------------------------------------------------------

/// GET /api/queue -- List all jobs in the download queue.
pub async fn h_queue_list(
    State(state): State<Arc<AppState>>,
    Query(q): Query<QueueQuery>,
) -> Result<Json<QueueResponse>, ApiError> {
    let qm = &state.queue_manager;
    let all_jobs = qm.get_jobs();
    let total = all_jobs.len();
    let speed_bps = qm.get_speed();
    let paused = qm.is_paused();

    // Apply pagination (default: first 100 jobs)
    let offset = q.offset.unwrap_or(0);
    let limit = q.limit.unwrap_or(100);
    let jobs: Vec<_> = all_jobs.into_iter().skip(offset).take(limit).collect();

    Ok(Json(QueueResponse {
        jobs,
        total,
        speed_bps,
        paused,
    }))
}

/// Extract NZB files from an uploaded file. If it's an archive (zip, gz),
/// returns all `.nzb` entries found inside. Otherwise returns the file as-is.
fn extract_nzbs(file_name: &str, data: &[u8]) -> Result<Vec<(String, Vec<u8>)>, anyhow::Error> {
    let lower = file_name.to_lowercase();

    // .nzb.gz or .gz containing an nzb
    if lower.ends_with(".gz") {
        let mut decoder = GzDecoder::new(data);
        let mut decompressed = Vec::new();
        decoder
            .by_ref()
            .take(MAX_NZB_DECOMPRESSED_BYTES + 1)
            .read_to_end(&mut decompressed)
            .map_err(|e| anyhow::anyhow!("Failed to decompress gzip: {e}"))?;
        if decompressed.len() as u64 > MAX_NZB_DECOMPRESSED_BYTES {
            anyhow::bail!(
                "Decompressed NZB exceeds the {} MB limit",
                MAX_NZB_DECOMPRESSED_BYTES / 1024 / 1024
            );
        }
        let inner_name = file_name
            .strip_suffix(".gz")
            .or_else(|| file_name.strip_suffix(".GZ"))
            .unwrap_or(file_name);
        return Ok(vec![(inner_name.to_string(), decompressed)]);
    }

    // .zip archive — extract all .nzb files inside
    if lower.ends_with(".zip") {
        let cursor = Cursor::new(data);
        let mut archive = zip::ZipArchive::new(cursor)
            .map_err(|e| anyhow::anyhow!("Failed to read zip archive: {e}"))?;
        let mut nzbs = Vec::new();
        let mut total_uncompressed = 0u64;
        for i in 0..archive.len() {
            let mut entry = archive
                .by_index(i)
                .map_err(|e| anyhow::anyhow!("Zip entry error: {e}"))?;
            let entry_name = entry.name().to_string();
            if entry_name.to_lowercase().ends_with(".nzb") {
                total_uncompressed = total_uncompressed
                    .checked_add(entry.size())
                    .ok_or_else(|| anyhow::anyhow!("Zip archive size overflow"))?;
                if total_uncompressed > MAX_NZB_DECOMPRESSED_BYTES {
                    anyhow::bail!(
                        "Decompressed NZB exceeds the {} MB limit",
                        MAX_NZB_DECOMPRESSED_BYTES / 1024 / 1024
                    );
                }

                let mut buf = Vec::new();
                entry
                    .by_ref()
                    .take(MAX_NZB_DECOMPRESSED_BYTES + 1)
                    .read_to_end(&mut buf)
                    .map_err(|e| anyhow::anyhow!("Failed to read zip entry '{entry_name}': {e}"))?;
                if buf.len() as u64 > MAX_NZB_DECOMPRESSED_BYTES {
                    anyhow::bail!(
                        "Decompressed NZB exceeds the {} MB limit",
                        MAX_NZB_DECOMPRESSED_BYTES / 1024 / 1024
                    );
                }
                nzbs.push((entry_name, buf));
            }
        }
        if nzbs.is_empty() {
            anyhow::bail!("No .nzb files found in zip archive '{file_name}'");
        }
        return Ok(nzbs);
    }

    // Plain .nzb or unrecognized — pass through as-is
    Ok(vec![(file_name.to_string(), data.to_vec())])
}

/// Enqueue a single NZB from raw bytes, applying category/priority from query params.
fn enqueue_nzb(
    state: &AppState,
    q: &AddNzbQuery,
    file_name: &str,
    data: Vec<u8>,
) -> Result<String, ApiError> {
    let name = q.name.clone().unwrap_or_else(|| {
        file_name
            .strip_suffix(".nzb")
            .unwrap_or(file_name)
            .to_string()
    });

    let mut job = nzb_parser::parse_nzb(&name, &data).map_err(ApiError::from)?;

    if let Some(ref cat) = q.category {
        job.category = cat.clone();
    }
    if let Some(prio) = q.priority {
        job.priority = priority_from_i32(prio);
    }

    let qm = &state.queue_manager;
    job.work_dir = qm.incomplete_dir().join(&job.id);
    job.output_dir = qm.complete_dir().join(&job.category).join(&job.name);

    std::fs::create_dir_all(&job.work_dir).map_err(|e| {
        ApiError::from(anyhow::anyhow!(
            "Failed to create work dir '{}': {}",
            job.work_dir.display(),
            e
        ))
    })?;

    let id = job.id.clone();

    tracing::info!(
        name = %job.name,
        id = %job.id,
        files = job.file_count,
        articles = job.article_count,
        "NZB added to queue"
    );

    qm.add_job(job, Some(data)).map_err(ApiError::from)?;
    Ok(id)
}

/// POST /api/queue/add -- Add NZB file(s) to the queue.
/// Accepts `.nzb` files directly, or `.zip`/`.gz` archives containing `.nzb` files.
/// Multiple files can be uploaded in a single multipart request.
pub async fn h_queue_add(
    State(state): State<Arc<AppState>>,
    Query(q): Query<AddNzbQuery>,
    mut multipart: Multipart,
) -> Result<impl IntoResponse, ApiError> {
    let mut nzo_ids = Vec::new();

    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| ApiError::from(anyhow::anyhow!("Multipart error: {e}")))?
    {
        let file_name = field
            .file_name()
            .map(|s| s.to_string())
            .unwrap_or_else(|| "unknown.nzb".into());

        let data = field
            .bytes()
            .await
            .map_err(|e| ApiError::from(anyhow::anyhow!("Read error: {e}")))?;

        // Extract NZBs (handles zip/gz archives or plain .nzb)
        let nzbs = extract_nzbs(&file_name, &data).map_err(ApiError::from)?;

        for (nzb_name, nzb_data) in nzbs {
            let id = enqueue_nzb(&state, &q, &nzb_name, nzb_data)?;
            nzo_ids.push(id);
        }
    }

    Ok((
        StatusCode::OK,
        Json(AddNzbResponse {
            status: true,
            nzo_ids,
        }),
    ))
}

/// PUT /api/queue/{id}/priority -- Change job priority.
pub async fn h_queue_set_priority(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<SetPriorityBody>,
) -> Result<Json<SimpleResponse>, ApiError> {
    let priority = match body.priority {
        0..=3 => priority_from_i32(body.priority),
        _ => return Err(ApiError::from(anyhow::anyhow!("Invalid priority value"))),
    };
    state
        .queue_manager
        .set_job_priority(&id, priority)
        .map_err(ApiError::from)?;
    Ok(Json(SimpleResponse { status: true }))
}

// ---------------------------------------------------------------------------
// Add URL handler
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct AddUrlBody {
    pub url: String,
    pub name: Option<String>,
    pub category: Option<String>,
    pub priority: Option<i32>,
}

/// POST /api/queue/add-url -- Add an NZB from a URL.
pub async fn h_queue_add_url(
    State(state): State<Arc<AppState>>,
    Json(body): Json<AddUrlBody>,
) -> Result<impl IntoResponse, ApiError> {
    if body.url.is_empty() {
        return Err(ApiError::from(anyhow::anyhow!("No URL provided")));
    }

    let fetch_plan = validate_fetch_url(&body.url).await?;
    tracing::info!(url = %body.url, "Fetching NZB from URL");

    let client = fetch_plan
        .resolved_addrs
        .as_ref()
        .map(|_| build_fetch_client(&fetch_plan))
        .transpose()?
        .unwrap_or_else(|| HTTP_CLIENT.clone());
    let response = client
        .get(fetch_plan.url.clone())
        .send()
        .await
        .map_err(|e| ApiError::from(anyhow::anyhow!("Failed to fetch URL: {e}")))?;

    if !response.status().is_success() {
        return Err(ApiError::from(anyhow::anyhow!(
            "URL returned HTTP {}",
            response.status()
        )));
    }

    let data = read_response_bytes_limited(response, MAX_FETCH_BODY_BYTES).await?;

    // Derive file name from URL path for archive extraction and job naming.
    let file_name = body
        .url
        .rsplit('/')
        .next()
        .and_then(|s| s.split('?').next())
        .filter(|s| !s.is_empty())
        .unwrap_or("download.nzb")
        .to_string();

    let q = AddNzbQuery {
        name: body.name,
        category: body.category.filter(|c| !c.is_empty()),
        priority: body.priority,
    };

    let nzbs = extract_nzbs(&file_name, &data).map_err(ApiError::from)?;
    let mut nzo_ids = Vec::new();
    for (nzb_name, nzb_data) in nzbs {
        let id = enqueue_nzb(&state, &q, &nzb_name, nzb_data)?;
        nzo_ids.push(id);
    }

    Ok((
        StatusCode::OK,
        Json(AddNzbResponse {
            status: true,
            nzo_ids,
        }),
    ))
}

/// POST /api/queue/{id}/pause -- Pause a job.
pub async fn h_queue_pause(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<SimpleResponse>, ApiError> {
    state.queue_manager.pause_job(&id).map_err(ApiError::from)?;
    Ok(Json(SimpleResponse { status: true }))
}

/// POST /api/queue/{id}/resume -- Resume a paused job.
pub async fn h_queue_resume(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<SimpleResponse>, ApiError> {
    state
        .queue_manager
        .resume_job(&id)
        .map_err(ApiError::from)?;
    Ok(Json(SimpleResponse { status: true }))
}

/// DELETE /api/queue/{id} -- Remove a job from the queue.
pub async fn h_queue_delete(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<SimpleResponse>, ApiError> {
    state
        .queue_manager
        .remove_job(&id)
        .map_err(ApiError::from)?;
    Ok(Json(SimpleResponse { status: true }))
}

/// POST /api/queue/{id}/move -- Move a job to a new position.
pub async fn h_queue_move(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<MoveJobBody>,
) -> Result<Json<SimpleResponse>, ApiError> {
    state
        .queue_manager
        .move_job(&id, body.position)
        .map_err(ApiError::from)?;
    Ok(Json(SimpleResponse { status: true }))
}

/// POST /api/queue/pause -- Pause all downloads.
pub async fn h_queue_pause_all(
    State(state): State<Arc<AppState>>,
) -> Result<Json<SimpleResponse>, ApiError> {
    state.queue_manager.pause_all();
    Ok(Json(SimpleResponse { status: true }))
}

/// POST /api/queue/resume -- Resume all downloads.
pub async fn h_queue_resume_all(
    State(state): State<Arc<AppState>>,
) -> Result<Json<SimpleResponse>, ApiError> {
    state.queue_manager.resume_all();
    Ok(Json(SimpleResponse { status: true }))
}

/// POST /api/queue/pause-for -- Pause all downloads for a duration.
pub async fn h_queue_pause_for(
    State(state): State<Arc<AppState>>,
    Query(q): Query<PauseForQuery>,
) -> Result<Json<SimpleResponse>, ApiError> {
    state.queue_manager.pause_for(q.duration_secs);
    Ok(Json(SimpleResponse { status: true }))
}

// ---------------------------------------------------------------------------
// History handlers
// ---------------------------------------------------------------------------

/// GET /api/history -- List completed/failed jobs.
pub async fn h_history_list(
    State(state): State<Arc<AppState>>,
    Query(q): Query<HistoryQuery>,
) -> Result<Json<HistoryResponse>, ApiError> {
    let limit = q.limit.unwrap_or(50);
    let entries = state
        .queue_manager
        .history_list(limit)
        .map_err(ApiError::from)?;
    let total = entries.len();
    let entries: Vec<HistoryResponseEntry> = entries.into_iter().map(Into::into).collect();
    Ok(Json(HistoryResponse { entries, total }))
}

/// DELETE /api/history/{id} -- Remove a history entry.
pub async fn h_history_delete(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<SimpleResponse>, ApiError> {
    state
        .queue_manager
        .history_remove(&id)
        .map_err(ApiError::from)?;
    Ok(Json(SimpleResponse { status: true }))
}

/// DELETE /api/history -- Clear all history.
pub async fn h_history_clear(
    State(state): State<Arc<AppState>>,
) -> Result<Json<SimpleResponse>, ApiError> {
    state
        .queue_manager
        .history_clear()
        .map_err(ApiError::from)?;
    Ok(Json(SimpleResponse { status: true }))
}

/// POST /api/history/{id}/retry -- Re-add a failed/completed NZB from history.
pub async fn h_history_retry(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    // Get the history entry to get the name/category
    let entry = state
        .queue_manager
        .history_get(&id)
        .map_err(ApiError::from)?
        .ok_or_else(|| ApiError::from(anyhow::anyhow!("History entry not found")))?;

    // Get the raw NZB data
    let nzb_data = state
        .queue_manager
        .history_get_nzb_data(&id)
        .map_err(ApiError::from)?
        .ok_or_else(|| ApiError::from(anyhow::anyhow!("No NZB data stored for this entry")))?;

    // Re-parse the NZB
    let mut job = nzb_parser::parse_nzb(&entry.name, &nzb_data).map_err(ApiError::from)?;

    job.category = entry.category.clone();

    // Set working directories
    let qm = &state.queue_manager;
    job.work_dir = qm.incomplete_dir().join(&job.id);
    job.output_dir = qm.complete_dir().join(&job.category).join(&job.name);

    std::fs::create_dir_all(&job.work_dir).map_err(|e| {
        ApiError::from(anyhow::anyhow!(
            "Failed to create work dir '{}': {}",
            job.work_dir.display(),
            e
        ))
    })?;

    let new_id = job.id.clone();

    tracing::info!(
        name = %job.name,
        id = %new_id,
        original_id = %id,
        "Retrying NZB from history"
    );

    qm.add_job(job, Some(nzb_data)).map_err(ApiError::from)?;

    Ok((
        StatusCode::OK,
        Json(AddNzbResponse {
            status: true,
            nzo_ids: vec![new_id],
        }),
    ))
}

// ---------------------------------------------------------------------------
// Status handler
// ---------------------------------------------------------------------------

/// GET /api/status -- Overall application status.
pub async fn h_status(
    State(state): State<Arc<AppState>>,
    #[cfg(feature = "webdav")] axum::Extension(dav): axum::Extension<
        Option<Arc<crate::dav::DavHandle>>,
    >,
) -> Result<Json<StatusResponse>, ApiError> {
    let qm = &state.queue_manager;
    let config = state.config();
    Ok(Json(StatusResponse {
        version: env!("CARGO_PKG_VERSION"),
        paused: qm.is_paused(),
        speed_bps: qm.get_speed(),
        speed_limit_bps: qm.get_speed_limit(),
        queue_size: qm.queue_size(),
        disk_space_free: get_disk_space_free(&config.general.complete_dir),
        min_free_space_bytes: qm.min_free_space(),
        pause_remaining_secs: qm.pause_remaining_secs(),
        #[cfg(feature = "webdav")]
        webdav_enabled: dav.is_some(),
        #[cfg(not(feature = "webdav"))]
        webdav_enabled: false,
    }))
}

// ---------------------------------------------------------------------------
// Log handler
// ---------------------------------------------------------------------------

/// GET /api/logs -- Get log entries.
pub async fn h_logs(
    State(state): State<Arc<AppState>>,
    Query(q): Query<LogQuery>,
) -> Result<Json<LogResponse>, ApiError> {
    let limit = q.limit.unwrap_or(200);
    let entries =
        state
            .log_buffer
            .get_entries(q.job_id.as_deref(), q.after_seq, q.level.as_deref(), limit);
    let latest_seq = state.log_buffer.latest_seq();
    Ok(Json(LogResponse {
        entries,
        latest_seq,
    }))
}

// ---------------------------------------------------------------------------
// Config handlers
// ---------------------------------------------------------------------------

/// GET /api/config -- Get current configuration.
pub async fn h_config_get(
    State(state): State<Arc<AppState>>,
) -> Result<Json<nzb_web::nzb_core::config::AppConfig>, ApiError> {
    Ok(Json((*state.config()).clone()))
}

/// GET /api/config/servers -- List configured servers.
pub async fn h_servers_list(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Vec<ServerConfig>>, ApiError> {
    Ok(Json(state.config().servers.clone()))
}

/// POST /api/config/servers -- Add a new server.
pub async fn h_server_add(
    State(state): State<Arc<AppState>>,
    Json(mut server): Json<ServerConfig>,
) -> Result<impl IntoResponse, ApiError> {
    // Generate ID if empty
    if server.id.is_empty() {
        server.id = uuid::Uuid::new_v4().to_string();
    }
    sanitize_server_config(&mut server);

    let mut config = (*state.config()).clone();
    config.servers.push(server);
    state
        .update_config(config.clone())
        .map_err(ApiError::from)?;
    state.queue_manager.update_servers(config.servers);

    Ok((StatusCode::OK, Json(SimpleResponse { status: true })))
}

/// PUT /api/config/servers/{id} -- Update an existing server.
pub async fn h_server_update(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(mut server): Json<ServerConfig>,
) -> Result<Json<SimpleResponse>, ApiError> {
    sanitize_server_config(&mut server);
    let mut config = (*state.config()).clone();

    let idx = config
        .servers
        .iter()
        .position(|s| s.id == id)
        .ok_or_else(|| ApiError::from(anyhow::anyhow!("Server not found: {id}")))?;

    config.servers[idx] = server;
    state
        .update_config(config.clone())
        .map_err(ApiError::from)?;
    state.queue_manager.update_servers(config.servers);

    Ok(Json(SimpleResponse { status: true }))
}

/// DELETE /api/config/servers/{id} -- Delete a server.
pub async fn h_server_delete(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<SimpleResponse>, ApiError> {
    let mut config = (*state.config()).clone();
    let before = config.servers.len();
    config.servers.retain(|s| s.id != id);

    if config.servers.len() == before {
        return Err(ApiError::from(anyhow::anyhow!("Server not found: {id}")));
    }

    state
        .update_config(config.clone())
        .map_err(ApiError::from)?;
    state.queue_manager.update_servers(config.servers);

    Ok(Json(SimpleResponse { status: true }))
}

/// POST /api/config/servers/{id}/test -- Test a server connection.
pub async fn h_server_test(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<ServerTestResponse>, ApiError> {
    let config = state.config();
    let server = config
        .servers
        .iter()
        .find(|s| s.id == id)
        .ok_or_else(|| ApiError::from(anyhow::anyhow!("Server not found: {id}")))?
        .clone();

    // Test connection in a spawned task with timeout
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(15),
        test_server_connection(server),
    )
    .await;

    match result {
        Ok(Ok(msg)) => Ok(Json(ServerTestResponse {
            success: true,
            message: msg,
        })),
        Ok(Err(msg)) => Ok(Json(ServerTestResponse {
            success: false,
            message: msg,
        })),
        Err(_) => Ok(Json(ServerTestResponse {
            success: false,
            message: "Connection timed out after 15 seconds".into(),
        })),
    }
}

#[derive(Serialize)]
pub struct ServerTestResponse {
    pub success: bool,
    pub message: String,
}

/// POST /api/config/servers/test-config -- Test a server config without saving.
pub async fn h_server_test_inline(
    Json(server): Json<ServerConfig>,
) -> Result<Json<ServerTestResponse>, ApiError> {
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(15),
        test_server_connection(server),
    )
    .await;

    match result {
        Ok(Ok(msg)) => Ok(Json(ServerTestResponse {
            success: true,
            message: msg,
        })),
        Ok(Err(msg)) => Ok(Json(ServerTestResponse {
            success: false,
            message: msg,
        })),
        Err(_) => Ok(Json(ServerTestResponse {
            success: false,
            message: "Connection timed out after 15 seconds".into(),
        })),
    }
}

/// Strip whitespace from user-supplied string fields before persisting or
/// connecting. Paste-in-hostname with a trailing `\n` or space makes
/// `getaddrinfo` fail with a misleading "Name does not resolve" even for
/// literal IPs — trimming on the server side defeats that class of bug
/// regardless of what the frontend sent.
pub fn sanitize_server_config(s: &mut ServerConfig) {
    fn trim_in_place(v: &mut String) {
        let t = v.trim();
        if t.len() != v.len() {
            *v = t.to_string();
        }
    }
    fn trim_opt(v: &mut Option<String>) {
        if let Some(inner) = v.as_mut() {
            trim_in_place(inner);
        }
    }
    trim_in_place(&mut s.host);
    trim_in_place(&mut s.name);
    trim_opt(&mut s.username);
    trim_opt(&mut s.password);
    trim_opt(&mut s.proxy_url);
    trim_opt(&mut s.trusted_fingerprint);
}

async fn test_server_connection(mut server: ServerConfig) -> Result<String, String> {
    sanitize_server_config(&mut server);
    use nzb_web::nzb_core::nzb_nntp::connection::NntpConnection;

    let mut conn = NntpConnection::new(format!("test-{}", server.id));
    conn.connect(&server)
        .await
        .map_err(|e| format!("Connection failed: {e}"))?;
    let _ = conn.quit().await;
    Ok(format!(
        "Successfully connected to {}:{}",
        server.host, server.port
    ))
}

/// GET /api/history/{id}/logs -- Get persisted logs for a history entry.
pub async fn h_history_logs(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<LogResponse>, ApiError> {
    let logs_json = state
        .queue_manager
        .history_get_logs(&id)
        .map_err(ApiError::from)?;

    let entries: Vec<LogEntry> = match logs_json {
        Some(json) if !json.is_empty() && json != "[]" => {
            serde_json::from_str(&json).unwrap_or_default()
        }
        _ => Vec::new(),
    };

    let latest_seq = entries.last().map(|e| e.seq).unwrap_or(0);
    Ok(Json(LogResponse {
        entries,
        latest_seq,
    }))
}

/// GET /api/config/categories -- List configured categories.
pub async fn h_categories_list(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Vec<nzb_web::nzb_core::config::CategoryConfig>>, ApiError> {
    Ok(Json(state.config().categories.clone()))
}

/// POST /api/config/categories -- Add a new category.
pub async fn h_category_add(
    State(state): State<Arc<AppState>>,
    Json(cat): Json<CategoryConfig>,
) -> Result<impl IntoResponse, ApiError> {
    let mut config = (*state.config()).clone();
    if config.categories.iter().any(|c| c.name == cat.name) {
        return Err(ApiError::from(anyhow::anyhow!(
            "Category '{}' already exists",
            cat.name
        )));
    }
    config.categories.push(cat);
    state
        .queue_manager
        .set_categories(config.categories.clone());
    state.update_config(config).map_err(ApiError::from)?;
    Ok(Json(serde_json::json!({"status": true})))
}

/// PUT /api/config/categories/{name} -- Update a category.
pub async fn h_category_update(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    Json(cat): Json<CategoryConfig>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let mut config = (*state.config()).clone();
    let idx = config
        .categories
        .iter()
        .position(|c| c.name == name)
        .ok_or_else(|| ApiError::from(anyhow::anyhow!("Category not found")))?;
    config.categories[idx] = cat;
    state
        .queue_manager
        .set_categories(config.categories.clone());
    state.update_config(config).map_err(ApiError::from)?;
    Ok(Json(serde_json::json!({"status": true})))
}

/// DELETE /api/config/categories/{name} -- Delete a category.
pub async fn h_category_delete(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let mut config = (*state.config()).clone();
    let initial_len = config.categories.len();
    config.categories.retain(|c| c.name != name);
    if config.categories.len() == initial_len {
        return Err(ApiError::from(anyhow::anyhow!("Category not found")));
    }
    state
        .queue_manager
        .set_categories(config.categories.clone());
    state.update_config(config).map_err(ApiError::from)?;
    Ok(Json(serde_json::json!({"status": true})))
}

/// PUT /api/config/history-retention -- Update history retention setting.
pub async fn h_history_retention_set(
    State(state): State<Arc<AppState>>,
    Json(body): Json<HistoryRetentionBody>,
) -> Result<Json<SimpleResponse>, ApiError> {
    let mut config = (*state.config()).clone();
    config.general.history_retention = body.retention;
    state.update_config(config).map_err(ApiError::from)?;
    state.queue_manager.set_history_retention(body.retention);
    Ok(Json(SimpleResponse { status: true }))
}

/// GET /api/config/history-retention -- Get history retention setting.
pub async fn h_history_retention_get(
    State(state): State<Arc<AppState>>,
) -> Result<Json<HistoryRetentionBody>, ApiError> {
    let config = state.config();
    Ok(Json(HistoryRetentionBody {
        retention: config.general.history_retention,
    }))
}

/// PUT /api/config/max-active-downloads -- Update max concurrent downloads.
pub async fn h_max_active_downloads_set(
    State(state): State<Arc<AppState>>,
    Json(body): Json<MaxActiveDownloadsBody>,
) -> Result<Json<SimpleResponse>, ApiError> {
    let mut config = (*state.config()).clone();
    config.general.max_active_downloads = body.max_active_downloads;
    state.update_config(config).map_err(ApiError::from)?;
    state
        .queue_manager
        .set_max_active_downloads(body.max_active_downloads);
    Ok(Json(SimpleResponse { status: true }))
}

/// GET /api/config/max-active-downloads -- Get max concurrent downloads.
pub async fn h_max_active_downloads_get(
    State(state): State<Arc<AppState>>,
) -> Result<Json<MaxActiveDownloadsBody>, ApiError> {
    let config = state.config();
    Ok(Json(MaxActiveDownloadsBody {
        max_active_downloads: config.general.max_active_downloads,
    }))
}

// ---------------------------------------------------------------------------
// Speed limit handlers
// ---------------------------------------------------------------------------

#[derive(Serialize)]
pub struct SpeedLimitResponse {
    pub speed_limit_bps: u64,
}

/// GET /api/config/speed-limit -- Get current speed limit.
pub async fn h_get_speed_limit(
    State(state): State<Arc<AppState>>,
) -> Result<Json<SpeedLimitResponse>, ApiError> {
    Ok(Json(SpeedLimitResponse {
        speed_limit_bps: state.queue_manager.get_speed_limit(),
    }))
}

#[derive(Deserialize)]
pub struct SetSpeedLimitBody {
    pub speed_limit_bps: u64,
}

/// PUT /api/config/speed-limit -- Set download speed limit.
pub async fn h_set_speed_limit(
    State(state): State<Arc<AppState>>,
    Json(body): Json<SetSpeedLimitBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    state.queue_manager.set_speed_limit(body.speed_limit_bps);
    // Also update config and persist
    let mut config = (*state.config()).clone();
    config.general.speed_limit_bps = body.speed_limit_bps;
    state.update_config(config).map_err(ApiError::from)?;
    Ok(Json(serde_json::json!({"status": true})))
}

// ---------------------------------------------------------------------------
// Disk guards handlers
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize)]
pub struct DiskGuardsBody {
    pub min_free_space_bytes: u64,
    pub abort_hopeless: bool,
}

/// GET /api/config/disk-guards -- Get disk guard settings.
pub async fn h_disk_guards_get(
    State(state): State<Arc<AppState>>,
) -> Result<Json<DiskGuardsBody>, ApiError> {
    let config = state.config();
    Ok(Json(DiskGuardsBody {
        min_free_space_bytes: config.general.min_free_space_bytes,
        abort_hopeless: config.general.abort_hopeless,
    }))
}

/// PUT /api/config/disk-guards -- Update disk guard settings (persisted; restart to apply).
pub async fn h_disk_guards_set(
    State(state): State<Arc<AppState>>,
    Json(body): Json<DiskGuardsBody>,
) -> Result<Json<SimpleResponse>, ApiError> {
    let mut config = (*state.config()).clone();
    config.general.min_free_space_bytes = body.min_free_space_bytes;
    config.general.abort_hopeless = body.abort_hopeless;
    state.update_config(config).map_err(ApiError::from)?;
    Ok(Json(SimpleResponse { status: true }))
}

// ---------------------------------------------------------------------------
// RSS feed handlers
// ---------------------------------------------------------------------------

/// GET /api/config/rss-feeds -- List RSS feeds.
pub async fn h_rss_feeds_list(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Vec<RssFeedConfig>>, ApiError> {
    let config = state.config();
    Ok(Json(config.rss_feeds.clone()))
}

/// POST /api/config/rss-feeds -- Add an RSS feed.
pub async fn h_rss_feed_add(
    State(state): State<Arc<AppState>>,
    Json(feed): Json<RssFeedConfig>,
) -> Result<impl IntoResponse, ApiError> {
    let mut config = (*state.config()).clone();
    if config.rss_feeds.iter().any(|f| f.name == feed.name) {
        return Err(ApiError::from(anyhow::anyhow!(
            "Feed '{}' already exists",
            feed.name
        )));
    }
    config.rss_feeds.push(feed);
    state.update_config(config).map_err(ApiError::from)?;
    Ok(Json(serde_json::json!({"status": true})))
}

/// PUT /api/config/rss-feeds/{name} -- Update an RSS feed.
pub async fn h_rss_feed_update(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    Json(feed): Json<RssFeedConfig>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let mut config = (*state.config()).clone();
    let idx = config
        .rss_feeds
        .iter()
        .position(|f| f.name == name)
        .ok_or_else(|| ApiError::from(anyhow::anyhow!("Feed not found")))?;
    config.rss_feeds[idx] = feed;
    state.update_config(config).map_err(ApiError::from)?;
    Ok(Json(serde_json::json!({"status": true})))
}

/// DELETE /api/config/rss-feeds/{name} -- Delete an RSS feed.
pub async fn h_rss_feed_delete(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let mut config = (*state.config()).clone();
    let len = config.rss_feeds.len();
    config.rss_feeds.retain(|f| f.name != name);
    if config.rss_feeds.len() == len {
        return Err(ApiError::from(anyhow::anyhow!("Feed not found")));
    }
    state.update_config(config).map_err(ApiError::from)?;
    Ok(Json(serde_json::json!({"status": true})))
}

// ---------------------------------------------------------------------------
// RSS item handlers
// ---------------------------------------------------------------------------

#[derive(Deserialize, Default)]
pub struct RssItemsQuery {
    pub feed: Option<String>,
    pub limit: Option<usize>,
}

/// GET /api/rss/items -- List RSS feed items.
pub async fn h_rss_items_list(
    State(state): State<Arc<AppState>>,
    Query(q): Query<RssItemsQuery>,
) -> Result<Json<Vec<RssItem>>, ApiError> {
    let limit = q.limit.unwrap_or(500);
    let items = state
        .queue_manager
        .rss_items_list(q.feed.as_deref(), limit)
        .map_err(ApiError::from)?;
    Ok(Json(items))
}

/// POST /api/rss/items/{id}/download -- Download a specific RSS feed item.
pub async fn h_rss_item_download(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<SimpleResponse>, ApiError> {
    let item = state
        .queue_manager
        .rss_item_get(&id)
        .map_err(ApiError::from)?
        .ok_or_else(|| ApiError::from(anyhow::anyhow!("RSS item not found")))?;

    let url = item
        .url
        .as_ref()
        .ok_or_else(|| ApiError::from(anyhow::anyhow!("No download URL for this item")))?;

    // Fetch the NZB
    let fetch_plan = validate_fetch_url(url).await?;
    let client = fetch_plan
        .resolved_addrs
        .as_ref()
        .map(|_| build_fetch_client(&fetch_plan))
        .transpose()?
        .unwrap_or_else(|| HTTP_CLIENT.clone());
    let response = client
        .get(fetch_plan.url.clone())
        .send()
        .await
        .map_err(|e| ApiError::from(anyhow::anyhow!("Failed to fetch NZB: {e}")))?;

    if !response.status().is_success() {
        return Err(ApiError::from(anyhow::anyhow!(
            "HTTP {}",
            response.status()
        )));
    }

    let data = read_response_bytes_limited(response, MAX_FETCH_BODY_BYTES).await?;

    let mut job = nzb_parser::parse_nzb(&item.title, &data)
        .map_err(|e| ApiError::from(anyhow::anyhow!("Failed to parse NZB: {e}")))?;

    // Use the item's category or feed category
    if let Some(ref cat) = item.category {
        job.category = cat.clone();
    }

    job.work_dir = state.queue_manager.incomplete_dir().join(&job.id);
    job.output_dir = if let Some(ref cat) = item.category {
        state.queue_manager.complete_dir().join(cat).join(&job.name)
    } else {
        state.queue_manager.complete_dir().join(&job.name)
    };

    std::fs::create_dir_all(&job.work_dir).map_err(|e| {
        ApiError::from(anyhow::anyhow!(
            "Failed to create work dir '{}': {}",
            job.work_dir.display(),
            e
        ))
    })?;

    state
        .queue_manager
        .add_job(job, Some(data.to_vec()))
        .map_err(ApiError::from)?;

    // Mark as downloaded
    let _ = state
        .queue_manager
        .rss_item_mark_downloaded(&id, item.category.as_deref());

    Ok(Json(SimpleResponse { status: true }))
}

// ---------------------------------------------------------------------------
// RSS rule handlers
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct RssRuleBody {
    pub name: String,
    pub feed_names: Vec<String>,
    pub category: Option<String>,
    pub priority: Option<i32>,
    pub match_regex: String,
    pub enabled: Option<bool>,
}

/// GET /api/rss/rules -- List RSS download rules.
pub async fn h_rss_rules_list(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Vec<RssRule>>, ApiError> {
    let rules = state
        .queue_manager
        .rss_rule_list()
        .map_err(ApiError::from)?;
    Ok(Json(rules))
}

/// POST /api/rss/rules -- Add an RSS download rule.
pub async fn h_rss_rule_add(
    State(state): State<Arc<AppState>>,
    Json(body): Json<RssRuleBody>,
) -> Result<Json<SimpleResponse>, ApiError> {
    // Validate the regex
    regex::Regex::new(&body.match_regex)
        .map_err(|e| ApiError::from(anyhow::anyhow!("Invalid regex: {e}")))?;

    let rule = RssRule {
        id: uuid::Uuid::new_v4().to_string(),
        name: body.name,
        feed_names: body.feed_names,
        category: body.category,
        priority: body.priority.unwrap_or(1),
        match_regex: body.match_regex,
        enabled: body.enabled.unwrap_or(true),
    };
    state
        .queue_manager
        .rss_rule_insert(&rule)
        .map_err(ApiError::from)?;
    Ok(Json(SimpleResponse { status: true }))
}

/// PUT /api/rss/rules/{id} -- Update an RSS download rule.
pub async fn h_rss_rule_update(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<RssRuleBody>,
) -> Result<Json<SimpleResponse>, ApiError> {
    // Validate the regex
    regex::Regex::new(&body.match_regex)
        .map_err(|e| ApiError::from(anyhow::anyhow!("Invalid regex: {e}")))?;

    let rule = RssRule {
        id,
        name: body.name,
        feed_names: body.feed_names,
        category: body.category,
        priority: body.priority.unwrap_or(1),
        match_regex: body.match_regex,
        enabled: body.enabled.unwrap_or(true),
    };
    state
        .queue_manager
        .rss_rule_update(&rule)
        .map_err(ApiError::from)?;
    Ok(Json(SimpleResponse { status: true }))
}

/// DELETE /api/rss/rules/{id} -- Delete an RSS download rule.
pub async fn h_rss_rule_delete(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<SimpleResponse>, ApiError> {
    state
        .queue_manager
        .rss_rule_delete(&id)
        .map_err(ApiError::from)?;
    Ok(Json(SimpleResponse { status: true }))
}

// ---------------------------------------------------------------------------
// General settings handler
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct UpdateGeneralBody {
    pub incomplete_dir: Option<String>,
    pub complete_dir: Option<String>,
    pub data_dir: Option<String>,
    pub watch_dir: Option<String>,
    pub cache_size: Option<u64>,
    pub max_active_downloads: Option<usize>,
    pub history_retention: Option<Option<usize>>,
    pub rss_history_limit: Option<Option<usize>>,
}

/// PUT /api/config/general -- Update general settings.
pub async fn h_general_update(
    State(state): State<Arc<AppState>>,
    Json(body): Json<UpdateGeneralBody>,
) -> Result<Json<SimpleResponse>, ApiError> {
    let mut config = (*state.config()).clone();

    if let Some(dir) = body.incomplete_dir {
        config.general.incomplete_dir = dir.into();
    }
    if let Some(dir) = body.complete_dir {
        config.general.complete_dir = dir.into();
    }
    if let Some(dir) = body.data_dir {
        config.general.data_dir = dir.into();
    }
    // watch_dir: empty string means unset
    if let Some(dir) = body.watch_dir {
        config.general.watch_dir = if dir.is_empty() {
            None
        } else {
            Some(dir.into())
        };
    }
    if let Some(cs) = body.cache_size {
        config.general.cache_size = cs;
    }
    if let Some(mad) = body.max_active_downloads {
        state.queue_manager.set_max_active_downloads(mad);
        config.general.max_active_downloads = mad;
    }
    if let Some(ret) = body.history_retention {
        state.queue_manager.set_history_retention(ret);
        config.general.history_retention = ret;
    }
    if let Some(rss_limit) = body.rss_history_limit {
        config.general.rss_history_limit = rss_limit;
        // Prune RSS items if a limit is set
        if let Some(limit) = rss_limit {
            let _ = state.queue_manager.rss_items_prune(limit);
        }
    }

    state.update_config(config).map_err(ApiError::from)?;
    Ok(Json(SimpleResponse { status: true }))
}

// ---------------------------------------------------------------------------
// Server health check handler
// ---------------------------------------------------------------------------

#[derive(Serialize)]
pub struct ServerHealthResult {
    pub id: String,
    pub name: String,
    pub success: bool,
    pub message: String,
}

/// GET /api/config/servers/health -- Test all servers and return health status.
pub async fn h_servers_health(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Vec<ServerHealthResult>>, ApiError> {
    let servers = state.config().servers.clone();

    // Test all servers concurrently. Carry the original index so we can restore
    // the configured order in the response.
    let mut set = tokio::task::JoinSet::new();
    for (idx, server) in servers.into_iter().enumerate() {
        set.spawn(async move {
            let result = if !server.enabled {
                ServerHealthResult {
                    id: server.id,
                    name: server.name,
                    success: false,
                    message: "Disabled".into(),
                }
            } else {
                let (id, name) = (server.id.clone(), server.name.clone());
                match tokio::time::timeout(
                    std::time::Duration::from_secs(15),
                    test_server_connection(server),
                )
                .await
                {
                    Ok(Ok(msg)) => ServerHealthResult {
                        id,
                        name,
                        success: true,
                        message: msg,
                    },
                    Ok(Err(msg)) => ServerHealthResult {
                        id,
                        name,
                        success: false,
                        message: msg,
                    },
                    Err(_) => ServerHealthResult {
                        id,
                        name,
                        success: false,
                        message: "Connection timed out (15s)".into(),
                    },
                }
            };
            (idx, result)
        });
    }

    let mut indexed: Vec<(usize, ServerHealthResult)> = Vec::new();
    while let Some(res) = set.join_next().await {
        match res {
            Ok(pair) => indexed.push(pair),
            Err(e) => tracing::warn!("Server health check task panicked: {e}"),
        }
    }
    indexed.sort_by_key(|(i, _)| *i);

    Ok(Json(indexed.into_iter().map(|(_, r)| r).collect()))
}

/// GET /api/config/servers/stats -- Per-server download statistics.
pub async fn h_server_stats(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Vec<nzb_web::ServerStatsData>>, ApiError> {
    let servers = state.config().servers.clone();
    let stats = state.queue_manager.server_stats_get_all(&servers);
    Ok(Json(stats))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Get free disk space for a path (returns 0 on error).
fn get_disk_space_free(path: &std::path::Path) -> u64 {
    #[cfg(unix)]
    {
        use std::ffi::CString;
        use std::mem::MaybeUninit;
        let c_path = match CString::new(path.to_string_lossy().as_bytes()) {
            Ok(p) => p,
            Err(_) => return 0,
        };
        unsafe {
            let mut stat = MaybeUninit::<libc::statvfs>::uninit();
            if libc::statvfs(c_path.as_ptr(), stat.as_mut_ptr()) == 0 {
                let stat = stat.assume_init();
                #[allow(clippy::unnecessary_cast)] // u32 on macOS, u64 on Linux
                return stat.f_bavail as u64 * stat.f_frsize as u64;
            }
        }
        0
    }
    #[cfg(not(unix))]
    {
        0
    }
}

// ---------------------------------------------------------------------------
// Health check
// ---------------------------------------------------------------------------

pub async fn h_health() -> Json<serde_json::Value> {
    Json(serde_json::json!({"status": "ok"}))
}

// ---------------------------------------------------------------------------
// Directory browser
// ---------------------------------------------------------------------------

#[derive(Deserialize, Default)]
pub struct BrowseDirectoryQuery {
    pub path: Option<String>,
}

#[derive(Serialize)]
pub struct BrowseDirectoryResponse {
    pub current: String,
    pub parent: Option<String>,
    pub directories: Vec<String>,
}

/// GET /api/browse-directory -- List subdirectories for the directory picker.
pub async fn h_browse_directory(
    Query(q): Query<BrowseDirectoryQuery>,
) -> Result<Json<BrowseDirectoryResponse>, ApiError> {
    let path = q
        .path
        .filter(|p| !p.is_empty())
        .unwrap_or_else(|| "/".to_string());
    let dir = std::path::Path::new(&path);

    if !dir.is_dir() {
        return Err(ApiError::from(anyhow::anyhow!("Not a directory: {path}")));
    }

    let parent = dir.parent().map(|p| p.to_string_lossy().to_string());

    let mut directories = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            if let Ok(ft) = entry.file_type()
                && ft.is_dir()
                && let Some(name) = entry.file_name().to_str()
            {
                // Skip hidden directories
                if !name.starts_with('.') {
                    directories.push(entry.path().to_string_lossy().to_string());
                }
            }
        }
    }
    directories.sort();

    Ok(Json(BrowseDirectoryResponse {
        current: dir.to_string_lossy().to_string(),
        parent,
        directories,
    }))
}

// ---------------------------------------------------------------------------
// Queue category change
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct ChangeCategoryBody {
    pub category: String,
}

/// PUT /api/queue/{id}/category -- Change a job's category.
pub async fn h_queue_change_category(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<ChangeCategoryBody>,
) -> Result<Json<SimpleResponse>, ApiError> {
    state
        .queue_manager
        .change_job_category(&id, &body.category)
        .map_err(ApiError::from)?;
    Ok(Json(SimpleResponse { status: true }))
}

// ---------------------------------------------------------------------------
// Bulk queue operations
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct BulkActionBody {
    pub ids: Vec<String>,
    pub action: String,
    /// Optional value for priority (int) or category (string).
    pub value: Option<serde_json::Value>,
}

#[derive(Serialize)]
pub struct BulkActionResponse {
    pub status: bool,
    pub succeeded: usize,
    pub failed: usize,
}

/// POST /api/queue/bulk -- Perform an action on multiple jobs.
pub async fn h_queue_bulk_action(
    State(state): State<Arc<AppState>>,
    Json(body): Json<BulkActionBody>,
) -> Result<Json<BulkActionResponse>, ApiError> {
    let qm = &state.queue_manager;
    let mut succeeded = 0usize;
    let mut failed = 0usize;

    for id in &body.ids {
        let result = match body.action.as_str() {
            "pause" => qm.pause_job(id),
            "resume" => qm.resume_job(id),
            "delete" => qm.remove_job(id),
            "priority" => {
                let p = body.value.as_ref().and_then(|v| v.as_i64()).unwrap_or(1) as i32;
                qm.set_job_priority(id, priority_from_i32(p))
            }
            "category" => {
                let cat = body.value.as_ref().and_then(|v| v.as_str()).unwrap_or("");
                qm.change_job_category(id, cat)
            }
            _ => Err(nzb_web::nzb_core::NzbError::Other(format!(
                "Unknown action: {}",
                body.action
            ))),
        };
        match result {
            Ok(_) => succeeded += 1,
            Err(_) => failed += 1,
        }
    }

    Ok(Json(BulkActionResponse {
        status: failed == 0,
        succeeded,
        failed,
    }))
}

// ---------------------------------------------------------------------------
// SABnzbd Import / Setup
// ---------------------------------------------------------------------------

/// Setup status — tells the frontend whether the import wizard should be shown.
pub async fn h_setup_status(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let config = state.config();
    Json(serde_json::json!({
        "needs_setup": config.servers.is_empty(),
        "has_servers": !config.servers.is_empty(),
        "has_categories": !config.categories.is_empty(),
        "version": env!("CARGO_PKG_VERSION"),
    }))
}

/// Import SABnzbd INI file (multipart upload) → returns preview JSON.
pub async fn h_import_sabnzbd_ini(mut multipart: Multipart) -> Result<impl IntoResponse, ApiError> {
    let field = multipart
        .next_field()
        .await
        .map_err(|e| ApiError::from(anyhow::anyhow!("Multipart error: {e}")))?
        .ok_or(ApiError::bad_request("no file uploaded"))?;

    let data = field
        .bytes()
        .await
        .map_err(|e| ApiError::from(anyhow::anyhow!("Read error: {e}")))?;

    let content = String::from_utf8(data.to_vec())
        .map_err(|_| ApiError::bad_request("file is not valid UTF-8"))?;

    let preview = sabnzbd_import::parse_sabnzbd_ini(&content);
    Ok(Json(preview))
}

/// Import from a running SABnzbd instance via API → returns preview JSON.
#[derive(Deserialize)]
pub struct ImportApiRequest {
    pub url: String,
    pub api_key: String,
}

pub async fn h_import_sabnzbd_api(
    Json(req): Json<ImportApiRequest>,
) -> Result<impl IntoResponse, ApiError> {
    let base_url = req.url.trim_end_matches('/');

    let fetch_plan = validate_fetch_url(base_url).await?;
    let client = fetch_plan
        .resolved_addrs
        .as_ref()
        .map(|_| build_fetch_client(&fetch_plan))
        .transpose()?
        .unwrap_or_else(|| HTTP_CLIENT.clone());

    // SABnzbd exposes its API at /api (default) or /sabnzbd/api (when configured
    // with a URL base prefix). Try /api first, fall back to /sabnzbd/api.
    let candidates = [
        format!(
            "{}/api?mode=get_config&output=json&apikey={}",
            base_url, req.api_key
        ),
        format!(
            "{}/sabnzbd/api?mode=get_config&output=json&apikey={}",
            base_url, req.api_key
        ),
    ];

    let mut last_err = String::new();
    for url in &candidates {
        let resp = match client.get(url).send().await {
            Ok(r) => r,
            Err(e) => {
                last_err = format!("Failed to connect to SABnzbd: {e}");
                continue;
            }
        };

        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            last_err = format!("SABnzbd API not found at {url}");
            continue;
        }

        if !resp.status().is_success() {
            return Err(ApiError::from(anyhow::anyhow!(
                "SABnzbd returned HTTP {} — check your API key",
                resp.status()
            )));
        }

        let json: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| ApiError::from(anyhow::anyhow!("Invalid JSON from SABnzbd: {e}")))?;

        let preview = sabnzbd_import::parse_sabnzbd_api_response(&json);
        return Ok(Json(preview));
    }

    Err(ApiError::from(anyhow::anyhow!(
        "Could not reach SABnzbd API — {last_err}"
    )))
}

/// Apply an import preview — writes servers, categories, general settings to config.
pub async fn h_setup_apply(
    State(state): State<Arc<AppState>>,
    Json(preview): Json<sabnzbd_import::SabnzbdImportPreview>,
) -> Result<impl IntoResponse, ApiError> {
    // Reject any server with a masked password
    for server in &preview.servers {
        if server.password_masked {
            return Err(ApiError::bad_request(
                "cannot apply: one or more servers have masked passwords (***)",
            ));
        }
    }

    let mut config = (*state.config()).clone();

    // Convert imported servers → ServerConfig with fresh UUIDs
    config.servers = preview
        .servers
        .iter()
        .map(|s| s.to_server_config())
        .collect();

    // Replace categories
    if !preview.categories.is_empty() {
        config.categories = preview.categories;
    }

    // Apply general settings
    if let Some(ref key) = preview.general.api_key {
        config.general.api_key = Some(key.clone());
    }
    if let Some(ref dir) = preview.general.complete_dir {
        config.general.complete_dir = std::path::PathBuf::from(dir);
    }
    if let Some(ref dir) = preview.general.incomplete_dir {
        config.general.incomplete_dir = std::path::PathBuf::from(dir);
    }
    if preview.general.speed_limit_bps > 0 {
        config.general.speed_limit_bps = preview.general.speed_limit_bps;
    }

    // Apply RSS feeds
    if !preview.rss_feeds.is_empty() {
        config.rss_feeds = preview.rss_feeds;
    }

    // Persist to disk + update in-memory config
    state
        .update_config(config.clone())
        .map_err(|e| ApiError::from(anyhow::anyhow!("Failed to save config: {e}")))?;

    // Update runtime state
    state.queue_manager.update_servers(config.servers.clone());
    state
        .queue_manager
        .set_categories(config.categories.clone());

    if config.general.speed_limit_bps > 0 {
        state
            .queue_manager
            .set_speed_limit(config.general.speed_limit_bps);
    }

    Ok(Json(serde_json::json!({ "status": true })))
}

// ---------------------------------------------------------------------------
// WebDAV media library handlers
// ---------------------------------------------------------------------------

#[cfg(feature = "webdav")]
#[derive(Deserialize)]
pub struct DavAddQuery {
    pub id: String,
}

#[cfg(feature = "webdav")]
#[derive(Serialize)]
pub struct DavAddResponse {
    pub status: bool,
    pub dav_id: String,
}

// ── DAV pipeline status types ──────────────────────────────────────────────

#[cfg(feature = "webdav")]
#[derive(Serialize)]
pub struct DavQueueEntry {
    pub job_name: String,
    pub queued_at: String,
}

#[cfg(feature = "webdav")]
#[derive(Serialize)]
pub struct DavHistoryEntry {
    pub job_name: String,
    pub status: String,
    pub fail_message: Option<String>,
    pub completed_at: String,
}

#[cfg(feature = "webdav")]
#[derive(Serialize)]
pub struct DavStatusResponse {
    pub queue: Vec<DavQueueEntry>,
    pub history: Vec<DavHistoryEntry>,
}

/// GET /api/dav/status — pipeline queue + history for media library status overlay.
#[cfg(feature = "webdav")]
pub async fn h_dav_status(
    axum::Extension(dav): axum::Extension<Option<Arc<crate::dav::DavHandle>>>,
) -> Result<Json<DavStatusResponse>, ApiError> {
    let dav = dav
        .as_ref()
        .ok_or_else(|| ApiError::from(anyhow::anyhow!("WebDAV library not initialised")))?;

    let status = dav
        .pipeline_status()
        .await
        .map_err(|e| ApiError::from(anyhow::anyhow!("DAV status query failed: {e}")))?;

    use nzbdav_core::models::DownloadStatus;

    let queue = status
        .queue
        .into_iter()
        .map(|q| DavQueueEntry {
            job_name: q.job_name,
            queued_at: q.created_at.and_utc().to_rfc3339(),
        })
        .collect();

    let history = status
        .history
        .into_iter()
        .map(|h| DavHistoryEntry {
            job_name: h.job_name,
            status: match h.download_status {
                DownloadStatus::Completed => "completed".into(),
                DownloadStatus::Failed => "failed".into(),
            },
            fail_message: h.fail_message,
            completed_at: h.created_at.and_utc().to_rfc3339(),
        })
        .collect();

    Ok(Json(DavStatusResponse { queue, history }))
}

/// POST /api/dav/add?id=<history-id>
/// Feeds a completed download's NZB into the WebDAV streaming pipeline.
/// The item must exist in history (completed or failed) with NZB data retained.
#[cfg(feature = "webdav")]
pub async fn h_dav_add(
    State(state): State<Arc<AppState>>,
    axum::Extension(dav): axum::Extension<Option<Arc<crate::dav::DavHandle>>>,
    Query(q): Query<DavAddQuery>,
) -> Result<Json<DavAddResponse>, ApiError> {
    let dav = dav
        .as_ref()
        .ok_or_else(|| ApiError::from(anyhow::anyhow!("WebDAV library not initialised")))?;
    let qm = &state.queue_manager;

    let entry = qm
        .history_get(&q.id)
        .map_err(ApiError::from)?
        .ok_or_else(|| ApiError::not_found("history item not found"))?;

    let nzb_data = qm
        .history_get_nzb_data(&q.id)
        .map_err(ApiError::from)?
        .ok_or_else(|| ApiError::not_found("NZB data not retained for this item"))?;

    let file_name: String = entry.name.clone();
    let job_name = file_name.trim_end_matches(".nzb").to_string();

    let dav_id = dav
        .enqueue_nzb(&file_name, &job_name, &nzb_data)
        .await
        .map_err(|e| ApiError::from(anyhow::anyhow!("DAV enqueue failed: {e}")))?;

    Ok(Json(DavAddResponse {
        status: true,
        dav_id: dav_id.to_string(),
    }))
}

// ---------------------------------------------------------------------------
// DAV config handlers
// ---------------------------------------------------------------------------

/// GET /api/config/dav -- Get DAV auto-send configuration.
#[cfg(feature = "webdav")]
pub async fn h_dav_config_get(
    State(state): State<Arc<AppState>>,
) -> Result<Json<DavConfig>, ApiError> {
    Ok(Json(state.config().dav.clone()))
}

/// PUT /api/config/dav -- Update DAV auto-send configuration.
///
/// When `auto_send_all` is true the `category_rules` list is cleared — the two
/// modes are mutually exclusive.
#[cfg(feature = "webdav")]
pub async fn h_dav_config_set(
    State(state): State<Arc<AppState>>,
    Json(mut body): Json<DavConfig>,
) -> Result<Json<serde_json::Value>, ApiError> {
    if body.auto_send_all {
        body.category_rules.clear();
    }
    let mut config = (*state.config()).clone();
    config.dav = body;
    state.update_config(config).map_err(ApiError::from)?;
    Ok(Json(serde_json::json!({ "status": true })))
}

#[cfg(test)]
mod tests {
    use super::{
        MAX_NZB_DECOMPRESSED_BYTES, extract_nzbs, sanitize_server_config, validate_fetch_url,
    };
    use std::io::Write;

    use nzb_web::nzb_core::config::ServerConfig;
    use zip::CompressionMethod;
    use zip::write::SimpleFileOptions;

    fn build_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let cursor = std::io::Cursor::new(Vec::new());
        let mut writer = zip::ZipWriter::new(cursor);
        let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);

        for (name, contents) in entries {
            writer.start_file(name, options).unwrap();
            writer.write_all(contents).unwrap();
        }

        writer.finish().unwrap().into_inner()
    }

    #[tokio::test]
    async fn validate_fetch_url_rejects_private_ip_literals() {
        let err = validate_fetch_url("http://127.0.0.1/file.nzb")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("private/reserved"));
    }

    #[tokio::test]
    async fn validate_fetch_url_rejects_localhost_hostname() {
        let err = validate_fetch_url("http://localhost/file.nzb")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("private/reserved"));
    }

    #[test]
    fn extract_nzbs_rejects_zip_bombs() {
        let oversized = vec![b'x'; (MAX_NZB_DECOMPRESSED_BYTES + 1) as usize];
        let zip = build_zip(&[("oversized.nzb", oversized.as_slice())]);
        let err = extract_nzbs("oversized.zip", &zip).unwrap_err();
        assert!(err.to_string().contains("100 MB limit"));
    }

    #[test]
    fn extract_nzbs_accepts_small_zip_nzb() {
        let zip = build_zip(&[("sample.nzb", br#"<nzb><file subject="ok" /></nzb>"#)]);
        let nzbs = extract_nzbs("sample.zip", &zip).unwrap();
        assert_eq!(nzbs.len(), 1);
        assert_eq!(nzbs[0].0, "sample.nzb");
        assert_eq!(nzbs[0].1, br#"<nzb><file subject="ok" /></nzb>"#);
    }

    #[test]
    fn sanitize_server_config_trims_string_fields() {
        let mut server = ServerConfig::new("srv-1", " news.example.com \n");
        server.name = " Primary ".into();
        server.username = Some(" user ".into());
        server.password = Some(" pass ".into());
        server.proxy_url = Some(" socks5://proxy ".into());
        server.trusted_fingerprint = Some(" abc123 ".into());

        sanitize_server_config(&mut server);

        assert_eq!(server.host, "news.example.com");
        assert_eq!(server.name, "Primary");
        assert_eq!(server.username.as_deref(), Some("user"));
        assert_eq!(server.password.as_deref(), Some("pass"));
        assert_eq!(server.proxy_url.as_deref(), Some("socks5://proxy"));
        assert_eq!(server.trusted_fingerprint.as_deref(), Some("abc123"));
    }
}
