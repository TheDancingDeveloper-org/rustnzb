use std::sync::Arc;

use axum::Json;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Multipart, Path, Query, State};
use axum::http::HeaderMap;
use axum::http::header;
use axum::response::IntoResponse;
use http::StatusCode;
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};

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
use nzb_web::nzb_core::config_backup::{self, mask_proxy_userinfo};
use nzb_web::nzb_core::models::*;
use nzb_web::nzb_core::nzb_parser;
use nzb_web::nzb_core::sabnzbd_import;
use nzb_web::nzb_core::schedule::{ScheduleRule, evaluate_config};
use nzb_web::nzb_core::{HistoryFilter, HistorySummary, HistoryWindowStats};
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

#[derive(Deserialize, Default, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct QueueQuery {
    /// Maximum rows to return.
    pub limit: Option<usize>,
    /// Rows to skip.
    pub offset: Option<usize>,
}

#[derive(Deserialize, Default, IntoParams)]
#[into_params(parameter_in = Query)]
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

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct AddNzbQuery {
    /// Category to file the job under.
    pub category: Option<String>,
    /// Priority: 0 low, 1 normal, 2 high, 3 force.
    pub priority: Option<i32>,
    /// Display name; defaults to the uploaded file's name.
    pub name: Option<String>,
}

#[derive(Deserialize, Default, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct LogQuery {
    /// Only entries for this job's persisted log.
    pub job_id: Option<String>,
    /// Only entries after this sequence number.
    pub after_seq: Option<u64>,
    /// Minimum level (`error`, `warn`, `info`, `debug`, `trace`).
    pub level: Option<String>,
    /// Maximum entries to return.
    pub limit: Option<usize>,
}

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct PauseForQuery {
    /// How long to pause for, in seconds.
    pub duration_secs: u64,
}

#[derive(Deserialize, ToSchema)]
pub struct MoveJobBody {
    /// Zero-based position to move the job to.
    pub position: usize,
}

#[derive(Deserialize, ToSchema)]
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

#[derive(Deserialize, Serialize, ToSchema)]
pub struct HistoryRetentionBody {
    /// Entries to keep; null or 0 keeps all.
    pub retention: Option<usize>,
}

#[derive(Deserialize, Serialize, ToSchema)]
pub struct MaxActiveDownloadsBody {
    /// Concurrent downloads; 0 means unlimited.
    pub max_active_downloads: usize,
}

#[derive(Deserialize, ToSchema)]
pub struct SetPriorityBody {
    /// Priority: 0 low, 1 normal, 2 high, 3 force.
    pub priority: i32,
}

// ---------------------------------------------------------------------------
// Response types
// ---------------------------------------------------------------------------

#[derive(Serialize, ToSchema)]
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
#[derive(Serialize, ToSchema)]
pub struct QueueJob {
    #[serde(flatten)]
    pub job: NzbJob,
    pub pause_reason: Option<PauseReason>,
}

#[derive(Serialize, ToSchema)]
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

#[derive(Serialize, Default, ToSchema)]
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

#[derive(Serialize, ToSchema)]
pub struct HistoryFailReason {
    pub reason: String,
    pub count: usize,
}

impl HistoryStats {
    /// Build the panel statistics from SQL aggregates plus the failed rows'
    /// error text (most recent first).
    ///
    /// This is the same fold the panel used to perform over whole summaries,
    /// but the counts, bytes and duration are already summed by SQLite over a
    /// covering index, so the endpoint no longer loads every matching history
    /// row to render them.
    fn from_window_stats(
        window: &HistoryWindowStats,
        failure_messages: impl Iterator<Item = Option<String>>,
    ) -> Self {
        let mut stats = Self {
            completed: window.completed as usize,
            completed_bytes: window.completed_bytes,
            failed: window.failed as usize,
            ..Self::default()
        };
        let mut reasons: Vec<HistoryFailReason> = Vec::new();
        for message in failure_messages {
            // First clause of the message, as a short grouping key.
            let msg = message.as_deref().unwrap_or("unknown");
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
        let finished = stats.completed + stats.failed;
        if finished > 0 {
            stats.success_pct = ((stats.completed as f64 / finished as f64) * 100.0).round() as u32;
        }
        if stats.completed > 0 {
            stats.avg_duration_secs =
                Some(window.completed_duration_ms as f64 / 1000.0 / stats.completed as f64);
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
#[derive(Serialize, ToSchema)]
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

#[derive(Serialize, ToSchema)]
pub struct AddNzbResponse {
    pub status: bool,
    pub nzo_ids: Vec<String>,
}

#[derive(Serialize, ToSchema)]
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

#[derive(Serialize, ToSchema)]
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

#[derive(Serialize, ToSchema)]
pub struct NntpConnectionStatus {
    pub server_id: String,
    pub connected: usize,
    pub limit: usize,
}

#[derive(Serialize, ToSchema)]
pub struct SimpleResponse {
    pub status: bool,
}

/// The credential used by Sonarr, Radarr, Lidarr, and other SABnzbd-compatible
/// clients to call `/sabnzbd/api`.
#[derive(Serialize, ToSchema)]
pub struct SabApiKeyResponse {
    pub api_key: Option<String>,
}

#[derive(Serialize, ToSchema)]
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
#[utoipa::path(
    get,
    path = "/api/queue",
    tag = "queue",
    summary = "List the download queue",
    params(QueueQuery),
    responses(
        (status = 200, description = "Queue page with live speed and pause state", body = QueueResponse),
        (status = 401, description = "Not authenticated", body = ApiError),
        (status = 500, description = "Queue could not be read", body = ApiError),
    ),
)]
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
#[utoipa::path(
    post,
    path = "/api/queue/add",
    tag = "queue",
    summary = "Add one or more NZBs",
    description = "Accepts a `multipart/form-data` body. Parts with a file name (or named `file`) are NZB uploads; the text fields `password`, `category` (alias `cat`), `priority` and `name` (alias `nzbname`) set job options. A `GET`-style set of query parameters is accepted as defaults for the text fields. An `Idempotency-Key` header makes the admission repeatable.",
    params(AddNzbQuery, ("Idempotency-Key" = Option<String>, Header, description = "Opaque key that makes a repeated add resolve to the same admission instead of creating a second job")),
    request_body(content_type = "multipart/form-data", description = "One or more NZB files plus optional `password`, `category`, `priority` and `name` text fields", content = String),
    responses(
        (status = 200, description = "Admission result", body = AddNzbResponse),
        (status = 400, description = "No NZB supplied or a field is invalid", body = ApiError),
        (status = 401, description = "Not authenticated", body = ApiError),
        (status = 409, description = "The NZB duplicates a job under the configured duplicate policy", body = ApiError),
        (status = 413, description = "The upload exceeds the configured limit", body = ApiError),
    ),
)]
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
#[utoipa::path(
    put,
    path = "/api/queue/{id}/priority",
    tag = "queue",
    summary = "Set a job's priority",
    params(("id" = String, Path, description = "Job id")),
    request_body = SetPriorityBody,
    responses(
        (status = 200, description = "Priority updated", body = SimpleResponse),
        (status = 401, description = "Not authenticated", body = ApiError),
        (status = 404, description = "Job not found", body = ApiError),
    ),
)]
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
#[utoipa::path(
    post,
    path = "/api/queue/sort",
    tag = "queue",
    summary = "Sort the queue",
    request_body = SortQueueBody,
    responses(
        (status = 200, description = "Queue reordered", body = SimpleResponse),
        (status = 400, description = "Unknown sort field or direction", body = ApiError),
        (status = 401, description = "Not authenticated", body = ApiError),
    ),
)]
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

#[derive(Deserialize, ToSchema)]
pub struct AddUrlBody {
    /// URL of the NZB to fetch.
    pub url: String,
    /// Display name; defaults to the NZB's own name.
    pub name: Option<String>,
    /// Category to file the job under.
    pub category: Option<String>,
    /// Priority: 0 low, 1 normal, 2 high, 3 force.
    pub priority: Option<i32>,
    /// Archive password; overrides inline and NZB-embedded passwords.
    #[serde(default)]
    pub password: Option<String>,
}

