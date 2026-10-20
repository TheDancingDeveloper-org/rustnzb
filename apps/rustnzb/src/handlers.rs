use std::sync::Arc;

use axum::Json;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Multipart, Path, Query, State};
use axum::http::HeaderMap;
use axum::response::IntoResponse;
use http::StatusCode;
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Shared HTTP client — connection-pooling; created once, reused across requests
// ---------------------------------------------------------------------------

static HTTP_CLIENT: std::sync::LazyLock<reqwest::Client> = std::sync::LazyLock::new(|| {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("Failed to build shared HTTP client")
});

use chrono::Local;
use nzb_web::nzb_core::NzbError;
#[cfg(feature = "webdav")]
use nzb_web::nzb_core::config::DavConfig;
use nzb_web::nzb_core::config::{
    CategoryConfig, RssFeedConfig, ServerConfig, normalize_history_retention,
    normalize_rss_expiry_days,
};
use nzb_web::nzb_core::models::*;
use nzb_web::nzb_core::nzb_parser;
use nzb_web::nzb_core::sabnzbd_import;
use nzb_web::nzb_core::schedule::{ScheduleRule, evaluate_config};
use nzb_web::nzb_core::{HistoryFilter, HistorySummary};
use nzb_web::queue_manager::PauseReason;

use nzb_web::HistoryRetryOutcome;
use nzb_web::error::ApiError;
use nzb_web::fetch_guard::{
    FetchPolicy, MAX_FETCH_BODY_BYTES, build_fetch_client, check_fetch_url_allowed,
    read_response_bytes_limited, validate_fetch_url_with,
};
use nzb_web::log_buffer::LogEntry;
use nzb_web::nzb_archive::extract_nzbs;
use nzb_web::state::AppState;

use crate::admissions::{IdempotencyKey, payload_digest};

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
    /// Page size (default 50).
    pub limit: Option<usize>,
    /// Number of matching entries to skip (default 0).
    pub offset: Option<usize>,
    /// Only `completed` or `failed` entries.
    pub status: Option<String>,
    /// Only entries in this category.
    pub category: Option<String>,
    /// Case-insensitive substring of the job name.
    pub search: Option<String>,
    /// Only entries completed within the last N days. Also bounds `stats`.
    /// Values above [`MAX_HISTORY_DAYS`] are rejected: subtracting the span
    /// from `chrono::Utc::now()` overflows chrono's DateTime range (about
    /// ±262 000 years), which panics.
    pub days: Option<u32>,
    /// Include the detail-only `stages` and `server_stats` arrays on every
    /// row. Off by default: they are most of the bytes on a page, the history
    /// table never renders them, and the expanded row fetches them from
    /// `GET /api/history/{id}`. Tools that summarise a run in one request can
    /// ask for them here.
    #[serde(default)]
    pub detail: bool,
}

/// Largest `days` window `GET /api/history` accepts (about 100 years).
const MAX_HISTORY_DAYS: u32 = 36_500;

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

#[derive(Deserialize)]
pub struct SortQueueBody {
    /// Field to sort by: `remaining` (default), `name`, `size`, `age` or
    /// `priority`.
    #[serde(default)]
    pub sort_by: Option<String>,
    /// `asc` or `desc`. Takes precedence over `ascending` when present.
    #[serde(default)]
    pub direction: Option<String>,
    /// Legacy direction flag, used when `direction` is absent.
    #[serde(default = "default_sort_ascending")]
    pub ascending: bool,
}

fn default_sort_ascending() -> bool {
    true
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
    pub jobs: Vec<QueueJob>,
    pub total: usize,
    pub speed_bps: u64,
    pub paused: bool,
    /// Why downloads are globally paused (`global`, `disk_space`), or null.
    pub pause_reason: Option<PauseReason>,
}

/// A queued job plus why it is paused (`manual`, `global`, `disk_space`,
/// `server`), or null when it is not paused.
#[derive(Serialize)]
pub struct QueueJob {
    #[serde(flatten)]
    pub job: NzbJob,
    pub pause_reason: Option<PauseReason>,
}

#[derive(Serialize)]
pub struct HistoryResponse {
    pub entries: Vec<HistoryResponseEntry>,
    /// Number of entries matching the filters (across all pages).
    pub total: usize,
    pub offset: usize,
    pub limit: usize,
    /// Every category present in history, sorted.
    pub categories: Vec<String>,
    /// Aggregates over the `days` window (ignores status/category/search).
    pub stats: HistoryStats,
}

#[derive(Serialize, Default)]
pub struct HistoryStats {
    pub completed: usize,
    pub completed_bytes: u64,
    pub failed: usize,
    /// Completed share of completed + failed, rounded to a whole percent.
    pub success_pct: u32,
    /// Mean wall-clock time from add to completion of completed jobs.
    pub avg_duration_secs: Option<f64>,
    /// The two most common failure reasons.
    pub fail_reasons: Vec<HistoryFailReason>,
}

#[derive(Serialize)]
pub struct HistoryFailReason {
    pub reason: String,
    pub count: usize,
}

impl HistoryStats {
    fn from_entries<'a>(entries: impl Iterator<Item = &'a HistorySummary>) -> Self {
        let mut stats = Self::default();
        let mut duration_total = 0.0;
        let mut reasons: Vec<HistoryFailReason> = Vec::new();
        for e in entries {
            match e.status {
                JobStatus::Completed => {
                    stats.completed += 1;
                    stats.completed_bytes += e.total_bytes;
                    duration_total +=
                        (e.completed_at - e.added_at).num_milliseconds() as f64 / 1000.0;
                }
                JobStatus::Failed => {
                    stats.failed += 1;
                    // First clause of the message, as a short grouping key.
                    let msg = e.error_message.as_deref().unwrap_or("unknown");
                    let reason: String = msg
                        .split(['.', ':'])
                        .next()
                        .unwrap_or_default()
                        .chars()
                        .take(32)
                        .collect();
                    match reasons.iter_mut().find(|r| r.reason == reason) {
                        Some(r) => r.count += 1,
                        None => reasons.push(HistoryFailReason { reason, count: 1 }),
                    }
                }
                _ => {}
            }
        }
        let finished = stats.completed + stats.failed;
        if finished > 0 {
            stats.success_pct = ((stats.completed as f64 / finished as f64) * 100.0).round() as u32;
        }
        if stats.completed > 0 {
            stats.avg_duration_secs = Some(duration_total / stats.completed as f64);
        }
        // Stable sort keeps first-seen order among equal counts.
        reasons.sort_by_key(|r| std::cmp::Reverse(r.count));
        reasons.truncate(2);
        stats.fail_reasons = reasons;
        stats
    }
}

/// One history row.
///
/// A list response (`GET /api/history`) omits the detail-only `stages` and
/// `server_stats` arrays: between them they are most of the bytes on a page
/// and the table never renders either. `GET /api/history/{id}` returns them,
/// and that is what the expanded row loads. Both routes share this shape so a
/// client can parse a list entry and a detail entry with one type; the two
/// fields are simply empty on list rows.
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
    pub failure_code: Option<JobFailureCode>,
    pub server_stats: Vec<ServerArticleStats>,
    pub has_nzb_data: bool,
    pub duration_secs: f64,
    pub average_speed_bps: u64,
    /// Every article the entry's server stats account for. Computed when the
    /// row is built, so a list row can report availability without shipping
    /// the per-server breakdown.
    pub articles_served: usize,
    pub articles_missing: usize,
}

impl HistoryResponseEntry {
    /// A list row: the table's columns plus the precomputed article totals,
    /// without the per-server and per-stage detail only the expanded row
    /// shows. Those are dropped after the conversion, which is what sums them.
    fn list_row(e: HistoryEntry) -> Self {
        let mut row = Self::from(e);
        row.stages = Vec::new();
        row.server_stats = Vec::new();
        row
    }
}