/// POST /api/queue/add-url -- Add an NZB from a URL.
#[utoipa::path(
    post,
    path = "/api/queue/add-url",
    tag = "queue",
    summary = "Add an NZB from a URL",
    description = "Fetches the NZB through the configured fetch policy (private addresses are refused) and admits it like `POST /api/queue/add`. An `Idempotency-Key` header makes the admission repeatable.",
    params(("Idempotency-Key" = Option<String>, Header, description = "Opaque key that makes a repeated add resolve to the same admission instead of creating a second job")),
    request_body = AddUrlBody,
    responses(
        (status = 200, description = "Admission result", body = AddNzbResponse),
        (status = 400, description = "No URL, malformed URL, or the URL is not allowed", body = ApiError),
        (status = 401, description = "Not authenticated", body = ApiError),
        (status = 409, description = "The NZB duplicates a job under the configured duplicate policy", body = ApiError),
        (status = 502, description = "The URL could not be fetched", body = ApiError),
    ),
)]
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
#[utoipa::path(
    post,
    path = "/api/queue/{id}/pause",
    tag = "queue",
    summary = "Pause one job",
    params(("id" = String, Path, description = "Job id")),
    responses(
        (status = 200, description = "Job paused", body = SimpleResponse),
        (status = 401, description = "Not authenticated", body = ApiError),
        (status = 404, description = "Job not found", body = ApiError),
    ),
)]
pub async fn h_queue_pause(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<SimpleResponse>, ApiError> {
    state.queue_manager.pause_job(&id).map_err(ApiError::from)?;
    Ok(Json(SimpleResponse { status: true }))
}

/// POST /api/queue/{id}/resume -- Resume a paused job.
#[utoipa::path(
    post,
    path = "/api/queue/{id}/resume",
    tag = "queue",
    summary = "Resume one job",
    params(("id" = String, Path, description = "Job id")),
    responses(
        (status = 200, description = "Job resumed", body = SimpleResponse),
        (status = 401, description = "Not authenticated", body = ApiError),
        (status = 404, description = "Job not found", body = ApiError),
    ),
)]
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
#[utoipa::path(
    delete,
    path = "/api/queue/{id}",
    tag = "queue",
    summary = "Remove a job from the queue",
    params(("id" = String, Path, description = "Job id")),
    responses(
        (status = 200, description = "Job removed", body = SimpleResponse),
        (status = 401, description = "Not authenticated", body = ApiError),
        (status = 404, description = "Job not found", body = ApiError),
    ),
)]
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
#[utoipa::path(
    post,
    path = "/api/queue/{id}/move",
    tag = "queue",
    summary = "Move a job within the queue",
    params(("id" = String, Path, description = "Job id")),
    request_body = MoveJobBody,
    responses(
        (status = 200, description = "Job moved", body = SimpleResponse),
        (status = 400, description = "Position is out of range", body = ApiError),
        (status = 401, description = "Not authenticated", body = ApiError),
        (status = 404, description = "Job not found", body = ApiError),
    ),
)]
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
#[utoipa::path(
    post,
    path = "/api/queue/pause",
    tag = "queue",
    summary = "Pause all downloads",
    responses(
        (status = 200, description = "Downloads paused", body = SimpleResponse),
        (status = 401, description = "Not authenticated", body = ApiError),
    ),
)]
pub async fn h_queue_pause_all(
    State(state): State<Arc<AppState>>,
) -> Result<Json<SimpleResponse>, ApiError> {
    state.queue_manager.pause_all();
    Ok(Json(SimpleResponse { status: true }))
}

/// POST /api/queue/resume -- Resume all downloads.
#[utoipa::path(
    post,
    path = "/api/queue/resume",
    tag = "queue",
    summary = "Resume all downloads",
    responses(
        (status = 200, description = "Downloads resumed", body = SimpleResponse),
        (status = 401, description = "Not authenticated", body = ApiError),
    ),
)]
pub async fn h_queue_resume_all(
    State(state): State<Arc<AppState>>,
) -> Result<Json<SimpleResponse>, ApiError> {
    state.queue_manager.resume_all();
    Ok(Json(SimpleResponse { status: true }))
}

/// POST /api/queue/pause-for -- Pause all downloads for a duration.
#[utoipa::path(
    post,
    path = "/api/queue/pause-for",
    tag = "queue",
    summary = "Pause all downloads for a while",
    params(PauseForQuery),
    responses(
        (status = 200, description = "Downloads paused until the deadline", body = SimpleResponse),
        (status = 401, description = "Not authenticated", body = ApiError),
    ),
)]
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
#[utoipa::path(
    get,
    path = "/api/history",
    tag = "history",
    summary = "List history entries",
    description = "Pages over completed and failed jobs. `stages` and `server_stats` are omitted from list rows; fetch `GET /api/history/{id}` for the full row.",
    params(HistoryQuery),
    responses(
        (status = 200, description = "History page, aggregate statistics and known categories", body = HistoryResponse),
        (status = 400, description = "A filter is invalid", body = ApiError),
        (status = 401, description = "Not authenticated", body = ApiError),
    ),
)]
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
        let stats = HistoryStats::from_window_stats(
            &db.history_window_stats(&window)?,
            db.history_failure_messages(&window)?.into_iter(),
        );
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
#[utoipa::path(
    get,
    path = "/api/history/{id}",
    tag = "history",
    summary = "Get one history entry",
    params(("id" = String, Path, description = "History entry id")),
    responses(
        (status = 200, description = "Full entry, including stages and per-server statistics", body = HistoryResponseEntry),
        (status = 401, description = "Not authenticated", body = ApiError),
        (status = 404, description = "Entry not found", body = ApiError),
    ),
)]
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
#[derive(Serialize, ToSchema)]
pub struct HistoryTrashResponse {
    pub status: bool,
    /// Entries actually trashed, in request order.
    pub entries: Vec<TrashedHistoryEntry>,
    /// RFC 3339 instant after which a restore is refused and the next purge
    /// sweep removes the rows.
    pub purge_after: String,
}

#[derive(Serialize, ToSchema)]
#[serde(untagged)]
pub enum HistoryDeleteResponse {
    Trashed(HistoryTrashResponse),
    Removed(SimpleResponse),
}

/// The list-row shape for a trashed entry. It keeps the fields an undo needs
/// to put the row back in view, without loading its blobs.
#[derive(Serialize, ToSchema)]
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
#[utoipa::path(
    delete,
    path = "/api/history/{id}",
    tag = "history",
    summary = "Delete one history entry",
    params(
        ("id" = String, Path, description = "History entry id"),
        ("trash" = Option<bool>, Query, description = "Move the entry into the undo window instead of deleting it"),
    ),
    responses(
        (status = 200, description = "Entry deleted, or trashed and restorable until purge_after", body = HistoryDeleteResponse),
        (status = 401, description = "Not authenticated", body = ApiError),
    ),
)]
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

#[derive(Deserialize, ToSchema)]
pub struct HistoryRestoreBody {
    pub ids: Vec<String>,
}

#[derive(Serialize, ToSchema)]
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
#[utoipa::path(
    post,
    path = "/api/history/restore",
    tag = "history",
    summary = "Restore trashed history entries",
    description = "Undo `DELETE /api/history/{id}?trash=true` while the undo window is open. One request, one result per id; an id whose window has closed is reported as a per-id failure rather than failing the batch.",
    request_body = HistoryRestoreBody,
    responses(
        (status = 200, description = "Per-id restore results", body = HistoryRestoreResponse),
        (status = 400, description = "Too many ids in one request", body = ApiError),
        (status = 401, description = "Not authenticated", body = ApiError),
    ),
)]
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
#[utoipa::path(
    delete,
    path = "/api/history",
    tag = "history",
    summary = "Clear all history",
    responses(
        (status = 200, description = "History cleared", body = SimpleResponse),
        (status = 401, description = "Not authenticated", body = ApiError),
    ),
)]
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
#[utoipa::path(
    post,
    path = "/api/history/{id}/retry",
    tag = "history",
    summary = "Retry a history entry",
    description = "Re-admits the entry's NZB with a new job id. Idempotent per entry: repeating the request while the retry is live resolves to the same new job.",
    params(("id" = String, Path, description = "History entry id")),
    responses(
        (status = 200, description = "Retry admitted", body = AddNzbResponse),
        (status = 400, description = "The entry carries no NZB data", body = ApiError),
        (status = 401, description = "Not authenticated", body = ApiError),
        (status = 404, description = "Entry not found", body = ApiError),
        (status = 409, description = "The retried NZB duplicates a live job", body = ApiError),
    ),
)]
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

#[derive(Deserialize, ToSchema)]
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
#[utoipa::path(
    post,
    path = "/api/history/bulk",
    tag = "history",
    summary = "Delete or retry several history entries",
    description = "One request, one result per id; a per-id failure is reported rather than failing the batch. With `status` set, `ids` may be empty and the action covers every entry with that status (optionally narrowed by `days`, `category` and `search`).",
    request_body = HistoryBulkBody,
    responses(
        (status = 200, description = "Per-id results; `trashed` is present when the caller asked for the undo window", body = HistoryBulkResponse),
        (status = 400, description = "Unknown action or too many ids", body = ApiError),
        (status = 401, description = "Not authenticated", body = ApiError),
    ),
)]
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
#[utoipa::path(
    get,
    path = "/api/status",
    tag = "system",
    summary = "Engine status",
    description = "Version, pause state, live speed, disk space, post-processing load and NNTP connection usage.",
    responses(
        (status = 200, description = "Current engine status", body = StatusResponse),
        (status = 401, description = "Not authenticated", body = ApiError),
    ),
)]
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
#[utoipa::path(
    get,
    path = "/api/logs",
    tag = "logs",
    summary = "Read the in-memory log buffer",
    description = "Pages over the live log ring buffer, or over one job's persisted log when `job_id` is given.",
    params(LogQuery),
    responses(
        (status = 200, description = "Matching log entries", body = LogResponse),
        (status = 400, description = "A filter is invalid", body = ApiError),
        (status = 401, description = "Not authenticated", body = ApiError),
    ),
)]
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
#[utoipa::path(
    get,
    path = "/api/config",
    tag = "config",
    summary = "Read the persisted configuration",
    description = "Returns the full configuration document as stored. Server and API-key secrets are masked exactly as the settings UI receives them.",
    responses(
        (status = 200, description = "Configuration document", body = serde_json::Value),
        (status = 401, description = "Not authenticated", body = ApiError),
    ),
)]
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

// ---------------------------------------------------------------------------
// Config backup and restore
// ---------------------------------------------------------------------------

/// Largest backup file accepted. A full `config.toml` is a few kilobytes; the
/// cap is generous enough for a large feed and category list and small enough
/// that a mistaken upload cannot be used to exhaust memory.
const MAX_BACKUP_BYTES: usize = 4 * 1024 * 1024;

#[derive(Deserialize, Default, IntoParams)]
pub struct BackupExportQuery {
    /// Include real credentials. Off by default: a masked backup is safe to
    /// share, and the file says which it is.
    #[serde(default)]
    pub secrets: bool,
}

/// GET /api/config/backup -- Download the configuration as one TOML file.
///
/// Secrets are masked unless `?secrets=true` is given. The response is marked
/// `no-store`, because even a masked export describes the install.
#[utoipa::path(
    get,
    path = "/api/config/backup",
    tag = "config",
    summary = "Download the configuration as a TOML backup",
    description = "Returns one TOML document describing the install. Secrets are masked unless `?secrets=true` is given. The response is marked `no-store`.",
    params(BackupExportQuery),
    responses(
        (status = 200, description = "Configuration backup as TOML", body = String, content_type = "application/toml"),
        (status = 401, description = "Not authenticated", body = ApiError),
        (status = 500, description = "The configuration could not be serialized", body = ApiError),
    ),
)]
pub async fn h_config_backup_export(
    State(state): State<Arc<AppState>>,
    Query(query): Query<BackupExportQuery>,
) -> Result<impl IntoResponse, ApiError> {
    let created_at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let body = config_backup::export(
        &state.config(),
        env!("RUSTNZB_BUILD_VERSION"),
        &created_at,
        query.secrets,
    )
    .map_err(ApiError::from)?;
    let file_stamp = chrono::Utc::now().format("%Y%m%dT%H%M%SZ");
    let disposition = format!("attachment; filename=\"rustnzb-backup-{file_stamp}.toml\"");
    Ok((
        StatusCode::OK,
        [
            (
                header::CONTENT_TYPE,
                "application/toml; charset=utf-8".to_string(),
            ),
            (header::CONTENT_DISPOSITION, disposition),
            (header::CACHE_CONTROL, "no-store".to_string()),
        ],
        body,
    ))
}

/// A backup document read from a request body, decoded once for both the
/// preview and the restore.
struct ParsedBackup {
    document: config_backup::BackupDocument,
    config: nzb_web::nzb_core::config::AppConfig,
    summary: config_backup::RestoreSummary,
}

/// Parse and check a backup body, resolve its masks against the install and
/// validate the result. Nothing is written here, so a preview and a restore
/// take exactly the same path up to the commit.
fn parse_and_prepare(
    body: &[u8],
    stored: &nzb_web::nzb_core::config::AppConfig,
) -> Result<ParsedBackup, ApiError> {
    if body.len() > MAX_BACKUP_BYTES {
        return Err(ApiError::from((
            StatusCode::PAYLOAD_TOO_LARGE,
            format!("backup file is larger than {MAX_BACKUP_BYTES} bytes"),
        )));
    }
    let text = std::str::from_utf8(body)
        .map_err(|_| ApiError::bad_request("backup file must be UTF-8 text"))?;
    let document =
        config_backup::parse(text).map_err(|e| ApiError::from((StatusCode::BAD_REQUEST, e)))?;
    let mut config = document.config.clone();
    config_backup::normalize(&mut config);
    let mut summary = config_backup::summarize(&config, Vec::new());
    config_backup::merge_secrets(&mut config, stored, &mut summary);
    let mut problems = config_backup::validate(&config);
    if let Err(problem) = config_backup::check_categories(&config.categories) {
        problems.push(problem);
    }
    if !problems.is_empty() {
        return Err(ApiError::from((
            StatusCode::BAD_REQUEST,
            format!(
                "the backup was refused and nothing was changed: {}",
                problems.join("; ")
            ),
        )));
    }
    Ok(ParsedBackup {
        document,
        config,
        summary,
    })
}

/// Settings this process reads only at start-up, so the operator knows a
/// restart is needed for them to take effect. The live settings — servers,
/// categories, download directories (new jobs only), speed limit, disk guard,
/// retention, concurrency, notifications and post-processing scripts — are
/// applied at once and are not listed.
pub(crate) fn restart_required(
    previous: &nzb_web::nzb_core::config::AppConfig,
    next: &nzb_web::nzb_core::config::AppConfig,
) -> Vec<String> {
    let (a, b) = (&previous.general, &next.general);
    let mut keys = Vec::new();
    let mut check = |key: &str, changed: bool| {
        if changed {
            keys.push(key.to_string());
        }
    };
    check("general.listen_addr", a.listen_addr != b.listen_addr);
    check("general.port", a.port != b.port);
    check("general.data_dir", a.data_dir != b.data_dir);
    check("general.watch_dir", a.watch_dir != b.watch_dir);
    check("general.log_level", a.log_level != b.log_level);
    check(
        "general.max_post_processing_jobs",
        a.max_post_processing_jobs != b.max_post_processing_jobs,
    );
    check(
        "general.max_repair_workers",
        a.max_repair_workers != b.max_repair_workers,
    );
    check(
        "general.max_extract_workers",
        a.max_extract_workers != b.max_extract_workers,
    );
    check("general.direct_unpack", a.direct_unpack != b.direct_unpack);
    check(
        "general.early_failure_check",
        a.early_failure_check != b.early_failure_check,
    );
    check(
        "general.required_completion_pct",
        a.required_completion_pct != b.required_completion_pct,
    );
    check(
        "general.article_timeout_secs",
        a.article_timeout_secs != b.article_timeout_secs,
    );
    check(
        "general.max_nested_archive_depth",
        a.max_nested_archive_depth != b.max_nested_archive_depth,
    );
    check(
        "general.local_import_dir",
        a.local_import_dir != b.local_import_dir,
    );
    check(
        "general.fetch_allow_private",
        a.fetch_allow_private != b.fetch_allow_private,
    );
    check(
        "general.fetch_allowed_hosts",
        a.fetch_allowed_hosts != b.fetch_allowed_hosts,
    );
    check(
        "general.group_max_headers",
        a.group_max_headers != b.group_max_headers,
    );
    check(
        "general.shutdown_timeout_secs",
        a.shutdown_timeout_secs != b.shutdown_timeout_secs,
    );
    let (a, b) = (&previous.otel, &next.otel);
    check("otel.enabled", a.enabled != b.enabled);
    check("otel.endpoint", a.endpoint != b.endpoint);
    check("otel.logs_enabled", a.logs_enabled != b.logs_enabled);
    check("otel.logs_endpoint", a.logs_endpoint != b.logs_endpoint);
    check(
        "otel.metrics_enabled",
        a.metrics_enabled != b.metrics_enabled,
    );
    check(
        "otel.metrics_endpoint",
        a.metrics_endpoint != b.metrics_endpoint,
    );
    check("otel.service_name", a.service_name != b.service_name);
    check(
        "config_version",
        previous.config_version != next.config_version,
    );
    keys
}

/// The JSON both routes return: what the file holds, what restoring it did or
/// would do, and which settings wait for a restart.
fn backup_report(
    parsed: &ParsedBackup,
    restart_keys: Vec<String>,
    restored: bool,
) -> serde_json::Value {
    serde_json::json!({
        "status": true,
        "restored": restored,
        "backup": {
            "backup_version": parsed.document.backup.backup_version,
            "created_at": parsed.document.backup.created_at,
            "app_version": parsed.document.backup.app_version,
            "secrets_included": parsed.document.backup.secrets_included,
        },
        "summary": parsed.summary,
        "restart_required": restart_keys,
    })
}

/// POST /api/config/backup/preview -- Check a backup without applying it.
///
/// Returns the same summary a restore would, including the secrets a masked
/// file would keep and the settings that would need a restart. A file that
/// would be refused is reported as a 400 here too, so the UI can show the
/// reason before the operator commits to it.
#[utoipa::path(
    post,
    path = "/api/config/backup/preview",
    tag = "config",
    summary = "Check a configuration backup without applying it",
    description = "Parses and validates a TOML backup and returns the same summary a restore would, including secrets a masked file keeps and settings that would need a restart. Nothing is written.",
    request_body(content_type = "application/toml", description = "A backup file produced by `GET /api/config/backup`", content = String),
    responses(
        (status = 200, description = "What the backup holds and what restoring it would do", body = serde_json::Value),
        (status = 400, description = "The backup is invalid or would be refused", body = ApiError),
        (status = 401, description = "Not authenticated", body = ApiError),
        (status = 413, description = "The backup file is larger than the accepted limit", body = ApiError),
    ),
)]
pub async fn h_config_backup_preview(
    State(state): State<Arc<AppState>>,
    body: axum::body::Bytes,
) -> Result<Json<serde_json::Value>, ApiError> {
    let stored = state.config();
    let parsed = parse_and_prepare(&body, &stored)?;
    let restart_keys = restart_required(&stored, &parsed.config);
    Ok(Json(backup_report(&parsed, restart_keys, false)))
}

/// POST /api/config/backup/restore -- Replace the configuration from a backup.
///
/// The whole file layer is replaced, so a setting absent from the backup
/// returns to its default. A masked secret keeps the value this install
/// already holds; a real one replaces it. Nothing is written unless the whole
/// document parses, validates and can be saved, and the live queue manager is
/// updated from the committed configuration afterwards.
#[utoipa::path(
    post,
    path = "/api/config/backup/restore",
    tag = "config",
    summary = "Replace the configuration from a backup",
    description = "Replaces the whole file layer from a TOML backup. A setting absent from the backup returns to its default; a masked secret keeps the value this install already holds. Nothing is written unless the whole document parses, validates and can be saved.",
    request_body(content_type = "application/toml", description = "A backup file produced by `GET /api/config/backup`", content = String),
    responses(
        (status = 200, description = "What the backup held and which settings wait for a restart", body = serde_json::Value),
        (status = 400, description = "The backup is invalid or would be refused", body = ApiError),
        (status = 401, description = "Not authenticated", body = ApiError),
        (status = 413, description = "The backup file is larger than the accepted limit", body = ApiError),
    ),
)]
pub async fn h_config_backup_restore(
    State(state): State<Arc<AppState>>,
    body: axum::body::Bytes,
) -> Result<Json<serde_json::Value>, ApiError> {
    let stored = state.config();
    let parsed = parse_and_prepare(&body, &stored)?;
    let mut restart_keys = Vec::new();
    state.update_config_with(|config| {
        restart_keys = restart_required(config, &parsed.config);
        *config = parsed.config.clone();
        Ok::<_, ApiError>(())
    })?;
    apply_restored_runtime(&state);
    tracing::info!(
        servers = parsed.summary.server_count,
        categories = parsed.summary.category_count,
        rss_feeds = parsed.summary.rss_feed_count,
        secrets_kept = parsed.summary.secrets_kept,
        restart_keys = restart_keys.len(),
        "configuration restored from a backup"
    );
    Ok(Json(backup_report(&parsed, restart_keys, true)))
}