impl From<HistoryEntry> for HistoryResponseEntry {
    /// The full row, including detail-only fields: `GET /api/history/{id}`.
    fn from(e: HistoryEntry) -> Self {
        let has_nzb = e.nzb_data.is_some();
        let duration_secs = e.download_time_secs.unwrap_or_else(|| {
            (e.completed_at - e.added_at).num_milliseconds().max(0) as f64 / 1000.0
        });
        let average_speed_bps = if duration_secs > 0.0 {
            (e.downloaded_bytes as f64 / duration_secs) as u64
        } else {
            0
        };
        let articles_served = e.server_stats.iter().map(|s| s.articles_downloaded).sum();
        let articles_missing = e.server_stats.iter().map(|s| s.articles_failed).sum();
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
            failure_code: e.failure_code,
            server_stats: e.server_stats,
            has_nzb_data: has_nzb,
            duration_secs,
            average_speed_bps,
            articles_served,
            articles_missing,
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
    /// Why downloads are globally paused (`global`, `disk_space`), or null.
    pub pause_reason: Option<PauseReason>,
    pub speed_bps: u64,
    pub speed_limit_bps: u64,
    pub queue_size: usize,
    pub post_processing: PostProcessingStatus,
    pub disk_space_free: u64,
    /// Total filesystem capacity, 0 if unknown (see `get_disk_space_total`).
    pub disk_space_total: u64,
    pub min_free_space_bytes: u64,
    pub pause_remaining_secs: Option<i64>,
    pub webdav_available: bool,
    pub webdav_enabled: bool,
    pub nntp_connections: Vec<NntpConnectionStatus>,
}

#[derive(Serialize)]
pub struct PostProcessingStatus {
    pub active_jobs: usize,
    pub active_repairs: usize,
    pub active_extractions: usize,
    pub peak_jobs: usize,
    pub peak_repairs: usize,
    pub peak_extractions: usize,
    pub max_jobs: usize,
    pub max_repairs: usize,
    pub max_extractions: usize,
}

#[derive(Serialize)]
pub struct NntpConnectionStatus {
    pub server_id: String,
    pub connected: usize,
    pub limit: usize,
}

#[derive(Serialize)]
pub struct SimpleResponse {
    pub status: bool,
}

/// The credential used by Sonarr, Radarr, Lidarr, and other SABnzbd-compatible
/// clients to call `/sabnzbd/api`.
#[derive(Serialize)]
pub struct SabApiKeyResponse {
    pub api_key: Option<String>,
}

#[derive(Serialize)]
pub struct LogResponse {
    pub entries: Vec<LogEntry>,
    pub latest_seq: u64,
    /// Live log buffer identity (changes on server restart, when `seq`
    /// restarts at 0). Absent for persisted per-job history logs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub boot_id: Option<String>,
}

// ---------------------------------------------------------------------------
// Queue handlers
// ---------------------------------------------------------------------------

/// GET /api/queue -- List jobs that are still active in the download queue.
pub async fn h_queue_list(
    State(state): State<Arc<AppState>>,
    Query(q): Query<QueueQuery>,
) -> Result<Json<QueueResponse>, ApiError> {
    let qm = &state.queue_manager;
    let all_jobs = qm.get_active_jobs();
    let total = all_jobs.len();
    let speed_bps = qm.get_speed();
    let paused = qm.is_paused();
    let pause_reason = qm.global_pause_reason();

    // Apply pagination (default: first 100 jobs)
    let offset = q.offset.unwrap_or(0);
    let limit = q.limit.unwrap_or(100);
    let jobs: Vec<_> = all_jobs
        .into_iter()
        .skip(offset)
        .take(limit)
        .map(|job| QueueJob {
            pause_reason: qm.pause_reason(&job.id),
            job: mask_job_password(job),
        })
        .collect();

    Ok(Json(QueueResponse {
        jobs,
        total,
        speed_bps,
        paused,
        pause_reason,
    }))
}

/// A parsed `POST /api/queue/add` multipart body.
#[derive(Default)]
struct UploadForm {
    files: Vec<(String, Vec<u8>)>,
    /// The optional archive `password` text field.
    password: Option<String>,
    category: Option<String>,
    priority: Option<i32>,
    name: Option<String>,
}

impl UploadForm {
    /// The query options with any form fields laid over them.
    fn options(&self, q: &AddNzbQuery) -> AddNzbQuery {
        AddNzbQuery {
            category: self.category.clone().or_else(|| q.category.clone()),
            priority: self.priority.or(q.priority),
            name: self.name.clone().or_else(|| q.name.clone()),
        }
    }
}

/// Read a whole `POST /api/queue/add` body. Parts that carry a file name
/// (or are named `file`) are NZB uploads; text fields `password`, `category` (alias `cat`), `priority`
/// and `name` (alias `nzbname`) set job options, in any order relative to
/// the files. Other text fields are ignored.
async fn read_upload(multipart: &mut Multipart) -> Result<UploadForm, ApiError> {
    let mut form = UploadForm::default();
    while let Some(field) = multipart.next_field().await.map_err(ApiError::from)? {
        let key = field.name().unwrap_or_default().to_string();
        // A part with a file name is an upload. So is a bare `file` part
        // (curl's `-F "file=<x.nzb"` sends the contents without a name).
        let file_name = match field.file_name() {
            Some(file_name) => Some(file_name.to_string()),
            None if key == "file" => Some("unknown.nzb".to_string()),
            None => None,
        };
        if let Some(file_name) = file_name {
            let data = field.bytes().await.map_err(ApiError::from)?;
            form.files.push((file_name, data.to_vec()));
            continue;
        }
        let text = field.text().await.map_err(ApiError::from)?;
        match key.as_str() {
            "password" => form.password = Some(text),
            "category" | "cat" => form.category = Some(text),
            "name" | "nzbname" => form.name = Some(text),
            "priority" => {
                form.priority = Some(
                    text.trim()
                        .parse()
                        .map_err(|_| ApiError::bad_request("priority must be an integer"))?,
                )
            }
            _ => {}
        }
    }
    Ok(form)
}

/// Replace a job's archive password with [`PASSWORD_MASK`] (or `""` when it
/// has none) for the native API, as server passwords are masked in config
/// responses. The SABnzbd layer keeps the real value: its queue slot
/// contract includes `password`. No native endpoint accepts a job password
/// back, so the mask can never be stored in place of the real one.
fn mask_job_password(mut job: NzbJob) -> NzbJob {
    job.password = Some(if job.password.is_some_and(|pw| !pw.is_empty()) {
        PASSWORD_MASK.to_string()
    } else {
        String::new()
    });
    job
}

/// An archive that cannot be unpacked into NZBs (corrupt, empty, over the
/// size or count limits) is bad input, not a server fault.
fn bad_upload(error: anyhow::Error) -> ApiError {
    ApiError::from((StatusCode::BAD_REQUEST, error))
}

/// Split SABnzbd's inline job password (`name{{pw}}`, `name/pw`) off a
/// name; an empty inline password counts as none.
fn take_inline_password(raw: &str) -> (String, Option<String>) {
    match nzb_web::nzb_core::path::split_job_password(raw) {
        Some((name, password)) => (name, Some(password).filter(|pw| !pw.is_empty())),
        None => (raw.to_string(), None),
    }
}

/// Enqueue a single NZB from raw bytes, applying the category, priority and
/// name override from `q`. The archive password is the explicit `password` if given,
/// else an inline one from the `name` override or the file name (SABnzbd's
/// `name{{password}}` / `name/password` convention), else the NZB's own
/// `<meta type="password">`.
fn enqueue_nzb(
    state: &AppState,
    q: &AddNzbQuery,
    file_name: &str,
    data: Vec<u8>,
    password: Option<&str>,
    idempotency_key: Option<&IdempotencyKey>,
) -> Result<String, ApiError> {
    // Split inline passwords off before parse_nzb sanitizes the name.
    let (name_override, name_password) = match q.name.as_deref() {
        Some(name) => {
            let (name, password) = take_inline_password(name);
            (Some(name), password)
        }
        None => (None, None),
    };
    let (file_name, file_password) = take_inline_password(file_name);
    let name = name_override.unwrap_or_else(|| {
        file_name
            .strip_suffix(".nzb")
            .unwrap_or(&file_name)
            .to_string()
    });

    let mut job = nzb_parser::parse_nzb(&name, &data).map_err(ApiError::from)?;
    if let Some(password) = password
        .filter(|pw| !pw.is_empty())
        .map(str::to_string)
        .or(name_password)
        .or(file_password)
    {
        job.password = Some(password);
    }

    if let Some(ref cat) = q.category {
        job.category = cat.clone();
    }
    if let Some(prio) = q.priority {
        job.priority = priority_from_i32(prio);
    }

    let qm = &state.queue_manager;
    job.work_dir = qm.incomplete_dir().join(&job.id);
    job.output_dir = qm
        .output_dir_for(&job.category, &job.name)
        .map_err(ApiError::from)?;

    if let Some(idempotency_key) = idempotency_key {
        let digest = payload_digest(&data);
        let outcome = qm
            .add_job_idempotent(job, data, idempotency_key.as_str(), &digest)
            .map_err(|error| match error {
                nzb_web::nzb_core::NzbError::AdmissionConflict => ApiError::admission_conflict(),
                error => ApiError::from(error),
            })?;
        return Ok(match outcome {
            QueueAdmissionOutcome::Inserted(admission)
            | QueueAdmissionOutcome::Existing(admission) => admission.job_id,
        });
    }

    let id = job.id.clone();
    qm.add_job(job, Some(data)).map_err(ApiError::from)?;
    Ok(id)
}

/// POST /api/queue/add -- Add NZB file(s) to the queue.
/// Accepts `.nzb` files directly, or `.zip`/`.gz`/`.bz2` archives containing `.nzb` files.
/// Multiple files can be uploaded in a single multipart request.
/// `category`/`cat`, `priority`, `name`/`nzbname` and `password` may be sent
/// as multipart text fields or (except `password`) query parameters; a form
/// field wins over the query parameter of the same name.
pub async fn h_queue_add(
    State(state): State<Arc<AppState>>,
    Query(q): Query<AddNzbQuery>,
    headers: HeaderMap,
    mut multipart: Multipart,
) -> Result<impl IntoResponse, ApiError> {
    let idempotency_key = IdempotencyKey::from_headers(&headers)?;
    let mut nzo_ids = Vec::new();
    if let Some(idempotency_key) = idempotency_key.as_ref() {
        // A keyed admission binds exactly one payload, so the request must carry
        // exactly one NZB — a multi-NZB upload has no single job to replay.
        let form = read_upload(&mut multipart).await?;
        let q = form.options(&q);
        let mut uploaded_nzbs = Vec::new();
        for (file_name, data) in &form.files {
            uploaded_nzbs.extend(extract_nzbs(file_name, data).map_err(bad_upload)?);
        }
        if uploaded_nzbs.len() != 1 {
            return Err(ApiError::bad_request(
                "Idempotency-Key requires exactly one NZB payload",
            ));
        }
        let (nzb_name, nzb_data) = uploaded_nzbs.pop().expect("exactly one NZB payload");
        nzo_ids.push(enqueue_nzb(
            &state,
            &q,
            &nzb_name,
            nzb_data,
            form.password.as_deref(),
            Some(idempotency_key),
        )?);
    } else {
        let form = read_upload(&mut multipart).await?;
        let q = form.options(&q);
        for (file_name, data) in &form.files {
            // Extract NZBs (handles zip/gz/bz2 archives or plain .nzb)
            for (nzb_name, nzb_data) in extract_nzbs(file_name, data).map_err(bad_upload)? {
                nzo_ids.push(enqueue_nzb(
                    &state,
                    &q,
                    &nzb_name,
                    nzb_data,
                    form.password.as_deref(),
                    None,
                )?);
            }
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
        _ => {
            return Err(ApiError::bad_request(
                "Invalid priority value (expected 0-3)",
            ));
        }
    };
    state
        .queue_manager
        .set_job_priority(&id, priority)
        .map_err(ApiError::from)?;
    Ok(Json(SimpleResponse { status: true }))
}

/// POST /api/queue/sort -- Stable sort by `sort_by` (default: remaining
/// work percentage) in `direction` (or the legacy `ascending` flag).
pub async fn h_queue_sort(
    State(state): State<Arc<AppState>>,
    Json(body): Json<SortQueueBody>,
) -> Result<Json<SimpleResponse>, ApiError> {
    let field = match body.sort_by.as_deref() {
        None => nzb_web::QueueSortField::Remaining,
        Some(value) => value.parse().map_err(|()| {
            ApiError::bad_request("sort_by must be one of: remaining, name, size, age, priority")
        })?,
    };
    let ascending = match body.direction.as_deref() {
        None => body.ascending,
        Some(value) if value.eq_ignore_ascii_case("asc") => true,
        Some(value) if value.eq_ignore_ascii_case("desc") => false,
        Some(_) => return Err(ApiError::bad_request("direction must be asc or desc")),
    };
    state.queue_manager.sort_queue(field, ascending);
    Ok(Json(SimpleResponse { status: true }))
}

// ---------------------------------------------------------------------------
// Add URL handler
// ---------------------------------------------------------------------------

/// SSRF policy for server-side fetches, from `general.fetch_allow_private`
/// and `general.fetch_allowed_hosts`.
fn fetch_policy(state: &AppState) -> FetchPolicy {
    FetchPolicy::from_config(&state.config().general)
}

/// Reject an RSS feed whose URL the fetch guard would refuse, so a blocked
/// feed is reported when it is saved instead of failing on every poll.
/// Reject a feed whose poll interval would hammer the indexer (and, at 0,
/// spin the RSS monitor).
fn validate_feed_poll_interval(feed: &RssFeedConfig) -> Result<(), ApiError> {
    let min = nzb_web::rss_monitor::MIN_POLL_INTERVAL_SECS;
    if feed.poll_interval_secs < min {
        return Err(ApiError::from((
            StatusCode::BAD_REQUEST,
            format!(
                "poll_interval_secs must be at least {min} (got {})",
                feed.poll_interval_secs
            ),
        )));
    }
    Ok(())
}

async fn validate_feed_url(state: &AppState, url: &str) -> Result<(), ApiError> {
    check_fetch_url_allowed(url, &fetch_policy(state))
        .await
        .map_err(|e| ApiError::url_rejected(format!("Feed URL rejected: {e}")))
}

#[derive(Deserialize)]
pub struct AddUrlBody {
    pub url: String,
    pub name: Option<String>,
    pub category: Option<String>,
    pub priority: Option<i32>,
    /// Archive password; overrides inline and NZB-embedded passwords.
    #[serde(default)]
    pub password: Option<String>,
}

/// POST /api/queue/add-url -- Add an NZB from a URL.
pub async fn h_queue_add_url(
    State(state): State<Arc<AppState>>,
    Json(body): Json<AddUrlBody>,
) -> Result<impl IntoResponse, ApiError> {
    if body.url.is_empty() {
        return Err(ApiError::bad_request("No URL provided"));
    }

    let fetch_plan = validate_fetch_url_with(&body.url, &fetch_policy(&state)).await?;
    tracing::info!(url = %body.url, "Fetching NZB from URL");

    let client = if fetch_plan.requires_pinned_client() {
        build_fetch_client(&fetch_plan)?
    } else {
        HTTP_CLIENT.clone()
    };
    let response = client
        .get(fetch_plan.url.clone())
        .send()
        .await
        .map_err(|e| ApiError::bad_gateway(format!("Failed to fetch URL: {e}")))?;

    if !response.status().is_success() {
        return Err(ApiError::bad_gateway(format!(
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

    let nzbs = extract_nzbs(&file_name, &data).map_err(bad_upload)?;
    let mut nzo_ids = Vec::new();
    for (nzb_name, nzb_data) in nzbs {
        let id = enqueue_nzb(
            &state,
            &q,
            &nzb_name,
            nzb_data,
            body.password.as_deref(),
            None,
        )?;
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
    if state.queue_manager.is_paused() {
        return Err(ApiError::from((
            StatusCode::CONFLICT,
            "Cannot resume an individual job while downloads are globally paused".to_string(),
        )));
    }
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
///
/// Filtering, paging, the category list and the window stats all run in SQL
/// on a blocking thread, so neither the history size nor the stored NZB blobs
/// cost the async runtime anything.
pub async fn h_history_list(
    State(state): State<Arc<AppState>>,
    Query(q): Query<HistoryQuery>,
) -> Result<Json<HistoryResponse>, ApiError> {
    // Reject an oversized window before touching the database: the filter
    // would otherwise be built and the full history loaded only to fail.
    if q.days.is_some_and(|d| d > MAX_HISTORY_DAYS) {
        return Err(ApiError::bad_request("days must be at most 36500"));
    }
    let limit = q.limit.unwrap_or(50);
    let offset = q.offset.unwrap_or(0);
    let completed_since = q
        .days
        .map(|d| chrono::Utc::now() - chrono::Duration::days(i64::from(d)));
    let window = HistoryFilter {
        completed_since,
        ..Default::default()
    };
    let status = q.status.as_deref().filter(|s| !s.is_empty() && *s != "all");
    // An unknown status matches nothing, as the string comparison it replaces did.
    let status_filter = status.map(parse_history_status);
    let filter = HistoryFilter {
        status: status_filter.flatten(),
        category: q.category.clone().filter(|c| !c.is_empty()),
        search: q
            .search
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned),
        ..window.clone()
    };
    let unknown_status = matches!(status_filter, Some(None));

    let (entries, total, categories, stats) = db_blocking(&state, move |db| {
        let categories = db.history_categories()?;
        let stats = HistoryStats::from_entries(db.history_summaries(&window)?.iter());
        let (entries, total) = if unknown_status {
            (Vec::new(), 0)
        } else {
            db.history_page(&filter, offset, limit)?
        };
        Ok((entries, total, categories, stats))
    })
    .await?;

    Ok(Json(HistoryResponse {
        entries: entries
            .into_iter()
            .map(|entry| {
                if q.detail {
                    entry.into()
                } else {
                    HistoryResponseEntry::list_row(entry)
                }
            })
            .collect(),
        total,
        offset,
        limit,
        categories,
        stats,
    }))
}

/// The terminal or in-flight status named by `s`, case-insensitively.
fn parse_history_status(s: &str) -> Option<JobStatus> {
    [
        JobStatus::Queued,
        JobStatus::Downloading,
        JobStatus::Paused,
        JobStatus::Verifying,
        JobStatus::Repairing,
        JobStatus::Extracting,
        JobStatus::PostProcessing,
        JobStatus::Completed,
        JobStatus::Failed,
    ]
    .into_iter()
    .find(|status| status.to_string().eq_ignore_ascii_case(s))
}

/// Run `query` against the database on a blocking thread, so the shared
/// database mutex is never waited on from an async worker.
async fn db_blocking<F, R>(state: &AppState, query: F) -> Result<R, ApiError>
where
    F: FnOnce(&nzb_web::nzb_core::Database) -> Result<R, NzbError> + Send + 'static,
    R: Send + 'static,
{
    let qm = state.queue_manager.clone();
    tokio::task::spawn_blocking(move || qm.with_db(query))
        .await
        .map_err(|e| ApiError::from(anyhow::anyhow!("Database task failed: {e}")))?
        .map_err(ApiError::from)
}

/// Run `f` with the queue manager on a blocking thread.
async fn qm_blocking<F, R>(state: &AppState, f: F) -> Result<R, ApiError>
where
    F: FnOnce(&Arc<nzb_web::queue_manager::QueueManager>) -> Result<R, NzbError> + Send + 'static,
    R: Send + 'static,
{
    let qm = state.queue_manager.clone();
    tokio::task::spawn_blocking(move || f(&qm))
        .await
        .map_err(|e| ApiError::from(anyhow::anyhow!("Database task failed: {e}")))?
        .map_err(ApiError::from)
}

/// GET /api/history/{id} -- Detailed information for one completed/failed job.
pub async fn h_history_get(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<HistoryResponseEntry>, ApiError> {
    let entry = qm_blocking(&state, move |qm| qm.history_get(&id))
        .await?
        .ok_or_else(|| ApiError::not_found("History entry not found"))?;
    Ok(Json(entry.into()))
}

/// How long a trashed history entry stays restorable. Long enough for the
/// toast that offers the undo to be read and acted on.
pub const HISTORY_UNDO_WINDOW: chrono::Duration = chrono::Duration::seconds(10);

/// Query for history deletions. A delete is a hard delete unless the caller
/// asks for the undo window with `trash=true`; SAB and *arr clients depend on
/// the old behaviour, and so does `clear all`.
#[derive(Deserialize, Default)]
pub struct HistoryDeleteQuery {
    #[serde(default)]
    pub trash: bool,
}

/// What a trashed deletion returns: the entries moved into the undo window
/// and when that window closes. A caller restores them with
/// `POST /api/history/restore`, so it must keep the ids.
#[derive(Serialize)]
pub struct HistoryTrashResponse {
    pub status: bool,
    /// Entries actually trashed, in request order.
    pub entries: Vec<TrashedHistoryEntry>,
    /// RFC 3339 instant after which a restore is refused and the next purge
    /// sweep removes the rows.
    pub purge_after: String,
}

#[derive(Serialize)]
#[serde(untagged)]
pub enum HistoryDeleteResponse {
    Trashed(HistoryTrashResponse),
    Removed(SimpleResponse),
}

/// The list-row shape for a trashed entry. It keeps the fields an undo needs
/// to put the row back in view, without loading its blobs.
#[derive(Serialize)]
pub struct TrashedHistoryEntry {
    pub id: String,
    pub name: String,
    pub status: JobStatus,
}

impl From<HistorySummary> for TrashedHistoryEntry {
    fn from(entry: HistorySummary) -> Self {
        Self {
            id: entry.id,
            name: entry.name,
            status: entry.status,
        }
    }
}

fn trash_response(
    entries: Vec<HistorySummary>,
    deadline: chrono::DateTime<chrono::Utc>,
) -> HistoryTrashResponse {
    HistoryTrashResponse {
        status: true,
        entries: entries.into_iter().map(Into::into).collect(),
        purge_after: deadline.to_rfc3339(),
    }
}

/// DELETE /api/history/{id} -- Remove a history entry.
///
/// `?trash=true` moves it into the undo window instead of deleting it, and
/// returns its summary plus `purge_after`.
pub async fn h_history_delete(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(q): Query<HistoryDeleteQuery>,
) -> Result<Json<HistoryDeleteResponse>, ApiError> {
    if !q.trash {
        qm_blocking(&state, move |qm| qm.history_remove(&id)).await?;
        return Ok(Json(HistoryDeleteResponse::Removed(SimpleResponse {
            status: true,
        })));
    }
    let deadline = chrono::Utc::now() + HISTORY_UNDO_WINDOW;
    let trashed = qm_blocking(&state, move |qm| qm.history_trash(&[id], deadline)).await?;
    if trashed.is_empty() {
        return Err(ApiError::not_found("History entry not found"));
    }
    Ok(Json(HistoryDeleteResponse::Trashed(trash_response(
        trashed, deadline,
    ))))
}

#[derive(Deserialize)]
pub struct HistoryRestoreBody {
    pub ids: Vec<String>,
}

#[derive(Serialize)]
pub struct HistoryRestoreResponse {
    pub status: bool,
    /// Ids brought back, in request order.
    pub restored: Vec<String>,
    /// One entry per id that could not be restored: unknown, already visible,
    /// or past its undo window.
    pub failures: Vec<BulkActionFailure>,
}

/// POST /api/history/restore -- Undo trashed history deletions.
///
/// Per-id like the bulk action: one id whose window has closed or that is
/// already visible does not stop the others from coming back.
pub async fn h_history_restore(
    State(state): State<Arc<AppState>>,
    Json(body): Json<HistoryRestoreBody>,
) -> Result<Json<HistoryRestoreResponse>, ApiError> {
    if body.ids.len() > MAX_HISTORY_BULK_IDS {
        return Err(ApiError::bad_request("too many ids in one bulk request"));
    }
    let ids = body.ids;
    let (restored, failures) = qm_blocking(&state, move |qm| {
        let mut restored = Vec::with_capacity(ids.len());
        let mut failures = Vec::new();
        for id in ids {
            match qm.history_restore(std::slice::from_ref(&id), chrono::Utc::now())? {
                ids if !ids.is_empty() => restored.extend(ids),
                _ => failures.push(BulkActionFailure {
                    id,
                    error: ApiError::not_found(
                        "History entry is not restorable (unknown, or its undo window has closed)",
                    ),
                }),
            }
        }
        Ok((restored, failures))
    })
    .await?;
    Ok(Json(HistoryRestoreResponse {
        status: failures.is_empty(),
        restored,
        failures,
    }))
}

/// DELETE /api/history -- Clear all history.
pub async fn h_history_clear(
    State(state): State<Arc<AppState>>,
) -> Result<Json<SimpleResponse>, ApiError> {
    qm_blocking(&state, |qm| qm.history_clear()).await?;
    Ok(Json(SimpleResponse { status: true }))
}

/// POST /api/history/{id}/retry -- Re-add a failed/completed NZB from history.
///
/// Idempotent per history entry: while a retry of `{id}` is still queued or
/// running, a repeat request enqueues nothing and returns `200` with the
/// existing job's id in `nzo_ids` (the same replay semantics as an
/// `Idempotency-Key` queue add). Once that job has left the queue the entry
/// can be retried again.
pub async fn h_history_retry(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    let lookup_id = id.clone();
    let entry = qm_blocking(&state, move |qm| qm.history_get(&lookup_id))
        .await?
        .ok_or(ApiError::not_found("History entry not found"))?;

    let job_id = match qm_blocking(&state, move |qm| qm.retry_history_entry(&entry)).await? {
        HistoryRetryOutcome::Started(job_id) => job_id,
        HistoryRetryOutcome::InProgress(job_id) => {
            tracing::info!(
                id = %job_id,
                original_id = %id,
                "History retry already in progress"
            );
            job_id
        }
        HistoryRetryOutcome::NoNzbData => {
            return Err(ApiError::from((
                StatusCode::CONFLICT,
                "No NZB data stored for this entry",
            )));
        }
    };

    Ok((
        StatusCode::OK,
        Json(AddNzbResponse {
            status: true,
            nzo_ids: vec![job_id],
        }),
    ))
}

// ---------------------------------------------------------------------------
// History bulk actions
// ---------------------------------------------------------------------------

/// Most ids one bulk request may name. The UI's "select all" is per page
/// (50); status-wide actions name no ids at all, so this only bounds a
/// hand-written client.
const MAX_HISTORY_BULK_IDS: usize = 5_000;

#[derive(Deserialize)]
pub struct HistoryBulkBody {
    /// `delete` or `retry`.
    pub action: String,
    /// Entries to act on. May be empty when `status` names the set.
    #[serde(default)]
    pub ids: Vec<String>,
    /// For `delete` only: also delete every entry with this status
    /// (`completed` or `failed`). This is "clear completed/failed".
    pub status: Option<String>,
    /// With `status`, narrow the set the same way `GET /api/history` does,
    /// so the action covers what the caller is looking at rather than all of
    /// history. Only meaningful alongside `status`.
    pub days: Option<u32>,
    pub category: Option<String>,
    pub search: Option<String>,
    /// With `delete`: move the entries into the undo window instead of
    /// deleting them, and return their ids and deadline in the response.
    #[serde(default)]
    pub trash: bool,
}

/// POST /api/history/bulk -- Delete or retry several history entries.
///
/// Mirrors `POST /api/queue/bulk`: one request, one result per id, and a
/// per-id failure rather than failing the whole batch. Deleting is
/// idempotent per id like `DELETE /api/history/{id}`, but a list entry must
/// still report an unknown id as a failure -- otherwise a selection the
/// server silently dropped looks like success.
pub async fn h_history_bulk(
    State(state): State<Arc<AppState>>,
    Json(body): Json<HistoryBulkBody>,
) -> Result<Json<HistoryBulkResponse>, ApiError> {
    let action = body.action.trim().to_ascii_lowercase();
    if !matches!(action.as_str(), "delete" | "retry") {
        return Err(ApiError::bad_request("action must be `delete` or `retry`"));
    }
    if body.ids.len() > MAX_HISTORY_BULK_IDS {
        return Err(ApiError::bad_request("too many ids in one bulk request"));
    }
    // History rows are terminal, so a status-wide delete only accepts the two
    // terminal statuses; anything else would match nothing and read as a
    // successful no-op.
    let status = match body
        .status
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        None => None,
        Some(raw) if raw.eq_ignore_ascii_case("completed") => Some(JobStatus::Completed),
        Some(raw) if raw.eq_ignore_ascii_case("failed") => Some(JobStatus::Failed),
        Some(_) => {
            return Err(ApiError::bad_request(
                "status must be `completed` or `failed`",
            ));
        }
    };
    if status.is_some() && action != "delete" {
        return Err(ApiError::bad_request(
            "status is only valid with the `delete` action",
        ));
    }
    if action == "retry"
        && (body.days.is_some() || body.category.is_some() || body.search.is_some())
    {
        return Err(ApiError::bad_request(
            "days, category and search are only valid with the `delete` action",
        ));
    }
    if body.days.is_some_and(|d| d > MAX_HISTORY_DAYS) {
        return Err(ApiError::bad_request("days must be at most 36500"));
    }
    if status.is_none() && body.ids.is_empty() {
        return Err(ApiError::bad_request("a bulk action needs ids or a status"));
    }

    let trash = body.trash && action == "delete";
    let deadline = chrono::Utc::now() + HISTORY_UNDO_WINDOW;
    let requested = body.ids;
    let filter = HistoryFilter {
        completed_since: body
            .days
            .map(|d| chrono::Utc::now() - chrono::Duration::days(i64::from(d))),
        status,
        category: body.category.filter(|c| !c.is_empty()),
        search: body
            .search
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty()),
    };
    let (succeeded, failures, trashed) = qm_blocking(&state, move |qm| {
        // The explicit ids first, then every entry the status names. An id
        // repeated across the two is acted on once.
        let mut seen = std::collections::HashSet::new();
        let mut targets: Vec<String> = Vec::with_capacity(requested.len());
        for id in requested {
            if seen.insert(id.clone()) {
                targets.push(id);
            }
        }
        if filter.status.is_some() {
            for summary in qm.history_summaries(&filter)? {
                if seen.insert(summary.id.clone()) {
                    targets.push(summary.id);
                }
            }
        }

        let mut succeeded = 0usize;
        let mut failures: Vec<BulkActionFailure> = Vec::new();
        let mut trashed: Vec<HistorySummary> = Vec::new();
        for id in targets {
            let error = if action == "delete" {
                // `history_remove` is idempotent, so look the entry up first:
                // a stale id in a client's selection is a failure to report.
                match qm.history_get(&id)? {
                    None => Some(ApiError::not_found("History entry not found")),
                    Some(_) if trash => {
                        match qm.history_trash(std::slice::from_ref(&id), deadline)? {
                            moved if moved.is_empty() => {
                                Some(ApiError::not_found("History entry not found"))
                            }
                            moved => {
                                trashed.extend(moved);
                                None
                            }
                        }
                    }
                    Some(_) => match qm.history_remove(&id) {
                        Ok(()) => None,
                        Err(e) => Some(ApiError::from(e)),
                    },
                }
            } else {
                match qm.history_get(&id)? {
                    None => Some(ApiError::not_found("History entry not found")),
                    Some(entry) => match qm.retry_history_entry(&entry)? {
                        HistoryRetryOutcome::Started(_) | HistoryRetryOutcome::InProgress(_) => {
                            None
                        }
                        HistoryRetryOutcome::NoNzbData => {
                            Some(ApiError::conflict("No NZB data stored for this entry"))
                        }
                    },
                }
            };
            match error {
                None => succeeded += 1,
                Some(error) => failures.push(BulkActionFailure { id, error }),
            }
        }
        Ok((succeeded, failures, trashed))
    })
    .await?;

    let trashed = trash.then(|| trash_response(trashed, deadline));
    Ok(Json(HistoryBulkResponse {
        status: failures.is_empty(),
        succeeded,
        failed: failures.len(),
        failures,
        trashed,
    }))
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
    let postproc = qm.postproc_resource_snapshot();
    Ok(Json(StatusResponse {
        version: env!("RUSTNZB_BUILD_VERSION"),
        paused: qm.is_paused(),
        pause_reason: qm.global_pause_reason(),
        speed_bps: qm.get_speed(),
        speed_limit_bps: qm.get_speed_limit(),
        queue_size: qm.queue_size(),
        post_processing: PostProcessingStatus {
            active_jobs: postproc.active_pipelines,
            active_repairs: postproc.active_repairs,
            active_extractions: postproc.active_extractions,
            peak_jobs: postproc.peak_pipelines,
            peak_repairs: postproc.peak_repairs,
            peak_extractions: postproc.peak_extractions,
            max_jobs: config.general.max_post_processing_jobs.max(1),
            max_repairs: config.general.max_repair_workers.max(1),
            max_extractions: config.general.max_extract_workers.max(1),
        },
        disk_space_free: get_disk_space_free(&config.general.complete_dir),
        disk_space_total: get_disk_space_total(&config.general.complete_dir),
        min_free_space_bytes: qm.min_free_space(),
        pause_remaining_secs: qm.pause_remaining_secs(),
        #[cfg(feature = "webdav")]
        webdav_available: true,
        #[cfg(not(feature = "webdav"))]
        webdav_available: false,
        #[cfg(feature = "webdav")]
        webdav_enabled: dav.is_some(),
        #[cfg(not(feature = "webdav"))]
        webdav_enabled: false,
        nntp_connections: qm
            .connected_snapshot()
            .into_iter()
            .map(|(server_id, connected, limit)| NntpConnectionStatus {
                server_id,
                connected,
                limit,
            })
            .collect(),
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
        boot_id: Some(state.log_buffer.boot_id().to_string()),
    }))
}

// ---------------------------------------------------------------------------
// Config handlers
// ---------------------------------------------------------------------------

/// GET /api/config -- Get current configuration.
///
/// Secrets are replaced with `********`: server passwords, `general.api_key`,
/// and the `user:pass@` part of server proxy URLs. The stored value is never
/// returned; an update that sends the mask back keeps it. The dead settings
/// `general.cache_size` and `general.log_file` are left out of the response.
pub async fn h_config_get(
    State(state): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let mut value =
        serde_json::to_value(masked_config(&state.config())).map_err(anyhow::Error::from)?;
    if let Some(general) = value.get_mut("general").and_then(|g| g.as_object_mut()) {
        general.remove("cache_size");
        general.remove("log_file");
    }
    Ok(Json(value))
}

/// GET /api/config/sabnzbd-api-key -- Return the current SABnzbd API key.
///
/// This route is protected by the native API authentication middleware. The
/// general config endpoint intentionally remains useful for regular settings,
/// while this dedicated route makes the sensitive-value access explicit.
pub async fn h_sab_api_key_get(
    State(state): State<Arc<AppState>>,
) -> Result<Json<SabApiKeyResponse>, ApiError> {
    Ok(Json(SabApiKeyResponse {
        api_key: state.config().general.api_key.clone(),
    }))
}

/// POST /api/config/sabnzbd-api-key/rotate -- Generate, persist, and return a
/// replacement SABnzbd API key. The previous key stops working immediately.
pub async fn h_sab_api_key_rotate(
    State(state): State<Arc<AppState>>,
) -> Result<Json<SabApiKeyResponse>, ApiError> {
    // UUID v4 is backed by the operating system's cryptographically secure
    // random source. A simple-form UUID supplies a 32-character token without
    // separators, matching the common SABnzbd API-key format.
    let api_key = uuid::Uuid::new_v4().simple().to_string();
    state.update_config_with(|config| {
        config.general.api_key = Some(api_key.clone());
        Ok::<_, ApiError>(())
    })?;
    tracing::info!("SABnzbd API key rotated");

    Ok(Json(SabApiKeyResponse {
        api_key: Some(api_key),
    }))
}

/// GET /api/config/servers -- List configured servers.
///
/// Passwords are replaced with `********`.
pub async fn h_servers_list(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Vec<ServerConfig>>, ApiError> {
    Ok(Json(masked_config(&state.config()).servers))
}

fn masked_config(
    config: &nzb_web::nzb_core::config::AppConfig,
) -> nzb_web::nzb_core::config::AppConfig {
    let mut config = config.clone();
    if config.general.api_key.is_some() {
        config.general.api_key = Some(PASSWORD_MASK.to_string());
    }
    if config.dav.password.is_some() {
        config.dav.password = Some(PASSWORD_MASK.to_string());
    }
    if config.dav.api_key.is_some() {
        config.dav.api_key = Some(PASSWORD_MASK.to_string());
    }
    for server in &mut config.servers {
        if server.password.is_some() {
            server.password = Some(PASSWORD_MASK.to_string());
        }
        if let Some(url) = server.proxy_url.as_deref() {
            server.proxy_url = Some(mask_proxy_userinfo(url));
        }
    }
    // Notification URLs often carry a secret in their path or query, and the
    // ntfy token is a credential. Mask any that are set.
    let notifications = &mut config.notifications;
    for secret in [
        &mut notifications.webhook_url,
        &mut notifications.apprise_api_url,
        &mut notifications.apprise_urls,
        &mut notifications.ntfy_token,
    ] {
        if !secret.is_empty() {
            *secret = PASSWORD_MASK.to_string();
        }
    }
    config
}

/// Replace the `user:pass@` part of a proxy URL with the mask. A URL without
/// userinfo is returned unchanged.
fn mask_proxy_userinfo(url: &str) -> String {
    let Some(scheme_end) = url.find("://").map(|i| i + 3) else {
        return url.to_string();
    };
    let rest = &url[scheme_end..];
    let authority_end = rest.find('/').unwrap_or(rest.len());
    let Some(at) = rest[..authority_end].rfind('@') else {
        return url.to_string();
    };
    format!("{}{PASSWORD_MASK}@{}", &url[..scheme_end], &rest[at + 1..])
}

/// Whether a submitted proxy URL is the masked form of the stored one, which
/// means "keep the stored URL" (it still carries the real credentials).
fn proxy_url_unchanged(submitted: Option<&str>, stored: Option<&str>) -> bool {
    match (submitted, stored) {
        (Some(s), Some(stored)) => s == mask_proxy_userinfo(stored),
        _ => false,
    }
}

#[cfg(feature = "webdav")]
/// Resolve a submitted secret field against the stored one. An absent key,
/// an empty string, or the mask keeps the stored value; JSON `null` clears
/// it; any other string replaces it.
fn resolve_secret(
    body: &serde_json::Map<String, serde_json::Value>,
    key: &str,
    stored: Option<String>,
) -> Option<String> {
    match body.get(key) {
        None => stored,
        Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::String(s)) if s.is_empty() || s == PASSWORD_MASK => stored,
        Some(serde_json::Value::String(s)) => Some(s.clone()),
        Some(_) => stored,
    }
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
    if server.password.as_deref() == Some(PASSWORD_MASK) {
        server.password = None;
    }
    // A new server has no stored URL to fall back on, so a masked one is dropped.
    if server
        .proxy_url
        .as_deref()
        .is_some_and(|u| u.contains(&format!("{PASSWORD_MASK}@")))
    {
        server.proxy_url = None;
    }

    state.update_config_with(|config| {
        if config.servers.iter().any(|s| s.id == server.id) {
            return Err(ApiError::conflict(format!(
                "Server '{}' already exists",
                server.id
            )));
        }
        config.servers.push(server);
        Ok::<_, ApiError>(())
    })?;
    // Apply the latest committed list, so concurrent writers converge.
    state
        .queue_manager
        .update_servers(state.config().servers.clone());

    Ok((StatusCode::OK, Json(SimpleResponse { status: true })))
}

/// Placeholder the API uses in place of a stored server password. A client
/// that sends it back (or sends an empty/absent password) on update means
/// "leave the password unchanged".
pub const PASSWORD_MASK: &str = "********";

/// Whether a submitted password means "keep the stored one".
fn password_unchanged(password: Option<&str>) -> bool {
    password.is_none_or(|p| p.is_empty() || p == PASSWORD_MASK)
}

/// Carry the stored password over when the client did not supply a new one.
fn keep_unchanged_password(server: &mut ServerConfig, existing: &ServerConfig) {
    if password_unchanged(server.password.as_deref()) {
        server.password = existing.password.clone();
    }
}

/// PUT /api/config/servers/{id} -- Update an existing server.
///
/// The body may be partial: fields it omits keep their stored values.
/// An empty or absent `password`, or the mask `********`, keeps the stored
/// password; JSON `null` clears it; any other value replaces it. A
/// `proxy_url` that is the masked form of the stored one keeps the stored URL.
pub async fn h_server_update(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(patch): Json<serde_json::Value>,
) -> Result<Json<SimpleResponse>, ApiError> {
    let clears_password = patch
        .get("password")
        .is_some_and(serde_json::Value::is_null);
    state.update_config_with(|config| {
        let idx = config
            .servers
            .iter()
            .position(|s| s.id == id)
            .ok_or_else(|| ApiError::from(NzbError::ServerNotFound(id.clone())))?;

        let stored = config.servers[idx].clone();
        let mut server = merge_server_update(&stored, patch)?;
        if clears_password {
            server.password = None;
        } else {
            keep_unchanged_password(&mut server, &stored);
        }
        if proxy_url_unchanged(server.proxy_url.as_deref(), stored.proxy_url.as_deref()) {
            server.proxy_url = stored.proxy_url.clone();
        }
        if server.id.is_empty() {
            server.id = id.clone();
        }
        sanitize_server_config(&mut server);
        config.servers[idx] = server;
        Ok::<_, ApiError>(())
    })?;
    // Apply the latest committed list, so concurrent writers converge.
    state
        .queue_manager
        .update_servers(state.config().servers.clone());

    Ok(Json(SimpleResponse { status: true }))
}

/// Overlay the fields present in `patch` onto `existing`.
fn merge_server_update(
    existing: &ServerConfig,
    patch: serde_json::Value,
) -> Result<ServerConfig, ApiError> {
    let serde_json::Value::Object(patch) = patch else {
        return Err(ApiError::bad_request("server update must be a JSON object"));
    };
    let mut merged = serde_json::to_value(existing).map_err(anyhow::Error::from)?;
    if let serde_json::Value::Object(fields) = &mut merged {
        fields.extend(patch);
    }
    serde_json::from_value(merged)
        .map_err(|e| ApiError::from((StatusCode::BAD_REQUEST, format!("Invalid server: {e}"))))
}

/// DELETE /api/config/servers/{id} -- Delete a server.
pub async fn h_server_delete(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<SimpleResponse>, ApiError> {
    state.update_config_with(|config| {
        let before = config.servers.len();
        config.servers.retain(|s| s.id != id);

        if config.servers.len() == before {
            return Err(ApiError::from(NzbError::ServerNotFound(id.clone())));
        }

        Ok::<_, ApiError>(())
    })?;
    // Apply the latest committed list, so concurrent writers converge.
    state
        .queue_manager
        .update_servers(state.config().servers.clone());

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
        .ok_or_else(|| ApiError::from(NzbError::ServerNotFound(id.clone())))?
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
///
/// When the body names an existing server and carries no new password (empty,
/// absent or the mask), the stored password is used, matching update.
pub async fn h_server_test_inline(
    State(state): State<Arc<AppState>>,
    Json(mut server): Json<ServerConfig>,
) -> Result<Json<ServerTestResponse>, ApiError> {
    if let Some(existing) = state.config().servers.iter().find(|s| s.id == server.id) {
        keep_unchanged_password(&mut server, existing);
        if proxy_url_unchanged(server.proxy_url.as_deref(), existing.proxy_url.as_deref()) {
            server.proxy_url = existing.proxy_url.clone();
        }
    } else if server.password.as_deref() == Some(PASSWORD_MASK) {
        server.password = None;
    }
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
    if state
        .queue_manager
        .history_get(&id)
        .map_err(ApiError::from)?
        .is_none()
    {
        return Err(ApiError::not_found("History entry not found"));
    }
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
        boot_id: None,
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
    cat.validate().map_err(ApiError::bad_request)?;
    state.update_config_with(|config| {
        if config.categories.iter().any(|c| c.name == cat.name) {
            return Err(ApiError::conflict(format!(
                "Category '{}' already exists",
                cat.name
            )));
        }
        config.categories.push(cat);
        state
            .queue_manager
            .set_categories(config.categories.clone());
        Ok::<_, ApiError>(())
    })?;
    Ok(Json(serde_json::json!({"status": true})))
}

/// PUT /api/config/categories/{name} -- Update a category.
pub async fn h_category_update(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    Json(cat): Json<CategoryConfig>,
) -> Result<Json<serde_json::Value>, ApiError> {
    cat.validate().map_err(ApiError::bad_request)?;
    state.update_config_with(|config| {
        let idx = config
            .categories
            .iter()
            .position(|c| c.name == name)
            .ok_or_else(|| ApiError::from(NzbError::CategoryNotFound(name.clone())))?;
        config.categories[idx] = cat;
        state
            .queue_manager
            .set_categories(config.categories.clone());
        Ok::<_, ApiError>(())
    })?;
    Ok(Json(serde_json::json!({"status": true})))
}

/// DELETE /api/config/categories/{name} -- Delete a category.
pub async fn h_category_delete(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    state.update_config_with(|config| {
        let initial_len = config.categories.len();
        config.categories.retain(|c| c.name != name);
        if config.categories.len() == initial_len {
            return Err(ApiError::from(NzbError::CategoryNotFound(name.clone())));
        }
        state
            .queue_manager
            .set_categories(config.categories.clone());
        Ok::<_, ApiError>(())
    })?;
    Ok(Json(serde_json::json!({"status": true})))
}

/// PUT /api/config/history-retention -- Update history retention setting.
pub async fn h_history_retention_set(
    State(state): State<Arc<AppState>>,
    Json(body): Json<HistoryRetentionBody>,
) -> Result<Json<SimpleResponse>, ApiError> {
    // 0 means "keep all" (GH #136); persist the normalized value so GET
    // reports what is actually enforced.
    let retention = normalize_history_retention(body.retention);
    state.update_config_with(|config| {
        config.general.history_retention = retention;
        Ok::<_, ApiError>(())
    })?;
    state.queue_manager.set_history_retention(retention);
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
    validate_max_active_downloads(body.max_active_downloads)?;
    state.update_config_with(|config| {
        config.general.max_active_downloads = body.max_active_downloads;
        Ok::<_, ApiError>(())
    })?;
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
    state.update_config_with(|config| {
        config.general.speed_limit_bps = body.speed_limit_bps;
        Ok::<_, ApiError>(())
    })?;
    Ok(Json(serde_json::json!({"status": true})))
}

// ---------------------------------------------------------------------------
// Download schedule
// ---------------------------------------------------------------------------

#[derive(Serialize)]
pub struct ScheduleResponse {
    /// The saved schedule, with every rule's id filled in.
    pub enabled: bool,
    pub rules: Vec<ScheduleRule>,
    /// Rules active at the moment of the call. Empty while the schedule is off.
    pub active_rule_ids: Vec<String>,
    /// True when a schedule rule is holding the queue paused right now. A
    /// window that is open while the disk guard holds the queue still reports
    /// `paused`, because the queue will not run until the window ends.
    pub paused: bool,
    /// The cap a rule is applying right now, if any. The configured
    /// `general.speed_limit_bps` applies when this is `null`.
    pub speed_limit_bps: Option<u64>,
    /// Servers a rule has out of rotation right now.
    pub disabled_servers: Vec<String>,
}

/// GET /api/config/schedule -- The schedule and what it is doing now.
pub async fn h_schedule_get(
    State(state): State<Arc<AppState>>,
) -> Result<Json<ScheduleResponse>, ApiError> {
    let config = state.config();
    let decision = evaluate_config(&config.schedule, Local::now().naive_local());
    Ok(Json(ScheduleResponse {
        enabled: config.schedule.enabled,
        rules: config.schedule.rules.clone(),
        active_rule_ids: decision.active_rule_ids,
        paused: decision.pause && state.queue_manager.is_paused(),
        speed_limit_bps: state.queue_manager.scheduled_speed_cap(),
        disabled_servers: state.queue_manager.schedule_disabled_servers(),
    }))
}

#[derive(Deserialize)]
pub struct ScheduleUpdate {
    pub enabled: bool,
    pub rules: Vec<ScheduleRule>,
}

/// PUT /api/config/schedule -- Replace the schedule.
///
/// Every rule is validated before anything is saved, so a bad rule never
/// half-applies. Rules without an id get one; ids stay stable across edits so
/// an active window survives a save.
pub async fn h_schedule_update(
    State(state): State<Arc<AppState>>,
    body: Result<Json<ScheduleUpdate>, JsonRejection>,
) -> Result<Json<ScheduleResponse>, ApiError> {
    let Json(body) =
        body.map_err(|rejection| ApiError::from((StatusCode::BAD_REQUEST, rejection.body_text())))?;

    let mut seen = std::collections::HashSet::new();
    for (i, rule) in body.rules.iter().enumerate() {
        if let Some(problem) = nzb_web::nzb_core::schedule::rule_problem(rule) {
            return Err(ApiError::from((
                StatusCode::BAD_REQUEST,
                format!("invalid `schedule.rules[{i}]`: {problem}"),
            )));
        }
        if !rule.id.trim().is_empty() && !seen.insert(rule.id.clone()) {
            return Err(ApiError::from((
                StatusCode::BAD_REQUEST,
                format!("invalid `schedule.rules[{i}]`: duplicate id `{}`", rule.id),
            )));
        }
    }
    let known_servers: std::collections::HashSet<String> = state
        .config()
        .servers
        .iter()
        .map(|server| server.id.clone())
        .collect();
    for (i, rule) in body.rules.iter().enumerate() {
        if let Some(unknown) = rule
            .servers
            .iter()
            .find(|server| !known_servers.contains(*server))
        {
            return Err(ApiError::from((
                StatusCode::BAD_REQUEST,
                format!("invalid `schedule.rules[{i}].servers`: unknown server `{unknown}`"),
            )));
        }
    }

    let mut rules = body.rules;
    for rule in &mut rules {
        if rule.id.trim().is_empty() {
            rule.id = uuid::Uuid::new_v4().to_string();
        }
        rule.name = rule.name.trim().to_string();
        rule.days = rule
            .days
            .iter()
            .map(|day| day.trim().to_ascii_lowercase())
            .collect();
        rule.start = rule.start.trim().to_string();
        rule.end = rule.end.trim().to_string();
    }

    state.update_config_with(|config| {
        config.schedule.enabled = body.enabled;
        config.schedule.rules = rules.clone();
        Ok::<_, ApiError>(())
    })?;

    // Applying now, rather than waiting for the monitor's next tick, makes a
    // save feel immediate and keeps the response truthful.
    let config = state.config();
    let decision = evaluate_config(&config.schedule, Local::now().naive_local());
    state
        .queue_manager
        .apply_schedule(&decision, config.schedule.enabled, true);
    state.schedule_wake.notify_one();

    h_schedule_get(State(state)).await
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

/// PUT /api/config/disk-guards -- Update disk guard settings.
pub async fn h_disk_guards_set(
    State(state): State<Arc<AppState>>,
    Json(body): Json<DiskGuardsBody>,
) -> Result<Json<SimpleResponse>, ApiError> {
    state.update_config_with(|config| {
        config.general.min_free_space_bytes = body.min_free_space_bytes;
        config.general.abort_hopeless = body.abort_hopeless;
        state
            .queue_manager
            .set_min_free_space(body.min_free_space_bytes);
        Ok::<_, ApiError>(())
    })?;
    Ok(Json(SimpleResponse { status: true }))
}

// ---------------------------------------------------------------------------
// Debug logging
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize)]
pub struct DebugLoggingBody {
    pub enabled: bool,
    /// The filter that is actually in effect, which may differ from the saved
    /// choice when `RUST_LOG`, `--log-level` or `RUSTNZB_LOG_LEVEL` overrides it.
    #[serde(default)]
    pub effective: String,
    /// True when an environment or command-line override beats the saved level,
    /// so the toggle cannot change what this process logs.
    #[serde(default)]
    pub overridden: bool,
}

/// GET /api/config/debug-logging -- Whether verbose download tracing is on.
///
/// Reports the effective state. When `RUST_LOG`, `--log-level` or
/// `RUSTNZB_LOG_LEVEL` is set it wins over the saved choice, and the response
/// says so instead of reporting the config value.
pub async fn h_debug_logging_get(
    State(state): State<Arc<AppState>>,
) -> Result<Json<DebugLoggingBody>, ApiError> {
    let saved = state.config().general.log_level.clone();
    Ok(Json(debug_logging_state(&saved)))
}

/// The state the settings page shows: the saved level, unless an override is
/// present, in which case the override is what the process actually runs.
fn debug_logging_state(saved_level: &str) -> DebugLoggingBody {
    let (effective, overridden) = match crate::log_level_override() {
        Some(level) => (level, true),
        None => (saved_level.to_string(), false),
    };
    DebugLoggingBody {
        enabled: crate::filter_is_debug(&effective),
        effective,
        overridden,
    }
}

/// PUT /api/config/debug-logging -- Turn verbose download tracing on or off.
///
/// Applies immediately: the tracing filter is rebuilt and the choice is
/// persisted, so it survives a restart. Turning it on installs
/// [`crate::DEBUG_LOG_DIRECTIVE`], scoped to rustnzb's own crates, and
/// remembers the level it replaced; turning it off restores that level. When
/// an environment or command-line override is active the choice is still
/// saved, but the live filter is left alone and the response says so.
pub async fn h_debug_logging_set(
    State(state): State<Arc<AppState>>,
    Json(body): Json<DebugLoggingBody>,
) -> Result<Json<DebugLoggingStatus>, ApiError> {
    let general = state.config().general.clone();
    let (level, before) = toggle_log_level(
        &general.log_level,
        general.log_level_before_debug.as_deref(),
        body.enabled,
    );
    let overridden = crate::log_level_override().is_some();
    if !overridden && !crate::set_log_filter(&level) {
        return Err(ApiError::bad_request(
            "the log filter is not reloadable in this process",
        ));
    }
    state.update_config_with(|config| {
        config.general.log_level = level.clone();
        config.general.log_level_before_debug = before.clone();
        Ok::<_, ApiError>(())
    })?;
    Ok(Json(DebugLoggingStatus {
        status: true,
        state: debug_logging_state(&level),
    }))
}

/// The saved level and remembered prior level after flipping the toggle.
///
/// On: install the scoped debug directive and remember the current level,
/// unless debug is already on (then keep whatever was remembered). Off:
/// restore the remembered level; with nothing remembered, a debug level drops
/// to `info` and any other level is left as it is.
fn toggle_log_level(
    current: &str,
    remembered: Option<&str>,
    enable: bool,
) -> (String, Option<String>) {
    let current = current.trim();
    if enable {
        if crate::filter_is_debug(current) {
            return (current.to_string(), remembered.map(str::to_string));
        }
        let prior = if current.is_empty() { "info" } else { current };
        return (
            crate::DEBUG_LOG_DIRECTIVE.to_string(),
            Some(prior.to_string()),
        );
    }
    match remembered.map(str::trim).filter(|r| !r.is_empty()) {
        Some(prior) => (prior.to_string(), None),
        None if crate::filter_is_debug(current) || current.is_empty() => ("info".to_string(), None),
        None => (current.to_string(), None),
    }
}

#[cfg(test)]
mod debug_toggle_tests {
    use super::toggle_log_level;
    use crate::DEBUG_LOG_DIRECTIVE;

    #[test]
    fn on_then_off_restores_a_custom_level() {
        let (on, before) = toggle_log_level("warn,nzb_nntp=info", None, true);
        assert_eq!(on, DEBUG_LOG_DIRECTIVE);
        assert_eq!(before.as_deref(), Some("warn,nzb_nntp=info"));
        let (off, before) = toggle_log_level(&on, before.as_deref(), false);
        assert_eq!(off, "warn,nzb_nntp=info");
        assert_eq!(before, None);
    }

    #[test]
    fn enabling_twice_keeps_the_original_level() {
        let (on, before) = toggle_log_level("warn", None, true);
        let (again, before) = toggle_log_level(&on, before.as_deref(), true);
        assert_eq!(again, DEBUG_LOG_DIRECTIVE);
        assert_eq!(before.as_deref(), Some("warn"));
    }

    #[test]
    fn off_without_memory_only_lowers_debug() {
        assert_eq!(toggle_log_level("debug", None, false).0, "info");
        assert_eq!(toggle_log_level("warn", None, false).0, "warn");
        assert_eq!(toggle_log_level("", None, false).0, "info");
    }

    #[test]
    fn debug_directive_leaves_transport_crates_at_info() {
        for quiet in ["h2", "hyper", "tonic", "reqwest", "opentelemetry"] {
            assert!(
                !DEBUG_LOG_DIRECTIVE.contains(quiet),
                "{quiet} must stay at the default level"
            );
        }
        assert!(DEBUG_LOG_DIRECTIVE.starts_with("info,"));
        assert!(crate::filter_is_debug(DEBUG_LOG_DIRECTIVE));
        assert!(!crate::filter_is_debug("info,h2=debug"));
    }
}

#[derive(Serialize)]
pub struct DebugLoggingStatus {
    pub status: bool,
    #[serde(flatten)]
    pub state: DebugLoggingBody,
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
    validate_feed_poll_interval(&feed)?;
    validate_feed_filter(&feed)?;
    validate_feed_url(&state, &feed.url).await?;
    state.update_config_with(|config| {
        if config.rss_feeds.iter().any(|f| f.name == feed.name) {
            return Err(ApiError::conflict(format!(
                "Feed '{}' already exists",
                feed.name
            )));
        }
        config.rss_feeds.push(feed);
        Ok::<_, ApiError>(())
    })?;
    Ok(Json(serde_json::json!({"status": true})))
}

/// PUT /api/config/rss-feeds/{name} -- Update an RSS feed.
pub async fn h_rss_feed_update(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    Json(feed): Json<RssFeedConfig>,
) -> Result<Json<serde_json::Value>, ApiError> {
    validate_feed_poll_interval(&feed)?;
    validate_feed_filter(&feed)?;
    validate_feed_url(&state, &feed.url).await?;
    state.update_config_with(|config| {
        let idx = config
            .rss_feeds
            .iter()
            .position(|f| f.name == name)
            .ok_or_else(|| ApiError::not_found("Feed not found"))?;
        config.rss_feeds[idx] = feed;
        Ok::<_, ApiError>(())
    })?;
    Ok(Json(serde_json::json!({"status": true})))
}

/// DELETE /api/config/rss-feeds/{name} -- Delete an RSS feed.
pub async fn h_rss_feed_delete(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    state.update_config_with(|config| {
        let len = config.rss_feeds.len();
        config.rss_feeds.retain(|f| f.name != name);
        if config.rss_feeds.len() == len {
            return Err(ApiError::not_found("Feed not found"));
        }
        Ok::<_, ApiError>(())
    })?;
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
        .ok_or_else(|| ApiError::not_found("RSS item not found"))?;

    let url = item
        .url
        .as_ref()
        .ok_or_else(|| ApiError::from((StatusCode::CONFLICT, "No download URL for this item")))?;

    // Fetch the NZB
    let fetch_plan = validate_fetch_url_with(url, &fetch_policy(&state)).await?;
    let client = if fetch_plan.requires_pinned_client() {
        build_fetch_client(&fetch_plan)?
    } else {
        HTTP_CLIENT.clone()
    };
    let response = client
        .get(fetch_plan.url.clone())
        .send()
        .await
        .map_err(|e| ApiError::bad_gateway(format!("Failed to fetch NZB: {e}")))?;

    if !response.status().is_success() {
        return Err(ApiError::bad_gateway(format!("HTTP {}", response.status())));
    }

    let data = read_response_bytes_limited(response, MAX_FETCH_BODY_BYTES).await?;

    let mut job = nzb_parser::parse_nzb(&item.title, &data).map_err(ApiError::from)?;

    // Use the item's category or feed category
    if let Some(ref cat) = item.category {
        job.category = cat.clone();
    }

    job.work_dir = state.queue_manager.incomplete_dir().join(&job.id);
    job.output_dir = state
        .queue_manager
        .output_dir_for(&job.category, &job.name)
        .map_err(ApiError::from)?;

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

/// Reject a feed whose `filter_regex` the RSS monitor could not compile.
/// The monitor fails closed on a bad filter (the feed is never checked), so
/// surface the problem as a 400 at save time, with the same bounds as rules.
fn validate_feed_filter(feed: &RssFeedConfig) -> Result<(), ApiError> {
    if let Some(pattern) = feed.filter_regex.as_deref() {
        compile_rss_regex(pattern)?;
    }
    Ok(())
}

/// Longest RSS match pattern we will accept, in bytes.
const MAX_RSS_REGEX_LEN: usize = 512;

/// Compile a user-supplied RSS match pattern with explicit bounds. The `regex`
/// crate is already linear-time (no catastrophic backtracking), but we also cap
/// the pattern length and the compiled-program size so a hostile or accidental
/// pattern cannot consume unbounded memory/CPU at compile time.
fn compile_rss_regex(pattern: &str) -> Result<regex::Regex, ApiError> {
    if pattern.len() > MAX_RSS_REGEX_LEN {
        return Err(ApiError::from((
            StatusCode::BAD_REQUEST,
            format!(
                "Regex too long ({} bytes, max {MAX_RSS_REGEX_LEN})",
                pattern.len()
            ),
        )));
    }
    regex::RegexBuilder::new(pattern)
        .size_limit(1 << 20) // 1 MiB compiled-program cap
        .build()
        .map_err(|e| ApiError::from((StatusCode::BAD_REQUEST, format!("Invalid regex: {e}"))))
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

/// Names in `feed_names` that match no configured RSS feed. Such a rule is
/// still saved (the feed may be added later) but can never match yet.
fn unknown_rule_feeds(state: &AppState, feed_names: &[String]) -> Vec<String> {
    let config = state.config();
    feed_names
        .iter()
        .filter(|name| !config.rss_feeds.iter().any(|f| &f.name == *name))
        .cloned()
        .collect()
}

/// `{"status": true}`, plus a `warnings` array when a rule references feeds
/// that are not configured.
fn rss_rule_saved_response(state: &AppState, rule: &RssRule) -> serde_json::Value {
    let unknown = unknown_rule_feeds(state, &rule.feed_names);
    if unknown.is_empty() {
        return serde_json::json!({ "status": true });
    }
    tracing::warn!(
        rule = %rule.name,
        feeds = ?unknown,
        "RSS rule references feeds that are not configured"
    );
    let warnings: Vec<String> = unknown
        .iter()
        .map(|name| {
            format!("feed '{name}' is not configured; the rule will not match until it is added")
        })
        .collect();
    serde_json::json!({ "status": true, "warnings": warnings })
}

/// POST /api/rss/rules -- Add an RSS download rule.
pub async fn h_rss_rule_add(
    State(state): State<Arc<AppState>>,
    Json(body): Json<RssRuleBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    // Validate and bound the user-supplied regex.
    compile_rss_regex(&body.match_regex)?;

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
    Ok(Json(rss_rule_saved_response(&state, &rule)))
}

/// PUT /api/rss/rules/{id} -- Update an RSS download rule. 404 if the rule
/// does not exist.
pub async fn h_rss_rule_update(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<RssRuleBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    // Validate and bound the user-supplied regex.
    compile_rss_regex(&body.match_regex)?;

    let exists = state
        .queue_manager
        .rss_rule_list()
        .map_err(ApiError::from)?
        .iter()
        .any(|rule| rule.id == id);
    if !exists {
        return Err(ApiError::not_found("RSS rule not found"));
    }

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
    Ok(Json(rss_rule_saved_response(&state, &rule)))
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

/// Body for `PUT /api/config/general`. Every field is optional; omitted
/// fields keep their current value.
///
/// Unknown fields are rejected with 400 rather than silently ignored, so a
/// client cannot believe it changed a setting this endpoint does not own.
/// Settings with their own endpoints stay there and are not accepted here:
/// `speed_limit_bps` (`/config/speed-limit`), `min_free_space_bytes` and
/// `abort_hopeless` (`/config/disk-guards`). `log_level` is not editable:
/// the tracing filter is built once at startup from `--log-level` /
/// `RUSTNZB_LOG_LEVEL` / `RUST_LOG`, so a saved value would have no effect.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateGeneralBody {
    pub incomplete_dir: Option<String>,
    pub complete_dir: Option<String>,
    pub data_dir: Option<String>,
    pub watch_dir: Option<String>,
    /// Dead setting: nothing reads it. Accepted so older clients still get a
    /// 200, but never stored (see the warning in `h_general_update`).
    #[serde(default)]
    pub cache_size: Option<serde::de::IgnoredAny>,
    pub max_active_downloads: Option<usize>,
    pub max_post_processing_jobs: Option<usize>,
    pub max_repair_workers: Option<usize>,
    pub max_extract_workers: Option<usize>,
    pub history_retention: Option<Option<usize>>,
    pub rss_history_limit: Option<Option<usize>>,
    pub auto_sort_remaining_pct: Option<bool>,
    /// `allow`, `warn` or `skip`: what to do with an NZB that duplicates a job.
    pub duplicate_policy: Option<String>,
    pub rss_downloaded_item_expiry_days: Option<Option<u64>>,
    pub scripts_dir: Option<String>,
    pub script_success: Option<String>,
    pub script_failure: Option<String>,
    pub script_timeout_secs: Option<u64>,
    pub script_max_output_bytes: Option<usize>,
    // The settings below are read when the engine starts; changes are
    // persisted immediately and take effect on the next process start.
    pub required_completion_pct: Option<f64>,
    pub article_timeout_secs: Option<u64>,
    pub max_nested_archive_depth: Option<u8>,
    pub direct_unpack: Option<bool>,
    pub early_failure_check: Option<bool>,
}

/// Upper bound for `max_active_downloads`; `0` still means unlimited.
const MAX_ACTIVE_DOWNLOADS_LIMIT: usize = 100;
/// Upper bound for the post-processing job and worker pool sizes.
const MAX_POSTPROC_WORKERS: usize = 64;
/// `required_completion_pct` range, matching the clamp the engine applies.
const REQUIRED_COMPLETION_PCT_RANGE: std::ops::RangeInclusive<f64> = 100.0..=200.0;
/// Upper bound for `article_timeout_secs`; `0` still disables the timeout.
const ARTICLE_TIMEOUT_SECS_MAX: u64 = 3600;
/// Upper bound for `max_nested_archive_depth`.
const MAX_NESTED_ARCHIVE_DEPTH_LIMIT: u8 = 10;

fn general_field_error(field: &str, requirement: &str) -> ApiError {
    ApiError::from((
        StatusCode::BAD_REQUEST,
        format!("invalid `{field}`: {requirement}"),
    ))
}

fn validate_max_active_downloads(value: usize) -> Result<(), ApiError> {
    if value > MAX_ACTIVE_DOWNLOADS_LIMIT {
        return Err(general_field_error(
            "max_active_downloads",
            &format!("must be at most {MAX_ACTIVE_DOWNLOADS_LIMIT} (0 = unlimited)"),
        ));
    }
    Ok(())
}

/// Reject out-of-range values before anything is applied, so a bad request
/// never half-updates the config or the live queue manager.
fn validate_general_update(body: &UpdateGeneralBody) -> Result<(), ApiError> {
    if let Some(policy) = body.duplicate_policy.as_deref()
        && policy
            .parse::<nzb_web::nzb_core::config::DuplicatePolicy>()
            .is_err()
    {
        return Err(general_field_error(
            "duplicate_policy",
            "must be allow, warn or skip",
        ));
    }
    if let Some(max) = body.max_active_downloads {
        validate_max_active_downloads(max)?;
    }
    for (field, value) in [
        ("max_post_processing_jobs", body.max_post_processing_jobs),
        ("max_repair_workers", body.max_repair_workers),
        ("max_extract_workers", body.max_extract_workers),
    ] {
        // 0 is still accepted and raised to 1, as before.
        if let Some(value) = value
            && value > MAX_POSTPROC_WORKERS
        {
            return Err(general_field_error(
                field,
                &format!("must be at most {MAX_POSTPROC_WORKERS}"),
            ));
        }
    }
    if let Some(pct) = body.required_completion_pct
        && !REQUIRED_COMPLETION_PCT_RANGE.contains(&pct)
    {
        return Err(general_field_error(
            "required_completion_pct",
            "must be between 100.0 and 200.0",
        ));
    }
    if let Some(secs) = body.article_timeout_secs
        && secs > ARTICLE_TIMEOUT_SECS_MAX
    {
        return Err(general_field_error(
            "article_timeout_secs",
            &format!("must be at most {ARTICLE_TIMEOUT_SECS_MAX} (0 = no timeout)"),
        ));
    }
    if let Some(depth) = body.max_nested_archive_depth
        && depth > MAX_NESTED_ARCHIVE_DEPTH_LIMIT
    {
        return Err(general_field_error(
            "max_nested_archive_depth",
            &format!("must be at most {MAX_NESTED_ARCHIVE_DEPTH_LIMIT}"),
        ));
    }
    Ok(())
}

/// PUT /api/config/general -- Update general settings.
pub async fn h_general_update(
    State(state): State<Arc<AppState>>,
    body: Result<Json<UpdateGeneralBody>, JsonRejection>,
) -> Result<Json<SimpleResponse>, ApiError> {
    // Map extractor failures (unknown field, wrong type) to a 400 that names
    // the offending field, instead of axum's plain-text 422.
    let Json(body) =
        body.map_err(|rejection| ApiError::from((StatusCode::BAD_REQUEST, rejection.body_text())))?;
    validate_general_update(&body)?;
    if body.cache_size.is_some() {
        tracing::warn!("general.cache_size is unused; the submitted value was ignored");
    }
    state.update_config_with(|config| {
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
        if let Some(mad) = body.max_active_downloads {
            state.queue_manager.set_max_active_downloads(mad);
            config.general.max_active_downloads = mad;
        }
        // Resource pools cannot safely discard live permits. Persist updated
        // stage limits now and apply them on the next process start.
        if let Some(max) = body.max_post_processing_jobs {
            config.general.max_post_processing_jobs = max.max(1);
        }
        if let Some(max) = body.max_repair_workers {
            config.general.max_repair_workers = max.max(1);
        }
        if let Some(max) = body.max_extract_workers {
            config.general.max_extract_workers = max.max(1);
        }
        if let Some(ret) = body.history_retention {
            let ret = normalize_history_retention(ret);
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
        if let Some(enabled) = body.auto_sort_remaining_pct {
            state.queue_manager.set_auto_sort_remaining_pct(enabled);
            config.general.auto_sort_remaining_pct = enabled;
        }
        // Validated by `validate_general_update`, so only a valid value gets here.
        if let Some(policy) = body.duplicate_policy.as_deref().and_then(|policy| {
            policy
                .parse::<nzb_web::nzb_core::config::DuplicatePolicy>()
                .ok()
        }) {
            state.queue_manager.set_duplicate_policy(policy);
            config.general.duplicate_policy = policy;
        }
        if let Some(days) = body.rss_downloaded_item_expiry_days {
            config.general.rss_downloaded_item_expiry_days = normalize_rss_expiry_days(days);
        }
        if let Some(directory) = body.scripts_dir {
            config.general.scripts_dir = if directory.is_empty() {
                None
            } else {
                Some(directory.into())
            };
        }
        if let Some(script) = body.script_success {
            config.general.script_success = if script.is_empty() {
                None
            } else {
                Some(script.into())
            };
        }
        if let Some(script) = body.script_failure {
            config.general.script_failure = if script.is_empty() {
                None
            } else {
                Some(script.into())
            };
        }
        if let Some(timeout) = body.script_timeout_secs {
            config.general.script_timeout_secs = timeout.max(1);
        }
        if let Some(max_output) = body.script_max_output_bytes {
            config.general.script_max_output_bytes = max_output;
        }
        if let Some(pct) = body.required_completion_pct {
            config.general.required_completion_pct = pct;
        }
        if let Some(secs) = body.article_timeout_secs {
            config.general.article_timeout_secs = secs;
        }
        if let Some(depth) = body.max_nested_archive_depth {
            config.general.max_nested_archive_depth = depth;
        }
        if let Some(enabled) = body.direct_unpack {
            config.general.direct_unpack = enabled;
        }
        if let Some(enabled) = body.early_failure_check {
            config.general.early_failure_check = enabled;
        }

        state.queue_manager.set_postproc_scripts(
            config.general.scripts_dir.clone(),
            config.general.script_success.clone(),
            config.general.script_failure.clone(),
            config.general.script_timeout_secs,
            config.general.script_max_output_bytes,
        );
        state
            .queue_manager
            .set_queue_complete_persist(config.general.queue_complete_pers);

        Ok::<_, ApiError>(())
    })?;
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

/// GET /api/statistics -- Persistent global download and NNTP statistics.
pub async fn h_global_statistics(
    State(state): State<Arc<AppState>>,
) -> Result<Json<nzb_web::GlobalStatisticsData>, ApiError> {
    let servers = state.config().servers.clone();
    Ok(Json(state.queue_manager.global_statistics(&servers)))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Get free disk space for a path (returns 0 on error).
fn get_disk_space_free(path: &std::path::Path) -> u64 {
    get_disk_space(path).0
}

/// Get total disk space for a path's filesystem (returns 0 on error or
/// unsupported platform — the UI treats 0 as "unknown" and hides any
/// usage bar derived from it rather than showing a bogus percentage).
fn get_disk_space_total(path: &std::path::Path) -> u64 {
    get_disk_space(path).1
}

/// Get (free, total) disk space in bytes for the filesystem containing
/// `path`. Returns `(0, 0)` on error or on platforms without a `statvfs`
/// equivalent wired up (currently: everything but Unix).
#[cfg_attr(not(unix), allow(unused_variables))]
fn get_disk_space(path: &std::path::Path) -> (u64, u64) {
    #[cfg(unix)]
    {
        use std::ffi::CString;
        use std::mem::MaybeUninit;
        let c_path = match CString::new(path.to_string_lossy().as_bytes()) {
            Ok(p) => p,
            Err(_) => return (0, 0),
        };
        unsafe {
            let mut stat = MaybeUninit::<libc::statvfs>::uninit();
            if libc::statvfs(c_path.as_ptr(), stat.as_mut_ptr()) == 0 {
                let stat = stat.assume_init();
                #[allow(clippy::unnecessary_cast)] // u32 on macOS, u64 on Linux
                let frsize = stat.f_frsize as u64;
                #[allow(clippy::unnecessary_cast)]
                return (stat.f_bavail as u64 * frsize, stat.f_blocks as u64 * frsize);
            }
        }
        (0, 0)
    }
    #[cfg(not(unix))]
    {
        (0, 0)
    }
}

// ---------------------------------------------------------------------------
// Health check
// ---------------------------------------------------------------------------

pub async fn h_health() -> Json<serde_json::Value> {
    Json(serde_json::json!({"status": "ok"}))
}

/// GET /api/ready -- Readiness probe. The engine is initialised before the
/// listener binds, so this is 200 whenever the process is serving, and 503
/// once shutdown has begun, so a load balancer stops routing new requests.
pub async fn h_ready(State(state): State<Arc<AppState>>) -> (StatusCode, Json<serde_json::Value>) {
    if state
        .shutting_down
        .load(std::sync::atomic::Ordering::Relaxed)
    {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"status": "shutting_down"})),
        )
    } else {
        (StatusCode::OK, Json(serde_json::json!({"status": "ready"})))
    }
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
        return Err(ApiError::from((
            StatusCode::BAD_REQUEST,
            format!("Not a directory: {path}"),
        )));
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
    /// One entry per id that failed, in request order.
    pub failures: Vec<BulkActionFailure>,
}

#[derive(Serialize)]
pub struct BulkActionFailure {
    pub id: String,
    /// Same shape as an API error body (`error_kind`, `human_readable`,
    /// `status`).
    pub error: ApiError,
}

/// Response to `POST /api/history/bulk`. `trashed` is present only when the
/// caller asked for the undo window.
#[derive(Serialize)]
pub struct HistoryBulkResponse {
    pub status: bool,
    pub succeeded: usize,
    pub failed: usize,
    pub failures: Vec<BulkActionFailure>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trashed: Option<HistoryTrashResponse>,
}

/// POST /api/queue/bulk -- Perform an action on multiple jobs.
pub async fn h_queue_bulk_action(
    State(state): State<Arc<AppState>>,
    Json(body): Json<BulkActionBody>,
) -> Result<Json<BulkActionResponse>, ApiError> {
    let qm = &state.queue_manager;
    let mut succeeded = 0usize;
    let mut failures = Vec::new();

    for id in &body.ids {
        let result = match body.action.as_str() {
            "pause" => qm.pause_job(id),
            "resume" => qm.resume_job(id),
            // `remove_job` is idempotent and returns Ok for an unknown id;
            // a bulk request must report that id as failed.
            "delete" if !qm.contains_job(id) => {
                Err(nzb_web::nzb_core::NzbError::JobNotFound(id.clone()))
            }
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
            Err(error) => failures.push(BulkActionFailure {
                id: id.clone(),
                error: ApiError::from(error),
            }),
        }
    }

    Ok(Json(BulkActionResponse {
        status: failures.is_empty(),
        succeeded,
        failed: failures.len(),
        failures,
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
        "version": env!("RUSTNZB_BUILD_VERSION"),
    }))
}

/// Import SABnzbd INI file (multipart upload) → returns preview JSON.
pub async fn h_import_sabnzbd_ini(mut multipart: Multipart) -> Result<impl IntoResponse, ApiError> {
    let field = multipart
        .next_field()
        .await
        .map_err(ApiError::from)?
        .ok_or(ApiError::bad_request("no file uploaded"))?;

    let data = field.bytes().await.map_err(ApiError::from)?;

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
    State(state): State<Arc<AppState>>,
    Json(req): Json<ImportApiRequest>,
) -> Result<impl IntoResponse, ApiError> {
    let base_url = req.url.trim_end_matches('/');

    let fetch_plan = validate_fetch_url_with(base_url, &fetch_policy(&state)).await?;
    let client = if fetch_plan.requires_pinned_client() {
        build_fetch_client(&fetch_plan)?
    } else {
        HTTP_CLIENT.clone()
    };

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
            return Err(ApiError::bad_gateway(format!(
                "SABnzbd returned HTTP {} — check your API key",
                resp.status()
            )));
        }

        let body = read_response_bytes_limited(resp, MAX_FETCH_BODY_BYTES).await?;
        let json: serde_json::Value = serde_json::from_slice(&body)
            .map_err(|e| ApiError::bad_gateway(format!("Invalid JSON from SABnzbd: {e}")))?;

        let preview = sabnzbd_import::parse_sabnzbd_api_response(&json);
        return Ok(Json(preview));
    }

    Err(ApiError::bad_gateway(format!(
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

    // Reject relative complete_dir/incomplete_dir rather than silently persisting
    // them: SABnzbd allows relative paths (resolved against its own working
    // directory), but rustnzb creates these directories eagerly at startup via
    // create_dir_all, which resolves a relative path against *its* process CWD
    // (e.g. "/" in Docker) instead of the intended download volume — producing
    // a confusing crash far away from the actual misconfiguration (see #62).
    if let Some(ref dir) = preview.general.complete_dir
        && !std::path::Path::new(dir).is_absolute()
    {
        return Err(ApiError::bad_request(
            "cannot apply: complete_dir must be an absolute path",
        ));
    }
    if let Some(ref dir) = preview.general.incomplete_dir
        && !std::path::Path::new(dir).is_absolute()
    {
        return Err(ApiError::bad_request(
            "cannot apply: incomplete_dir must be an absolute path",
        ));
    }

    // Imported categories replace the configured ones wholesale, so they get
    // the same check as a category save. Otherwise an unsafe name or
    // output_dir is persisted and every later job in it fails at enqueue.
    let invalid: Vec<String> = preview
        .categories
        .iter()
        .filter_map(|category| {
            category
                .validate()
                .err()
                .map(|reason| format!("'{}': {reason}", category.name))
        })
        .collect();
    if !invalid.is_empty() {
        return Err(ApiError::from((
            StatusCode::BAD_REQUEST,
            format!("cannot apply: invalid categories: {}", invalid.join("; ")),
        )));
    }

    let config = state
        .update_config_with(|config| {
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
            Ok::<_, ApiError>(config.clone())
        })
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
///
/// `password` and `api_key` are replaced with `********` when set.
#[cfg(feature = "webdav")]
pub async fn h_dav_config_get(
    State(state): State<Arc<AppState>>,
) -> Result<Json<DavConfig>, ApiError> {
    Ok(Json(masked_config(&state.config()).dav))
}

/// PUT /api/config/dav -- Update DAV auto-send configuration.
///
/// The block is replaced as a whole, except for the secrets `password` and
/// `api_key`, which follow the write rules in [`resolve_secret`]: absent, empty
/// or masked keeps the stored value, `null` clears it.
///
/// When `auto_send_all` is true the `category_rules` list is cleared — the two
/// modes are mutually exclusive.
#[cfg(feature = "webdav")]
pub async fn h_dav_config_set(
    State(state): State<Arc<AppState>>,
    Json(body): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let serde_json::Value::Object(fields) = &body else {
        return Err(ApiError::bad_request("DAV config must be a JSON object"));
    };
    let mut dav: DavConfig = serde_json::from_value(body.clone()).map_err(|e| {
        ApiError::from((StatusCode::BAD_REQUEST, format!("Invalid DAV config: {e}")))
    })?;
    if dav.auto_send_all {
        dav.category_rules.clear();
    }
    state.update_config_with(|config| {
        dav.password = resolve_secret(fields, "password", config.dav.password.clone());
        dav.api_key = resolve_secret(fields, "api_key", config.dav.api_key.clone());
        config.dav = dav;
        Ok::<_, ApiError>(())
    })?;
    Ok(Json(serde_json::json!({ "status": true })))
}

#[cfg(test)]
mod tests {
    #[cfg(feature = "webdav")]
    use super::resolve_secret;
    use super::{
        MAX_RSS_REGEX_LEN, compile_rss_regex, mask_proxy_userinfo, proxy_url_unchanged,
        sanitize_server_config,
    };

    use nzb_web::nzb_core::config::ServerConfig;

    #[test]
    fn compile_rss_regex_accepts_normal_pattern() {
        assert!(compile_rss_regex(r"(?i)ubuntu.*\.iso").is_ok());
    }

    #[test]
    fn compile_rss_regex_rejects_invalid_pattern() {
        let err = compile_rss_regex("(unclosed").unwrap_err();
        assert!(err.to_string().contains("Invalid regex"));
    }

    #[test]
    fn compile_rss_regex_rejects_overlong_pattern() {
        let pattern = "a".repeat(MAX_RSS_REGEX_LEN + 1);
        let err = compile_rss_regex(&pattern).unwrap_err();
        assert!(err.to_string().contains("too long"));
    }

    #[test]
    fn mask_proxy_userinfo_hides_credentials_only() {
        assert_eq!(
            mask_proxy_userinfo("socks5://user:pass@proxy.example.net:1080"),
            "socks5://********@proxy.example.net:1080"
        );
        assert_eq!(
            mask_proxy_userinfo("http://u@host:3128/path@x"),
            "http://********@host:3128/path@x"
        );
        assert_eq!(
            mask_proxy_userinfo("socks5://proxy.example.net:1080"),
            "socks5://proxy.example.net:1080"
        );
        assert_eq!(mask_proxy_userinfo("not a url"), "not a url");
    }

    #[test]
    fn proxy_url_unchanged_only_matches_the_masked_stored_form() {
        let stored = Some("socks5://user:pass@proxy:1080");
        assert!(proxy_url_unchanged(
            Some("socks5://********@proxy:1080"),
            stored
        ));
        assert!(!proxy_url_unchanged(
            Some("socks5://other@proxy:1080"),
            stored
        ));
        assert!(!proxy_url_unchanged(None, stored));
        assert!(!proxy_url_unchanged(
            Some("socks5://********@proxy:1080"),
            None
        ));
    }

    #[cfg(feature = "webdav")]
    #[test]
    fn resolve_secret_follows_absent_mask_null_rules() {
        let stored = || Some("stored".to_string());
        let body = |v: serde_json::Value| v.as_object().cloned().unwrap();

        assert_eq!(
            resolve_secret(&body(serde_json::json!({})), "k", stored()),
            stored()
        );
        assert_eq!(
            resolve_secret(&body(serde_json::json!({"k": "********"})), "k", stored()),
            stored()
        );
        assert_eq!(
            resolve_secret(&body(serde_json::json!({"k": ""})), "k", stored()),
            stored()
        );
        assert_eq!(
            resolve_secret(&body(serde_json::json!({"k": null})), "k", stored()),
            None
        );
        assert_eq!(
            resolve_secret(&body(serde_json::json!({"k": "new"})), "k", stored()),
            Some("new".to_string())
        );
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