/// Push a restored configuration into the running queue manager, using the
/// same setters the individual settings routes use.
fn apply_restored_runtime(state: &Arc<AppState>) {
    let config = state.config();
    state.queue_manager.update_servers(config.servers.clone());
    state
        .queue_manager
        .set_incomplete_dir(config.general.incomplete_dir.clone());
    state
        .queue_manager
        .set_complete_dir(config.general.complete_dir.clone());
    state
        .queue_manager
        .set_categories(config.categories.clone());
    state
        .queue_manager
        .set_max_active_downloads(config.general.max_active_downloads);
    state
        .queue_manager
        .set_history_retention(config.general.history_retention);
    state
        .queue_manager
        .set_speed_limit(config.general.speed_limit_bps);
    state
        .queue_manager
        .set_min_free_space(config.general.min_free_space_bytes);
    state
        .queue_manager
        .set_auto_sort_remaining_pct(config.general.auto_sort_remaining_pct);
    state
        .queue_manager
        .set_duplicate_policy(config.general.duplicate_policy);
    state.queue_manager.set_postproc_scripts(
        config.general.scripts_dir.clone(),
        config.general.script_success.clone(),
        config.general.script_failure.clone(),
        config.general.script_timeout_secs,
        config.general.script_max_output_bytes,
    );
    state.queue_manager.set_notifications(&config.notifications);
    state
        .queue_manager
        .set_queue_complete_persist(config.general.queue_complete_pers);
    state
        .queue_manager
        .set_defer_par2_volumes(config.par2.defer_volumes);
    state
        .queue_manager
        .set_trust_inflight_verify(config.par2.trust_inflight_verify);
}

/// GET /api/config/sabnzbd-api-key -- Return the current SABnzbd API key.
///
/// This route is protected by the native API authentication middleware. The
/// general config endpoint intentionally remains useful for regular settings,
/// while this dedicated route makes the sensitive-value access explicit.
#[utoipa::path(
    get,
    path = "/api/config/sabnzbd-api-key",
    tag = "config",
    summary = "Read the SABnzbd API key",
    responses(
        (status = 200, description = "The configured key, or null when unset", body = SabApiKeyResponse),
        (status = 401, description = "Not authenticated", body = ApiError),
    ),
)]
pub async fn h_sab_api_key_get(
    State(state): State<Arc<AppState>>,
) -> Result<Json<SabApiKeyResponse>, ApiError> {
    Ok(Json(SabApiKeyResponse {
        api_key: state.config().general.api_key.clone(),
    }))
}

/// POST /api/config/sabnzbd-api-key/rotate -- Generate, persist, and return a
/// replacement SABnzbd API key. The previous key stops working immediately.
#[utoipa::path(
    post,
    path = "/api/config/sabnzbd-api-key/rotate",
    tag = "config",
    summary = "Rotate the SABnzbd API key",
    responses(
        (status = 200, description = "The newly generated key", body = SabApiKeyResponse),
        (status = 401, description = "Not authenticated", body = ApiError),
    ),
)]
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
#[utoipa::path(
    get,
    path = "/api/config/servers",
    tag = "servers",
    summary = "List configured NNTP servers",
    responses(
        (status = 200, description = "Servers in configuration order, passwords masked", body = Vec<ServerConfig>),
        (status = 401, description = "Not authenticated", body = ApiError),
    ),
)]
pub async fn h_servers_list(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Vec<ServerConfig>>, ApiError> {
    Ok(Json(masked_config(&state.config()).servers))
}

fn masked_config(
    config: &nzb_web::nzb_core::config::AppConfig,
) -> nzb_web::nzb_core::config::AppConfig {
    config_backup::mask_secrets(config)
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
#[utoipa::path(
    post,
    path = "/api/config/servers",
    tag = "servers",
    summary = "Add an NNTP server",
    request_body = ServerConfig,
    responses(
        (status = 200, description = "Created server, password masked", body = ServerConfig),
        (status = 400, description = "The server config is invalid", body = ApiError),
        (status = 401, description = "Not authenticated", body = ApiError),
        (status = 409, description = "A server with that id already exists", body = ApiError),
    ),
)]
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
pub const PASSWORD_MASK: &str = nzb_web::nzb_core::config_backup::SECRET_MASK;

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
#[utoipa::path(
    put,
    path = "/api/config/servers/{id}",
    tag = "servers",
    summary = "Update an NNTP server",
    description = "Applies a partial patch. A masked or empty password, and an unchanged proxy URL, keep the stored values.",
    params(("id" = String, Path, description = "Server id")),
    request_body = serde_json::Value,
    responses(
        (status = 200, description = "Server updated", body = SimpleResponse),
        (status = 400, description = "The patch is invalid", body = ApiError),
        (status = 401, description = "Not authenticated", body = ApiError),
        (status = 404, description = "Server not found", body = ApiError),
    ),
)]
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
#[utoipa::path(
    delete,
    path = "/api/config/servers/{id}",
    tag = "servers",
    summary = "Delete an NNTP server",
    params(("id" = String, Path, description = "Server id")),
    responses(
        (status = 200, description = "Server deleted", body = SimpleResponse),
        (status = 401, description = "Not authenticated", body = ApiError),
        (status = 404, description = "Server not found", body = ApiError),
    ),
)]
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
#[utoipa::path(
    post,
    path = "/api/config/servers/{id}/test",
    tag = "servers",
    summary = "Test a configured server",
    params(("id" = String, Path, description = "Server id")),
    responses(
        (status = 200, description = "Connection test result", body = ServerTestResponse),
        (status = 401, description = "Not authenticated", body = ApiError),
        (status = 404, description = "Server not found", body = ApiError),
    ),
)]
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

#[derive(Serialize, ToSchema)]
pub struct ServerTestResponse {
    pub success: bool,
    pub message: String,
}

/// POST /api/config/servers/test-config -- Test a server config without saving.
///
/// When the body names an existing server and carries no new password (empty,
/// absent or the mask), the stored password is used, matching update.
#[utoipa::path(
    post,
    path = "/api/config/servers/test-config",
    tag = "servers",
    summary = "Test a server config without saving it",
    description = "When the body names an existing server and carries no new password, the stored password is used.",
    request_body = ServerConfig,
    responses(
        (status = 200, description = "Connection test result", body = ServerTestResponse),
        (status = 400, description = "The server config is invalid", body = ApiError),
        (status = 401, description = "Not authenticated", body = ApiError),
    ),
)]
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
#[utoipa::path(
    get,
    path = "/api/history/{id}/logs",
    tag = "history",
    summary = "Read a history entry's persisted log",
    params(("id" = String, Path, description = "History entry id")),
    responses(
        (status = 200, description = "Log entries", body = LogResponse),
        (status = 401, description = "Not authenticated", body = ApiError),
        (status = 404, description = "Entry not found", body = ApiError),
    ),
)]
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
#[utoipa::path(
    get,
    path = "/api/config/categories",
    tag = "categories",
    summary = "List categories",
    responses(
        (status = 200, description = "Categories in configuration order", body = Vec<CategoryConfig>),
        (status = 401, description = "Not authenticated", body = ApiError),
    ),
)]
pub async fn h_categories_list(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Vec<nzb_web::nzb_core::config::CategoryConfig>>, ApiError> {
    Ok(Json(state.config().categories.clone()))
}

/// POST /api/config/categories -- Add a new category.
#[utoipa::path(
    post,
    path = "/api/config/categories",
    tag = "categories",
    summary = "Add a category",
    request_body = CategoryConfig,
    responses(
        (status = 200, description = "Category created", body = CategoryConfig),
        (status = 400, description = "The category is invalid", body = ApiError),
        (status = 401, description = "Not authenticated", body = ApiError),
        (status = 409, description = "A category with that name already exists", body = ApiError),
    ),
)]
pub async fn h_category_add(
    State(state): State<Arc<AppState>>,
    Json(cat): Json<CategoryConfig>,
) -> Result<impl IntoResponse, ApiError> {
    cat.validate().map_err(ApiError::bad_request)?;
    validate_category_output_dir(&state, &cat, None)?;
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
#[utoipa::path(
    put,
    path = "/api/config/categories/{name}",
    tag = "categories",
    summary = "Update a category",
    params(("name" = String, Path, description = "Category name")),
    request_body = CategoryConfig,
    responses(
        (status = 200, description = "Category updated", body = serde_json::Value),
        (status = 400, description = "The category is invalid", body = ApiError),
        (status = 401, description = "Not authenticated", body = ApiError),
        (status = 404, description = "Category not found", body = ApiError),
    ),
)]
pub async fn h_category_update(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    Json(cat): Json<CategoryConfig>,
) -> Result<Json<serde_json::Value>, ApiError> {
    cat.validate().map_err(ApiError::bad_request)?;
    let previous = state
        .config()
        .categories
        .iter()
        .find(|c| c.name == name)
        .and_then(|c| c.output_dir.clone());
    validate_category_output_dir(&state, &cat, previous.as_deref())?;
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

/// Check a category's output folder the way the general directories are
/// checked, when it is new or changed. A relative folder is resolved below
/// the complete directory, as enqueue resolves it. An unchanged folder is not
/// re-checked, so editing another field never fails on it.
fn validate_category_output_dir(
    state: &AppState,
    cat: &CategoryConfig,
    previous: Option<&std::path::Path>,
) -> Result<(), ApiError> {
    let Some(output_dir) = cat.output_dir.as_deref() else {
        return Ok(());
    };
    if previous == Some(output_dir) {
        return Ok(());
    }
    let root = nzb_web::nzb_core::path::category_output_root(
        &state.queue_manager.complete_dir(),
        output_dir,
    )
    .ok_or_else(|| general_field_error("output_dir", "category output path is unsafe"))?;
    validate_writable_dir("output_dir", &root)
}

/// DELETE /api/config/categories/{name} -- Delete a category.
#[utoipa::path(
    delete,
    path = "/api/config/categories/{name}",
    tag = "categories",
    summary = "Delete a category",
    params(("name" = String, Path, description = "Category name")),
    responses(
        (status = 200, description = "Category deleted", body = serde_json::Value),
        (status = 401, description = "Not authenticated", body = ApiError),
        (status = 404, description = "Category not found", body = ApiError),
    ),
)]
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
#[utoipa::path(
    put,
    path = "/api/config/history-retention",
    tag = "config",
    summary = "Set history retention",
    request_body = HistoryRetentionBody,
    responses(
        (status = 200, description = "Retention updated", body = SimpleResponse),
        (status = 400, description = "Retention is invalid", body = ApiError),
        (status = 401, description = "Not authenticated", body = ApiError),
    ),
)]
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
#[utoipa::path(
    get,
    path = "/api/config/history-retention",
    tag = "config",
    summary = "Read history retention",
    responses(
        (status = 200, description = "Configured retention", body = HistoryRetentionBody),
        (status = 401, description = "Not authenticated", body = ApiError),
    ),
)]
pub async fn h_history_retention_get(
    State(state): State<Arc<AppState>>,
) -> Result<Json<HistoryRetentionBody>, ApiError> {
    let config = state.config();
    Ok(Json(HistoryRetentionBody {
        retention: config.general.history_retention,
    }))
}

/// PUT /api/config/max-active-downloads -- Update max concurrent downloads.
#[utoipa::path(
    put,
    path = "/api/config/max-active-downloads",
    tag = "config",
    summary = "Set the concurrent download limit",
    request_body = MaxActiveDownloadsBody,
    responses(
        (status = 200, description = "Limit updated", body = SimpleResponse),
        (status = 400, description = "Limit is out of range", body = ApiError),
        (status = 401, description = "Not authenticated", body = ApiError),
    ),
)]
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
#[utoipa::path(
    get,
    path = "/api/config/max-active-downloads",
    tag = "config",
    summary = "Read the concurrent download limit",
    responses(
        (status = 200, description = "Configured limit", body = MaxActiveDownloadsBody),
        (status = 401, description = "Not authenticated", body = ApiError),
    ),
)]
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

#[derive(Serialize, ToSchema)]
pub struct SpeedLimitResponse {
    pub speed_limit_bps: u64,
}

/// GET /api/config/speed-limit -- Get current speed limit.
#[utoipa::path(
    get,
    path = "/api/config/speed-limit",
    tag = "config",
    summary = "Read the speed limit",
    responses(
        (status = 200, description = "Configured cap in bytes/sec (0 = unlimited)", body = SpeedLimitResponse),
        (status = 401, description = "Not authenticated", body = ApiError),
    ),
)]
pub async fn h_get_speed_limit(
    State(state): State<Arc<AppState>>,
) -> Result<Json<SpeedLimitResponse>, ApiError> {
    Ok(Json(SpeedLimitResponse {
        speed_limit_bps: state.queue_manager.get_speed_limit(),
    }))
}

#[derive(Deserialize, ToSchema)]
pub struct SetSpeedLimitBody {
    /// Cap in bytes/sec; 0 means unlimited.
    pub speed_limit_bps: u64,
}

/// PUT /api/config/speed-limit -- Set download speed limit.
#[utoipa::path(
    put,
    path = "/api/config/speed-limit",
    tag = "config",
    summary = "Set the speed limit",
    request_body = SetSpeedLimitBody,
    responses(
        (status = 200, description = "Limit applied and persisted", body = serde_json::Value),
        (status = 401, description = "Not authenticated", body = ApiError),
    ),
)]
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

#[derive(Serialize, ToSchema)]
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
#[utoipa::path(
    get,
    path = "/api/config/schedule",
    tag = "config",
    summary = "Read the download schedule and what it is doing now",
    responses(
        (status = 200, description = "The saved schedule and its current effect", body = ScheduleResponse),
        (status = 401, description = "Not authenticated", body = ApiError),
    ),
)]
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

#[derive(Deserialize, ToSchema)]
pub struct ScheduleUpdate {
    pub enabled: bool,
    pub rules: Vec<ScheduleRule>,
}

/// PUT /api/config/schedule -- Replace the schedule.
///
/// Every rule is validated before anything is saved, so a bad rule never
/// half-applies. Rules without an id get one; ids stay stable across edits so
/// an active window survives a save.
#[utoipa::path(
    put,
    path = "/api/config/schedule",
    tag = "config",
    summary = "Replace the download schedule",
    description = "Validates every rule before saving any of them. Rules without an id are given one; existing ids stay stable so an active window survives a save.",
    request_body = ScheduleUpdate,
    responses(
        (status = 200, description = "The saved schedule and its current effect", body = ScheduleResponse),
        (status = 400, description = "A rule is invalid", body = ApiError),
        (status = 401, description = "Not authenticated", body = ApiError),
    ),
)]
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
        .apply_schedule(&decision, config.schedule.enabled);
    state.schedule_wake.notify_one();

    h_schedule_get(State(state)).await
}

// ---------------------------------------------------------------------------
// Disk guards handlers
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, ToSchema)]
pub struct DiskGuardsBody {
    /// Pause downloads when free space drops below this.
    pub min_free_space_bytes: u64,
    /// Abandon a job early when its slice map shows it cannot succeed.
    pub abort_hopeless: bool,
}

/// GET /api/config/disk-guards -- Get disk guard settings.
#[utoipa::path(
    get,
    path = "/api/config/disk-guards",
    tag = "config",
    summary = "Read the disk guard settings",
    responses(
        (status = 200, description = "Configured guards", body = DiskGuardsBody),
        (status = 401, description = "Not authenticated", body = ApiError),
    ),
)]
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
#[utoipa::path(
    put,
    path = "/api/config/disk-guards",
    tag = "config",
    summary = "Set the disk guard settings",
    request_body = DiskGuardsBody,
    responses(
        (status = 200, description = "Guards applied and persisted", body = SimpleResponse),
        (status = 401, description = "Not authenticated", body = ApiError),
    ),
)]
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

#[derive(Serialize, Deserialize, ToSchema)]
pub struct DebugLoggingBody {
    /// The saved choice.
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
#[utoipa::path(
    get,
    path = "/api/config/debug-logging",
    tag = "config",
    summary = "Read the debug logging state",
    description = "Reports the effective state: an environment or command-line log filter wins over the saved choice, and `overridden` says so.",
    responses(
        (status = 200, description = "Saved choice and effective state", body = DebugLoggingBody),
        (status = 401, description = "Not authenticated", body = ApiError),
    ),
)]
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
#[utoipa::path(
    put,
    path = "/api/config/debug-logging",
    tag = "config",
    summary = "Turn debug logging on or off",
    request_body = DebugLoggingBody,
    responses(
        (status = 200, description = "Toggle applied", body = DebugLoggingStatus),
        (status = 400, description = "An environment or command-line override owns the filter", body = ApiError),
        (status = 401, description = "Not authenticated", body = ApiError),
    ),
)]
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

#[derive(Serialize, ToSchema)]
pub struct DebugLoggingStatus {
    pub status: bool,
    #[serde(flatten)]
    pub state: DebugLoggingBody,
}

// ---------------------------------------------------------------------------
// RSS feed handlers
// ---------------------------------------------------------------------------

/// GET /api/config/rss-feeds -- List RSS feeds.
#[utoipa::path(
    get,
    path = "/api/config/rss-feeds",
    tag = "rss",
    summary = "List RSS feeds",
    responses(
        (status = 200, description = "Configured feeds", body = Vec<RssFeedConfig>),
        (status = 401, description = "Not authenticated", body = ApiError),
    ),
)]
pub async fn h_rss_feeds_list(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Vec<RssFeedConfig>>, ApiError> {
    let config = state.config();
    Ok(Json(config.rss_feeds.clone()))
}

/// POST /api/config/rss-feeds -- Add an RSS feed.
#[utoipa::path(
    post,
    path = "/api/config/rss-feeds",
    tag = "rss",
    summary = "Add an RSS feed",
    request_body = RssFeedConfig,
    responses(
        (status = 200, description = "Feed created", body = RssFeedConfig),
        (status = 400, description = "The feed or its filter regex is invalid", body = ApiError),
        (status = 401, description = "Not authenticated", body = ApiError),
        (status = 409, description = "A feed with that name already exists", body = ApiError),
    ),
)]
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
#[utoipa::path(
    put,
    path = "/api/config/rss-feeds/{name}",
    tag = "rss",
    summary = "Update an RSS feed",
    params(("name" = String, Path, description = "Feed name")),
    request_body = RssFeedConfig,
    responses(
        (status = 200, description = "Feed updated", body = serde_json::Value),
        (status = 400, description = "The feed or its filter regex is invalid", body = ApiError),
        (status = 401, description = "Not authenticated", body = ApiError),
        (status = 404, description = "Feed not found", body = ApiError),
    ),
)]
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
#[utoipa::path(
    delete,
    path = "/api/config/rss-feeds/{name}",
    tag = "rss",
    summary = "Delete an RSS feed",
    params(("name" = String, Path, description = "Feed name")),
    responses(
        (status = 200, description = "Feed deleted", body = serde_json::Value),
        (status = 401, description = "Not authenticated", body = ApiError),
        (status = 404, description = "Feed not found", body = ApiError),
    ),
)]
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

#[derive(Deserialize, Default, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct RssItemsQuery {
    /// Only items from this feed.
    pub feed: Option<String>,
    /// Maximum items to return (default 500).
    pub limit: Option<usize>,
}

/// GET /api/rss/items -- List RSS feed items.
#[utoipa::path(
    get,
    path = "/api/rss/items",
    tag = "rss",
    summary = "List discovered RSS items",
    params(RssItemsQuery),
    responses(
        (status = 200, description = "Newest items first", body = Vec<RssItem>),
        (status = 401, description = "Not authenticated", body = ApiError),
    ),
)]
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
#[utoipa::path(
    post,
    path = "/api/rss/items/{id}/download",
    tag = "rss",
    summary = "Download a discovered RSS item",
    params(("id" = String, Path, description = "RSS item id")),
    responses(
        (status = 200, description = "Item admitted and marked downloaded", body = SimpleResponse),
        (status = 401, description = "Not authenticated", body = ApiError),
        (status = 404, description = "Item not found", body = ApiError),
        (status = 409, description = "The item's NZB duplicates a live job", body = ApiError),
    ),
)]
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

#[derive(Deserialize, ToSchema)]
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
#[utoipa::path(
    get,
    path = "/api/rss/rules",
    tag = "rss",
    summary = "List RSS download rules",
    responses(
        (status = 200, description = "Configured rules", body = Vec<RssRule>),
        (status = 401, description = "Not authenticated", body = ApiError),
    ),
)]
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
#[utoipa::path(
    post,
    path = "/api/rss/rules",
    tag = "rss",
    summary = "Add an RSS download rule",
    request_body = RssRuleBody,
    responses(
        (status = 200, description = "Rule created", body = serde_json::Value),
        (status = 400, description = "The rule or its match regex is invalid", body = ApiError),
        (status = 401, description = "Not authenticated", body = ApiError),
    ),
)]
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
#[utoipa::path(
    put,
    path = "/api/rss/rules/{id}",
    tag = "rss",
    summary = "Update an RSS download rule",
    params(("id" = String, Path, description = "Rule id")),
    request_body = RssRuleBody,
    responses(
        (status = 200, description = "Rule updated", body = serde_json::Value),
        (status = 400, description = "The rule or its match regex is invalid", body = ApiError),
        (status = 401, description = "Not authenticated", body = ApiError),
        (status = 404, description = "Rule not found", body = ApiError),
    ),
)]
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
#[utoipa::path(
    delete,
    path = "/api/rss/rules/{id}",
    tag = "rss",
    summary = "Delete an RSS download rule",
    params(("id" = String, Path, description = "Rule id")),
    responses(
        (status = 200, description = "Rule deleted", body = SimpleResponse),
        (status = 401, description = "Not authenticated", body = ApiError),
        (status = 404, description = "Rule not found", body = ApiError),
    ),
)]
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
#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct UpdateGeneralBody {
    pub incomplete_dir: Option<String>,
    pub complete_dir: Option<String>,
    pub data_dir: Option<String>,
    pub watch_dir: Option<String>,
    /// Dead setting: nothing reads it. Accepted so older clients still get a
    /// 200, but never stored (see the warning in `h_general_update`).
    #[serde(default)]
    #[schema(ignore)]
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
    /// Address the HTTP server binds. Read at startup: restart required.
    pub listen_addr: Option<String>,
    /// Port the HTTP server binds. Read at startup: restart required.
    pub port: Option<u16>,
    /// `[par2] defer_volumes`; applies to new downloads at once.
    pub par2_defer_volumes: Option<bool>,
    /// `[par2] trust_inflight_verify`; applies at once.
    pub par2_trust_inflight_verify: Option<bool>,
}

/// Response of `PUT /api/config/general`.
#[derive(Serialize, ToSchema)]
pub struct GeneralUpdateResponse {
    pub status: bool,
    /// Every saved setting that waits for a restart, this save's and earlier
    /// ones (see `GET /api/config/restart-pending`).
    pub restart_required: Vec<String>,
}

/// Response of `GET /api/config/restart-pending`.
#[derive(Serialize, ToSchema)]
pub struct RestartPendingResponse {
    /// Config keys saved since startup that take effect only after a restart.
    pub restart_required: Vec<String>,
}

/// Keys saved since this process started that it reads only at startup.
fn pending_restart_keys(state: &AppState) -> Vec<String> {
    restart_required(&state.startup_config(), &state.config())
}

/// GET /api/config/restart-pending -- Settings waiting for a restart.
#[utoipa::path(
    get,
    path = "/api/config/restart-pending",
    tag = "config",
    summary = "List saved settings that wait for a restart",
    responses(
        (status = 200, description = "Keys changed since startup that apply after a restart", body = RestartPendingResponse),
        (status = 401, description = "Not authenticated", body = ApiError),
    ),
)]
pub async fn h_restart_pending(State(state): State<Arc<AppState>>) -> Json<RestartPendingResponse> {
    Json(RestartPendingResponse {
        restart_required: pending_restart_keys(&state),
    })
}

/// Where each `[general]` and `[par2]` setting can be changed, for the
/// settings audit (WI-2079). A test fails when a new setting is added
/// without a row here, so nothing becomes TOML-only by accident.
/// `Ok(route)` is the API the settings UI uses; `Err(reason)` is a setting
/// deliberately left out of the UI, with the reason.
#[cfg(test)]
pub(crate) const SETTINGS_AUDIT: &[(&str, Result<&str, &str>)] = &[
    (
        "general.listen_addr",
        Ok("PUT /api/config/general (restart required)"),
    ),
    (
        "general.port",
        Ok("PUT /api/config/general (restart required)"),
    ),
    (
        "general.api_key",
        Ok("POST /api/config/sabnzbd-api-key/rotate"),
    ),
    (
        "general.incomplete_dir",
        Ok("PUT /api/config/general (new jobs only)"),
    ),
    (
        "general.complete_dir",
        Ok("PUT /api/config/general (new jobs only)"),
    ),
    (
        "general.data_dir",
        Err(
            "holds the database and the config itself; moving it needs the files moved first, so it stays CLI/env/TOML only and is shown read-only",
        ),
    ),
    ("general.speed_limit_bps", Ok("PUT /api/config/speed-limit")),
    ("general.cache_size", Err("dead setting: nothing reads it")),
    (
        "general.log_level",
        Err(
            "the tracing filter is built at startup from --log-level / RUSTNZB_LOG_LEVEL / RUST_LOG; the debug-logging toggle covers the runtime need",
        ),
    ),
    (
        "general.log_level_before_debug",
        Err("internal state of the debug-logging toggle"),
    ),
    (
        "general.log_file",
        Err(
            "logging starts before the config can be edited; read at startup, with --log-file / RUSTNZB_LOG_FILE winning",
        ),
    ),
    (
        "general.history_retention",
        Ok("PUT /api/config/history-retention"),
    ),
    (
        "general.max_active_downloads",
        Ok("PUT /api/config/max-active-downloads"),
    ),
    (
        "general.max_post_processing_jobs",
        Ok("PUT /api/config/general (restart required)"),
    ),
    (
        "general.max_repair_workers",
        Ok("PUT /api/config/general (restart required)"),
    ),
    (
        "general.max_extract_workers",
        Ok("PUT /api/config/general (restart required)"),
    ),
    (
        "general.min_free_space_bytes",
        Ok("PUT /api/config/disk-guards"),
    ),
    (
        "general.watch_dir",
        Ok("PUT /api/config/general (restart required)"),
    ),
    ("general.rss_history_limit", Ok("PUT /api/config/general")),
    (
        "general.direct_unpack",
        Ok("PUT /api/config/general (restart required)"),
    ),
    (
        "general.max_nested_archive_depth",
        Ok("PUT /api/config/general (restart required)"),
    ),
    ("general.abort_hopeless", Ok("PUT /api/config/disk-guards")),
    (
        "general.early_failure_check",
        Ok("PUT /api/config/general (restart required)"),
    ),
    (
        "general.required_completion_pct",
        Ok("PUT /api/config/general (restart required)"),
    ),
    (
        "general.article_timeout_secs",
        Ok("PUT /api/config/general (restart required)"),
    ),
    (
        "general.auto_sort_remaining_pct",
        Ok("PUT /api/config/general"),
    ),
    (
        "general.rss_downloaded_item_expiry_days",
        Ok("PUT /api/config/general"),
    ),
    ("general.duplicate_policy", Ok("PUT /api/config/general")),
    (
        "general.local_import_dir",
        Err(
            "security boundary for SABnzbd addlocalfile: widening it from the web UI would let a stolen session read arbitrary files, so it stays TOML/env only",
        ),
    ),
    (
        "general.queue_complete_pers",
        Err("follow-up: SABnzbd queue-complete action persistence; only SABnzbd clients use it"),
    ),
    ("general.scripts_dir", Ok("PUT /api/config/general")),
    ("general.script_success", Ok("PUT /api/config/general")),
    ("general.script_failure", Ok("PUT /api/config/general")),
    ("general.script_timeout_secs", Ok("PUT /api/config/general")),
    (
        "general.script_max_output_bytes",
        Ok("PUT /api/config/general"),
    ),
    (
        "general.fetch_allow_private",
        Err(
            "SSRF guard: opening server-side fetches to the LAN from the web UI would let a stolen session pivot into the network, so it stays TOML/env only",
        ),
    ),
    (
        "general.fetch_allowed_hosts",
        Err("SSRF guard, as fetch_allow_private"),
    ),
    (
        "general.group_max_headers",
        Err("follow-up: newsgroup browsing settings get their own panel"),
    ),
    (
        "general.shutdown_timeout_secs",
        Err("follow-up: process lifecycle setting, rarely changed; restart required"),
    ),
    (
        "par2.defer_volumes",
        Ok("PUT /api/config/general (par2_defer_volumes)"),
    ),
    (
        "par2.trust_inflight_verify",
        Ok("PUT /api/config/general (par2_trust_inflight_verify)"),
    ),
];

/// The uid this process runs as, for "not writable by uid N" errors.
fn process_identity() -> String {
    #[cfg(unix)]
    {
        // SAFETY: geteuid has no preconditions and cannot fail.
        let uid = unsafe { libc::geteuid() };
        format!("uid {uid}")
    }
    #[cfg(not(unix))]
    {
        "this process".to_string()
    }
}

/// Check, in this process's view of the filesystem (inside the container),
/// that a download directory exists and that a test file can be created in it
/// and removed again. The directory is never created: a missing one is
/// usually a volume that is not mounted, and creating it would hide that.
pub(crate) fn validate_writable_dir(field: &str, path: &std::path::Path) -> Result<(), ApiError> {
    let shown = path.display();
    if path.as_os_str().is_empty() {
        return Err(general_field_error(field, "must not be empty"));
    }
    if !path.is_absolute() {
        return Err(general_field_error(
            field,
            &format!("`{shown}` must be an absolute path"),
        ));
    }
    match std::fs::metadata(path) {
        Ok(meta) if meta.is_dir() => {}
        Ok(_) => {
            return Err(general_field_error(
                field,
                &format!("`{shown}` is not a directory"),
            ));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(general_field_error(
                field,
                &format!(
                    "`{shown}` does not exist in the container (not mounted?); rustnzb does not create it"
                ),
            ));
        }
        Err(error) => {
            return Err(general_field_error(
                field,
                &format!(
                    "`{shown}` cannot be read by {}: {error}",
                    process_identity()
                ),
            ));
        }
    }
    let probe = path.join(format!(
        ".rustnzb-write-test-{}",
        uuid::Uuid::new_v4().simple()
    ));
    if let Err(error) = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)
    {
        return Err(general_field_error(
            field,
            &format!(
                "`{shown}` is not writable by {}: {error}",
                process_identity()
            ),
        ));
    }
    std::fs::remove_file(&probe).map_err(|error| {
        general_field_error(
            field,
            &format!(
                "a test file in `{shown}` could not be removed by {}: {error}",
                process_identity()
            ),
        )
    })
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
    if body.port == Some(0) {
        return Err(general_field_error("port", "must be between 1 and 65535"));
    }
    if let Some(addr) = body.listen_addr.as_deref()
        && addr.trim().parse::<std::net::IpAddr>().is_err()
    {
        return Err(general_field_error(
            "listen_addr",
            "must be an IP address such as 0.0.0.0, 127.0.0.1 or ::",
        ));
    }
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
#[utoipa::path(
    put,
    path = "/api/config/general",
    tag = "config",
    summary = "Update general settings",
    description = "Applies only the fields present in the body. Settings with their own endpoints (`speed_limit_bps`, `min_free_space_bytes`, `abort_hopeless`) are not accepted here. Changing settings the engine reads at startup is persisted immediately but takes effect on the next process start.",
    request_body = UpdateGeneralBody,
    responses(
        (status = 200, description = "General settings updated", body = GeneralUpdateResponse),
        (status = 400, description = "A field is unknown or out of range", body = ApiError),
        (status = 401, description = "Not authenticated", body = ApiError),
    ),
)]
pub async fn h_general_update(
    State(state): State<Arc<AppState>>,
    body: Result<Json<UpdateGeneralBody>, JsonRejection>,
) -> Result<Json<GeneralUpdateResponse>, ApiError> {
    // Map extractor failures (unknown field, wrong type) to a 400 that names
    // the offending field, instead of axum's plain-text 422.
    let Json(body) =
        body.map_err(|rejection| ApiError::from((StatusCode::BAD_REQUEST, rejection.body_text())))?;
    validate_general_update(&body)?;
    // A changed download directory must exist in the container and be
    // writable before it is saved. It applies to new jobs only: queued and
    // history entries keep the paths they were created with.
    let current = state.config();
    for (field, value, existing) in [
        (
            "complete_dir",
            body.complete_dir.as_deref(),
            &current.general.complete_dir,
        ),
        (
            "incomplete_dir",
            body.incomplete_dir.as_deref(),
            &current.general.incomplete_dir,
        ),
    ] {
        if let Some(dir) = value
            && std::path::Path::new(dir) != existing.as_path()
        {
            validate_writable_dir(field, std::path::Path::new(dir))?;
        }
    }
    let dirs_changed = body.complete_dir.is_some() || body.incomplete_dir.is_some();
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
        if let Some(addr) = body.listen_addr.as_deref() {
            config.general.listen_addr = addr.trim().to_string();
        }
        if let Some(port) = body.port {
            config.general.port = port;
        }
        if let Some(enabled) = body.par2_defer_volumes {
            state.queue_manager.set_defer_par2_volumes(enabled);
            config.par2.defer_volumes = enabled;
        }
        if let Some(enabled) = body.par2_trust_inflight_verify {
            state.queue_manager.set_trust_inflight_verify(enabled);
            config.par2.trust_inflight_verify = enabled;
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
    if dirs_changed {
        // The effective value: an override keeps winning over the file.
        let config = state.config();
        state
            .queue_manager
            .set_complete_dir(config.general.complete_dir.clone());
        state
            .queue_manager
            .set_incomplete_dir(config.general.incomplete_dir.clone());
    }
    Ok(Json(GeneralUpdateResponse {
        status: true,
        restart_required: pending_restart_keys(&state),
    }))
}

// ---------------------------------------------------------------------------
// Server health check handler
// ---------------------------------------------------------------------------

#[derive(Serialize, ToSchema)]
pub struct ServerHealthResult {
    pub id: String,
    pub name: String,
    pub success: bool,
    pub message: String,
}

/// GET /api/config/servers/health -- Test all servers and return health status.
#[utoipa::path(
    get,
    path = "/api/config/servers/health",
    tag = "servers",
    summary = "Test every configured server",
    responses(
        (status = 200, description = "One result per server, in configuration order", body = Vec<ServerHealthResult>),
        (status = 401, description = "Not authenticated", body = ApiError),
    ),
)]
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
#[utoipa::path(
    get,
    path = "/api/config/servers/stats",
    tag = "servers",
    summary = "Per-server article statistics",
    description = "Lifetime and rolling 24-hour, 7-day and 30-day totals per server.",
    responses(
        (status = 200, description = "One row per configured server", body = Vec<nzb_web::ServerStatsData>),
        (status = 401, description = "Not authenticated", body = ApiError),
    ),
)]
pub async fn h_server_stats(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Vec<nzb_web::ServerStatsData>>, ApiError> {
    let servers = state.config().servers.clone();
    let stats = state.queue_manager.server_stats_get_all(&servers);
    Ok(Json(stats))
}

/// GET /api/statistics -- Persistent global download and NNTP statistics.
#[utoipa::path(
    get,
    path = "/api/statistics",
    tag = "statistics",
    summary = "Lifetime and rolling download statistics",
    responses(
        (status = 200, description = "Aggregate statistics", body = nzb_web::GlobalStatisticsData),
        (status = 401, description = "Not authenticated", body = ApiError),
    ),
)]
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

#[utoipa::path(
    get,
    path = "/api/health",
    tag = "system",
    summary = "Liveness probe",
    description = "Unauthenticated. Returns 200 while the process is serving.",
    responses(
        (status = 200, description = "Process is alive", body = serde_json::Value),
    ),
)]
pub async fn h_health() -> Json<serde_json::Value> {
    Json(serde_json::json!({"status": "ok"}))
}

/// GET /api/ready -- Readiness probe. The engine is initialised before the
/// listener binds, so this is 200 whenever the process is serving, and 503
/// once shutdown has begun, so a load balancer stops routing new requests.
#[utoipa::path(
    get,
    path = "/api/ready",
    tag = "system",
    summary = "Readiness probe",
    description = "Unauthenticated. 200 while the engine is serving; 503 once shutdown has begun, so a load balancer stops routing new requests.",
    responses(
        (status = 200, description = "Ready (`{\"status\":\"ready\"}`)", body = serde_json::Value),
        (status = 503, description = "Shutting down (`{\"status\":\"shutting_down\"}`)", body = serde_json::Value),
    ),
)]
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

#[derive(Deserialize, Default, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct BrowseDirectoryQuery {
    /// Directory to list; defaults to the complete directory.
    pub path: Option<String>,
}

#[derive(Serialize, ToSchema)]
pub struct BrowseDirectoryResponse {
    pub current: String,
    pub parent: Option<String>,
    pub directories: Vec<String>,
}

/// GET /api/browse-directory -- List subdirectories for the directory picker.
#[utoipa::path(
    get,
    path = "/api/browse-directory",
    tag = "config",
    summary = "List subdirectories for the directory picker",
    description = "Only directories below the configured complete directory are returned.",
    params(BrowseDirectoryQuery),
    responses(
        (status = 200, description = "Directory listing", body = BrowseDirectoryResponse),
        (status = 400, description = "The path is outside the allowed root", body = ApiError),
        (status = 401, description = "Not authenticated", body = ApiError),
    ),
)]
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

#[derive(Deserialize, ToSchema)]
pub struct ChangeCategoryBody {
    /// Destination category.
    pub category: String,
}

/// PUT /api/queue/{id}/category -- Change a job's category.
#[utoipa::path(
    put,
    path = "/api/queue/{id}/category",
    tag = "queue",
    summary = "Change a job's category",
    params(("id" = String, Path, description = "Job id")),
    request_body = ChangeCategoryBody,
    responses(
        (status = 200, description = "Category changed", body = SimpleResponse),
        (status = 401, description = "Not authenticated", body = ApiError),
        (status = 404, description = "Job not found", body = ApiError),
    ),
)]
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

#[derive(Deserialize, ToSchema)]
pub struct BulkActionBody {
    /// Jobs to act on.
    pub ids: Vec<String>,
    pub action: String,
    /// Optional value for priority (int) or category (string).
    pub value: Option<serde_json::Value>,
}

#[derive(Serialize, ToSchema)]
pub struct BulkActionResponse {
    pub status: bool,
    pub succeeded: usize,
    pub failed: usize,
    /// One entry per id that failed, in request order.
    pub failures: Vec<BulkActionFailure>,
}

#[derive(Serialize, ToSchema)]
pub struct BulkActionFailure {
    pub id: String,
    /// Same shape as an API error body (`error_kind`, `human_readable`,
    /// `status`).
    pub error: ApiError,
}

/// Response to `POST /api/history/bulk`. `trashed` is present only when the
/// caller asked for the undo window.
#[derive(Serialize, ToSchema)]
pub struct HistoryBulkResponse {
    pub status: bool,
    pub succeeded: usize,
    pub failed: usize,
    pub failures: Vec<BulkActionFailure>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trashed: Option<HistoryTrashResponse>,
}

/// POST /api/queue/bulk -- Perform an action on multiple jobs.
#[utoipa::path(
    post,
    path = "/api/queue/bulk",
    tag = "queue",
    summary = "Act on several queue jobs",
    description = "One request, one result per id. A per-id failure is reported rather than failing the batch.",
    request_body = BulkActionBody,
    responses(
        (status = 200, description = "Per-id results", body = BulkActionResponse),
        (status = 400, description = "Unknown action or missing value", body = ApiError),
        (status = 401, description = "Not authenticated", body = ApiError),
    ),
)]
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
#[utoipa::path(
    get,
    path = "/api/setup/status",
    tag = "setup",
    summary = "First-boot setup status",
    description = "Unauthenticated before credentials are configured, so the setup wizard can decide whether to open.",
    responses(
        (status = 200, description = "Whether servers and categories exist", body = serde_json::Value),
    ),
)]
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
#[utoipa::path(
    post,
    path = "/api/setup/import-sabnzbd",
    tag = "setup",
    summary = "Parse a SABnzbd ini file",
    description = "Uploads a `sabnzbd.ini` as `multipart/form-data` and returns a preview of what would be imported. Nothing is applied; send the preview to `POST /api/setup/apply`.",
    request_body(content_type = "multipart/form-data", description = "The SABnzbd ini file", content = String),
    responses(
        (status = 200, description = "Import preview", body = sabnzbd_import::SabnzbdImportPreview),
        (status = 400, description = "No file uploaded or the file is not valid UTF-8", body = ApiError),
        (status = 401, description = "Not authenticated", body = ApiError),
    ),
)]
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
#[derive(Deserialize, ToSchema)]
pub struct ImportApiRequest {
    /// Base URL of the running SABnzbd instance.
    pub url: String,
    /// Its API key.
    pub api_key: String,
}

#[utoipa::path(
    post,
    path = "/api/setup/import-sabnzbd-api",
    tag = "setup",
    summary = "Import settings from a running SABnzbd",
    description = "Fetches the configuration from a SABnzbd instance through the configured fetch policy and returns what would be imported. Nothing is applied.",
    request_body = ImportApiRequest,
    responses(
        (status = 200, description = "Import preview", body = sabnzbd_import::SabnzbdImportPreview),
        (status = 400, description = "The request or URL is invalid", body = ApiError),
        (status = 401, description = "Not authenticated", body = ApiError),
        (status = 502, description = "The SABnzbd instance could not be reached", body = ApiError),
    ),
)]
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
#[utoipa::path(
    post,
    path = "/api/setup/apply",
    tag = "setup",
    summary = "Apply a SABnzbd import preview",
    description = "Stores the servers and categories in a preview returned by `POST /api/setup/import-sabnzbd` or `POST /api/setup/import-sabnzbd-api`.",
    request_body = sabnzbd_import::SabnzbdImportPreview,
    responses(
        (status = 200, description = "Import applied", body = serde_json::Value),
        (status = 400, description = "The preview is invalid", body = ApiError),
        (status = 401, description = "Not authenticated", body = ApiError),
    ),
)]
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
#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct DavAddQuery {
    /// NZB id in the DAV pipeline's history.
    pub id: String,
}

#[cfg(feature = "webdav")]
#[derive(Serialize, ToSchema)]
pub struct DavAddResponse {
    pub status: bool,
    pub dav_id: String,
}

// ── DAV pipeline status types ──────────────────────────────────────────────

#[cfg(feature = "webdav")]
#[derive(Serialize, ToSchema)]
pub struct DavQueueEntry {
    pub job_name: String,
    pub queued_at: String,
}

#[cfg(feature = "webdav")]
#[derive(Serialize, ToSchema)]
pub struct DavHistoryEntry {
    pub job_name: String,
    pub status: String,
    pub fail_message: Option<String>,
    pub completed_at: String,
}

#[cfg(feature = "webdav")]
#[derive(Serialize, ToSchema)]
pub struct DavStatusResponse {
    pub queue: Vec<DavQueueEntry>,
    pub history: Vec<DavHistoryEntry>,
}

/// GET /api/dav/status — pipeline queue + history for media library status overlay.
#[cfg(feature = "webdav")]
#[utoipa::path(
    get,
    path = "/api/dav/status",
    tag = "dav",
    summary = "WebDAV pipeline status",
    description = "Queue and recent history of the DAV media library pipeline. Requires a build with the `webdav` feature.",
    responses(
        (status = 200, description = "Pipeline queue and history", body = DavStatusResponse),
        (status = 401, description = "Not authenticated", body = ApiError),
        (status = 500, description = "The WebDAV library is not initialised or the status query failed", body = ApiError),
    ),
)]
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
#[utoipa::path(
    post,
    path = "/api/dav/add",
    tag = "dav",
    summary = "Send a job to the WebDAV pipeline",
    description = "Feeds a completed download's NZB into the WebDAV streaming pipeline. Requires a build with the `webdav` feature.",
    params(DavAddQuery),
    responses(
        (status = 200, description = "Job queued in the DAV pipeline", body = DavAddResponse),
        (status = 401, description = "Not authenticated", body = ApiError),
        (status = 404, description = "Job not found or it has no retained NZB", body = ApiError),
        (status = 500, description = "The WebDAV library is not initialised", body = ApiError),
    ),
)]
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
#[utoipa::path(
    get,
    path = "/api/config/dav",
    tag = "dav",
    summary = "Read the DAV pipeline configuration",
    description = "`password` and `api_key` are masked when set. Requires a build with the `webdav` feature.",
    responses(
        (status = 200, description = "DAV configuration", body = DavConfig),
        (status = 401, description = "Not authenticated", body = ApiError),
    ),
)]
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
#[utoipa::path(
    put,
    path = "/api/config/dav",
    tag = "dav",
    summary = "Update the DAV pipeline configuration",
    description = "Replaces the block as a whole, except for the secrets `password` and `api_key`: absent, empty or masked keeps the stored value, `null` clears it. Requires a build with the `webdav` feature.",
    request_body = serde_json::Value,
    responses(
        (status = 200, description = "Updated DAV configuration", body = serde_json::Value),
        (status = 400, description = "The configuration is invalid", body = ApiError),
        (status = 401, description = "Not authenticated", body = ApiError),
    ),
)]
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
        MAX_RSS_REGEX_LEN, SETTINGS_AUDIT, compile_rss_regex, proxy_url_unchanged,
        restart_required, sanitize_server_config,
    };

    /// Every `[general]` and `[par2]` key has a row in the settings audit,
    /// and every row names a real key.
    #[test]
    fn settings_audit_covers_every_general_and_par2_setting() {
        let mut config = nzb_web::nzb_core::config::AppConfig::default();
        // Skipped when unset; give it a value so the key is serialized.
        config.general.log_level_before_debug = Some("info".into());
        let value = serde_json::to_value(&config).unwrap();
        let mut keys: Vec<String> = Vec::new();
        for section in ["general", "par2"] {
            for key in value[section].as_object().unwrap().keys() {
                keys.push(format!("{section}.{key}"));
            }
        }
        keys.sort();
        let mut audited: Vec<String> = SETTINGS_AUDIT.iter().map(|(k, _)| k.to_string()).collect();
        audited.sort();
        assert_eq!(
            keys, audited,
            "update SETTINGS_AUDIT and docs/settings-audit.md"
        );
        for (key, entry) in SETTINGS_AUDIT {
            let text = match entry {
                Ok(route) | Err(route) => route,
            };
            assert!(!text.trim().is_empty(), "{key} needs a route or a reason");
        }
    }

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

    #[test]
    fn restart_required_names_only_settings_read_at_startup() {
        let mut previous = nzb_web::nzb_core::config::AppConfig::default();
        let mut next = previous.clone();
        // Boot-only settings.
        next.general.port = 9191;
        next.otel.enabled = true;
        // A live setting, which the restore applies without a restart.
        next.general.speed_limit_bps = 42;
        assert_eq!(
            restart_required(&previous, &next),
            vec!["general.port", "otel.enabled"]
        );

        // The same config twice needs no restart at all.
        previous = next.clone();
        assert!(restart_required(&previous, &next).is_empty());
    }
}
