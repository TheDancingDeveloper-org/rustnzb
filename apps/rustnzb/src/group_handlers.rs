//! HTTP handlers for newsgroup browsing.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use axum::Json;
use axum::extract::{FromRequestParts, Path, Query, State};
use axum::http::StatusCode;
use axum::http::request::Parts;
use parking_lot::Mutex;
use serde::Deserialize;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use nzb_web::QueueManager;
use nzb_web::error::{ApiError, WithStatusError};
use nzb_web::nzb_core::NzbError;
use nzb_web::nzb_core::config::ServerConfig;
use nzb_web::nzb_core::db::Database;
use nzb_web::state::AppState;

/// Largest page any group/header/thread listing returns.
pub const MAX_PAGE_LIMIT: usize = 1_000;

fn page_limit(requested: Option<usize>, default: usize) -> usize {
    requested.unwrap_or(default).min(MAX_PAGE_LIMIT)
}

/// Run a database query on the blocking pool.
///
/// Newsgroup queries can scan large header tables while holding the global
/// database mutex; doing that on an async worker thread would stall every
/// other task scheduled on it.
async fn db_blocking<F, R>(state: &AppState, query: F) -> Result<R, ApiError>
where
    F: FnOnce(&Database) -> Result<R, NzbError> + Send + 'static,
    R: Send + 'static,
{
    let qm = state.queue_manager.clone();
    tokio::task::spawn_blocking(move || qm.with_db(query))
        .await
        .map_err(|e| ApiError::from(anyhow::anyhow!("Database task failed: {e}")))?
        .map_err(ApiError::from)
}

/// `Path` extractor whose rejection is a JSON `ApiError` 400 instead of
/// axum's plain-text parser message (which leaks Rust type names such as
/// "Cannot parse `abc` to a `i64`").
pub struct IdPath<T>(pub T);

impl<S, T> FromRequestParts<S> for IdPath<T>
where
    S: Send + Sync,
    T: serde::de::DeserializeOwned + Send,
{
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        Path::<T>::from_request_parts(parts, state)
            .await
            .map(|Path(value)| IdPath(value))
            .map_err(|_| ApiError::bad_request("Invalid id in request path"))
    }
}

#[derive(Deserialize, Default)]
pub struct GroupListQuery {
    pub subscribed: Option<bool>,
    pub search: Option<String>,
    pub limit: Option<usize>,
    pub offset: Option<usize>,
}

#[derive(Deserialize, Default)]
pub struct HeaderListQuery {
    pub search: Option<String>,
    pub limit: Option<usize>,
    pub offset: Option<usize>,
}

fn mark_headers_read<F, E>(header_ids: &[i64], mut mark: F) -> Result<u64, E>
where
    F: FnMut(i64) -> Result<(), E>,
{
    let mut marked = 0u64;
    for &id in header_ids {
        mark(id)?;
        marked += 1;
    }
    Ok(marked)
}

// ---------------------------------------------------------------------------
// Server selection and browse connections
// ---------------------------------------------------------------------------

/// The server newsgroup browsing talks to: the highest-priority (lowest
/// `priority` value) enabled server with a non-zero connection limit. Ties
/// keep config order.
pub fn select_browse_server(servers: &[ServerConfig]) -> Option<&ServerConfig> {
    servers
        .iter()
        .filter(|server| server.enabled && server.connections > 0)
        .min_by_key(|server| server.priority)
}

/// Pick the browse server or fail with a client error.
fn browse_server(state: &AppState) -> Result<ServerConfig, ApiError> {
    let servers = state.queue_manager.get_servers();
    select_browse_server(&servers)
        .cloned()
        .ok_or_else(|| ApiError::bad_request("No enabled servers configured"))
}

/// How long a browse request waits for the server's browse connection
/// before giving up.
const BROWSE_CONNECTION_WAIT: Duration = Duration::from_secs(30);

/// One browse connection per server at a time.
///
/// The download engine keeps every one of a server's `connections` slots
/// open for the lifetime of its workers (see `ConnectionTracker` in
/// nzb-dispatch), so there is no free pool slot to borrow without taking a
/// worker down. Browsing therefore uses its own connection, but never more
/// than one per server: the overshoot of the provider's connection limit is
/// bounded at one, however many browse requests arrive.
fn browse_permits(server_id: &str) -> Arc<Semaphore> {
    static PERMITS: OnceLock<Mutex<HashMap<String, Arc<Semaphore>>>> = OnceLock::new();
    PERMITS
        .get_or_init(Default::default)
        .lock()
        .entry(server_id.to_string())
        .or_insert_with(|| Arc::new(Semaphore::new(1)))
        .clone()
}

async fn acquire_browse_connection(server: &ServerConfig) -> Option<OwnedSemaphorePermit> {
    browse_permits(&server.id).acquire_owned().await.ok()
}

async fn acquire_browse_connection_bounded(
    server: &ServerConfig,
) -> Result<OwnedSemaphorePermit, ApiError> {
    match tokio::time::timeout(BROWSE_CONNECTION_WAIT, acquire_browse_connection(server)).await {
        Ok(Some(permit)) => Ok(permit),
        _ => Err(status_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "The server's browse connection is busy; try again shortly",
        )),
    }
}

fn status_error(status: StatusCode, message: &'static str) -> ApiError {
    None::<()>.with_status_error(status, message).unwrap_err()
}

/// GET /api/groups
pub async fn h_group_list(
    State(state): State<Arc<AppState>>,
    Query(q): Query<GroupListQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let subscribed = q.subscribed.unwrap_or(false);
    let limit = page_limit(q.limit, 100);
    let offset = q.offset.unwrap_or(0);

    let (groups, total) = db_blocking(&state, move |db| {
        let search = q.search.as_deref();
        Ok((
            db.group_list(subscribed, search, limit, offset)?,
            db.group_count(subscribed, search)?,
        ))
    })
    .await?;

    Ok(Json(serde_json::json!({
        "groups": groups, "total": total, "limit": limit, "offset": offset,
    })))
}

/// POST /api/groups/refresh
pub async fn h_group_refresh(
    State(state): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, ApiError> {
    use nzb_web::nzb_core::nzb_nntp::connection::NntpConnection;

    let server = browse_server(&state)?;
    let _connection = acquire_browse_connection_bounded(&server).await?;

    let mut conn = NntpConnection::new("group-refresh".to_string());
    conn.connect(&server)
        .await
        .map_err(|e| ApiError::from(anyhow::anyhow!("Connect failed: {e}")))?;

    let entries = conn
        .list_active(None)
        .await
        .map_err(|e| ApiError::from(anyhow::anyhow!("LIST ACTIVE failed: {e}")))?;
    let _ = conn.quit().await;

    let groups: Vec<(String, u64, u64)> = entries
        .into_iter()
        .map(|e| (e.name, e.high, e.low))
        .collect();

    let count = state
        .queue_manager
        .with_db(|db| db.group_upsert_batch(&groups))
        .map_err(ApiError::from)?;

    Ok(Json(serde_json::json!({
        "status": true, "message": format!("Refreshed {count} groups"), "total": count,
    })))
}

/// GET /api/groups/{id}
pub async fn h_group_get(
    State(state): State<Arc<AppState>>,
    IdPath(id): IdPath<i64>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let group = db_blocking(&state, move |db| db.group_get(id))
        .await?
        .ok_or_else(|| ApiError::not_found("Group not found"))?;
    Ok(Json(serde_json::to_value(group).map_err(|e| {
        ApiError::from(anyhow::anyhow!("Serialisation error: {e}"))
    })?))
}

/// GET /api/groups/{id}/status
pub async fn h_group_status(
    State(state): State<Arc<AppState>>,
    IdPath(id): IdPath<i64>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let (group, total_headers, unread) = db_blocking(&state, move |db| {
        Ok((
            db.group_get(id)?,
            db.header_count(id, None)?,
            db.header_unread_count(id)?,
        ))
    })
    .await?;
    let group = group.ok_or_else(|| ApiError::not_found("Group not found"))?;
    let new_available = (group.last_article - group.last_scanned).max(0);

    Ok(Json(serde_json::json!({
        "group_id": group.id, "name": group.name,
        "last_scanned": group.last_scanned, "last_article": group.last_article,
        "new_available": new_available, "total_headers": total_headers,
        "unread_count": unread, "last_updated": group.last_updated,
    })))
}

/// POST /api/groups/{id}/subscribe
pub async fn h_group_subscribe(
    State(state): State<Arc<AppState>>,
    IdPath(id): IdPath<i64>,
) -> Result<Json<serde_json::Value>, ApiError> {
    state
        .queue_manager
        .with_db(|db| db.group_set_subscribed(id, true))
        .map_err(ApiError::from)?;
    Ok(Json(serde_json::json!({ "status": true })))
}

/// POST /api/groups/{id}/unsubscribe
pub async fn h_group_unsubscribe(
    State(state): State<Arc<AppState>>,
    IdPath(id): IdPath<i64>,
) -> Result<Json<serde_json::Value>, ApiError> {
    state
        .queue_manager
        .with_db(|db| db.group_set_subscribed(id, false))
        .map_err(ApiError::from)?;
    Ok(Json(serde_json::json!({ "status": true })))
}

/// GET /api/groups/{id}/headers
pub async fn h_header_list(
    State(state): State<Arc<AppState>>,
    IdPath(group_id): IdPath<i64>,
    Query(q): Query<HeaderListQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let limit = page_limit(q.limit, 50);
    let offset = q.offset.unwrap_or(0);

    let (headers, total) = db_blocking(&state, move |db| {
        let search = q.search.as_deref();
        Ok((
            db.header_list(group_id, search, limit, offset)?,
            db.header_count(group_id, search)?,
        ))
    })
    .await?;

    Ok(Json(serde_json::json!({
        "headers": headers, "total": total, "limit": limit, "offset": offset,
    })))
}

// ---------------------------------------------------------------------------
// Header fetch
// ---------------------------------------------------------------------------

/// Most articles one header fetch scans. A group that has grown by more
/// than this since the last fetch (or was never fetched) gets its newest
/// `HEADER_FETCH_MAX_ARTICLES` articles; the older gap is skipped, exactly
/// like the first fetch of a group.
pub const HEADER_FETCH_MAX_ARTICLES: u64 = 10_000;

/// Article numbers requested per XOVER command.
const HEADER_FETCH_BATCH: u64 = 2_000;

/// The inclusive article range one fetch should scan, or `None` when there
/// is nothing new.
///
/// `last_scanned` is the watermark measured on the server being fetched
/// from (`None` when the group was never scanned there). The start is
/// clamped up to the group's current first article, so a watermark pointing
/// at expired articles cannot pin the fetch below the server's range, and
/// the range is capped at `max_articles`, newest first.
pub fn plan_header_fetch(
    last_scanned: Option<u64>,
    first: u64,
    last: u64,
    max_articles: u64,
) -> Option<(u64, u64)> {
    if last == 0 || last < first || max_articles == 0 {
        return None;
    }
    let mut start = match last_scanned {
        Some(scanned) => scanned.saturating_add(1).max(first),
        None => first,
    };
    if start > last {
        return None;
    }
    start = start.max(last.saturating_sub(max_articles - 1));
    Some((start, last))
}

/// Groups with a header fetch in flight, keyed by queue manager (one per
/// app instance) and group id.
fn fetches_in_flight() -> &'static Mutex<HashSet<(usize, i64)>> {
    static IN_FLIGHT: OnceLock<Mutex<HashSet<(usize, i64)>>> = OnceLock::new();
    IN_FLIGHT.get_or_init(Default::default)
}

/// Marks a group's header fetch as running until dropped.
struct FetchInFlight {
    key: (usize, i64),
}

impl FetchInFlight {
    fn try_start(state: &AppState, group_id: i64) -> Option<Self> {
        let key = (Arc::as_ptr(&state.queue_manager) as usize, group_id);
        // Insert under the lock, but build the guard only on success: a
        // guard dropped while the lock is held would deadlock in `drop`.
        let inserted = fetches_in_flight().lock().insert(key);
        if inserted { Some(Self { key }) } else { None }
    }
}

impl Drop for FetchInFlight {
    fn drop(&mut self) {
        fetches_in_flight().lock().remove(&self.key);
    }
}

/// POST /api/groups/{id}/headers/fetch — Background XOVER fetch.
///
/// Returns 409 while a fetch of the same group is already running.
pub async fn h_header_fetch(
    State(state): State<Arc<AppState>>,
    IdPath(group_id): IdPath<i64>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let group = state
        .queue_manager
        .with_db(|db| db.group_get(group_id))
        .map_err(ApiError::from)?
        .ok_or_else(|| ApiError::not_found("Group not found"))?;
    let server = browse_server(&state)?;
    let scan_server = state
        .queue_manager
        .with_db(|db| db.group_scan_server(group_id))
        .map_err(ApiError::from)?;
    // Article numbers are per server: a watermark taken on another server
    // (or before the server was recorded) is not reused.
    let last_scanned = (scan_server.as_deref() == Some(server.id.as_str())
        && group.last_scanned > 0)
        .then_some(group.last_scanned as u64);

    let in_flight = FetchInFlight::try_start(&state, group_id).ok_or_else(|| {
        status_error(
            StatusCode::CONFLICT,
            "A header fetch for this group is already running",
        )
    })?;
    let max_headers = state.config.load().general.group_max_headers;
    let qm = state.queue_manager.clone();
    let group_name = group.name.clone();

    tokio::spawn(async move {
        let _in_flight = in_flight;
        let Some(_connection) = acquire_browse_connection(&server).await else {
            return;
        };
        run_header_fetch(&qm, &server, group_id, &group_name, last_scanned).await;
        match qm.with_db(|db| db.header_prune_group(group_id, max_headers)) {
            Ok(0) => {}
            Ok(pruned) => {
                tracing::info!(group = %group_name, pruned, "Pruned old headers");
            }
            Err(e) => {
                tracing::error!(error = %e, group = %group_name, "Failed to prune headers");
            }
        }
    });

    Ok(Json(serde_json::json!({
        "status": true,
        "message": format!("Header fetch started for '{}'", group.name),
    })))
}

async fn run_header_fetch(
    qm: &QueueManager,
    server: &ServerConfig,
    group_id: i64,
    group_name: &str,
    last_scanned: Option<u64>,
) {
    use nzb_web::nzb_core::nzb_nntp::connection::NntpConnection;

    let mut conn = NntpConnection::new("header-fetch".to_string());
    if let Err(e) = conn.connect(server).await {
        tracing::error!(error = %e, server = %server.name, "Header fetch connect failed");
        return;
    }

    let group_info = match conn.group(group_name).await {
        Ok(info) => info,
        Err(e) => {
            tracing::error!(error = %e, "GROUP command failed");
            let _ = conn.quit().await;
            return;
        }
    };

    let Some((start, end)) = plan_header_fetch(
        last_scanned,
        group_info.first,
        group_info.last,
        HEADER_FETCH_MAX_ARTICLES,
    ) else {
        tracing::info!(group = %group_name, "No new articles");
        let _ = conn.quit().await;
        return;
    };

    let mut batch_start = start;
    let mut total_stored = 0u64;
    while batch_start <= end {
        let batch_end = batch_start.saturating_add(HEADER_FETCH_BATCH - 1).min(end);
        let entries = match conn.xover(batch_start, batch_end).await {
            Ok(entries) => entries,
            Err(e) => {
                tracing::warn!(error = %e, "XOVER batch failed");
                break;
            }
        };
        let stored = qm.with_db(|db| {
            let stored = db.header_insert_batch(group_id, &entries)?;
            db.group_update_watermark(group_id, batch_end as i64, Some(&server.id))?;
            Ok::<_, nzb_web::nzb_core::NzbError>(stored)
        });
        match stored {
            Ok(count) => {
                total_stored += count;
                tracing::info!(
                    group = %group_name,
                    batch = %format!("{batch_start}-{batch_end}"),
                    stored = count,
                    "Header batch fetched"
                );
            }
            Err(e) => {
                tracing::error!(
                    error = %e,
                    group = %group_name,
                    batch = %format!("{batch_start}-{batch_end}"),
                    "Failed to persist fetched headers"
                );
                break;
            }
        }
        batch_start = batch_end + 1;
    }

    let _ = conn.quit().await;
    tracing::info!(group = %group_name, total = total_stored, "Header fetch complete");
}

/// DELETE /api/groups/{id}/headers — Delete a group's stored headers and
/// reset its scan watermark.
pub async fn h_header_clear(
    State(state): State<Arc<AppState>>,
    IdPath(group_id): IdPath<i64>,
) -> Result<Json<serde_json::Value>, ApiError> {
    // Not while a fetch would immediately re-populate the group.
    let _in_flight = FetchInFlight::try_start(&state, group_id).ok_or_else(|| {
        status_error(
            StatusCode::CONFLICT,
            "A header fetch for this group is running",
        )
    })?;
    let deleted = state
        .queue_manager
        .with_db(|db| db.header_clear_group(group_id))
        .map_err(ApiError::from)?;
    Ok(Json(
        serde_json::json!({ "status": true, "deleted": deleted }),
    ))
}

/// GET /api/groups/{id}/threads
pub async fn h_thread_list(
    State(state): State<Arc<AppState>>,
    IdPath(group_id): IdPath<i64>,
    Query(q): Query<HeaderListQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let limit = page_limit(q.limit, 50);
    let offset = q.offset.unwrap_or(0);

    let (threads, total) = db_blocking(&state, move |db| {
        db.header_list_threads(group_id, limit, offset)
    })
    .await?;

    Ok(Json(serde_json::json!({
        "threads": threads, "total": total, "limit": limit, "offset": offset,
    })))
}

/// GET /api/groups/{gid}/threads/{root_msg_id}
pub async fn h_thread_get(
    State(state): State<Arc<AppState>>,
    IdPath((group_id, root_msg_id)): IdPath<(i64, String)>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let root = root_msg_id.clone();
    let articles = db_blocking(&state, move |db| db.header_get_thread(group_id, &root)).await?;

    Ok(Json(serde_json::json!({
        "root_message_id": root_msg_id, "articles": articles,
    })))
}

/// POST /api/groups/{id}/headers/mark-read
pub async fn h_header_mark_read(
    State(state): State<Arc<AppState>>,
    IdPath(_group_id): IdPath<i64>,
    Json(input): Json<nzb_web::nzb_core::models::MarkReadInput>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let count = state
        .queue_manager
        .with_db(|db| mark_headers_read(&input.header_ids, |id| db.header_mark_read(id)))
        .map_err(ApiError::from)?;
    Ok(Json(serde_json::json!({ "marked": count })))
}

/// POST /api/groups/{id}/headers/mark-all-read
pub async fn h_header_mark_all_read(
    State(state): State<Arc<AppState>>,
    IdPath(group_id): IdPath<i64>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let count = db_blocking(&state, move |db| db.header_mark_all_read(group_id)).await?;
    Ok(Json(serde_json::json!({ "marked": count })))
}

/// GET /api/articles/{message_id} — Fetch from NNTP.
pub async fn h_article_get(
    State(state): State<Arc<AppState>>,
    Path(message_id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    use nzb_web::nzb_core::nzb_nntp::connection::NntpConnection;

    // Auto-mark as read
    state.queue_manager.with_db(|db| {
        if let Ok(Some(h)) = db.header_get_by_message_id(&message_id) {
            let _ = db.header_mark_read(h.id);
        }
    });

    let server = browse_server(&state)?;
    let _connection = acquire_browse_connection_bounded(&server).await?;

    let mut conn = NntpConnection::new("article-fetch".to_string());
    conn.connect(&server)
        .await
        .map_err(|e| ApiError::from(anyhow::anyhow!("Connect failed: {e}")))?;

    let response = conn.fetch_article(&message_id).await.map_err(|e| match e {
        nzb_web::nzb_core::nzb_nntp::error::NntpError::ArticleNotFound(_) => {
            ApiError::not_found("Article not found")
        }
        e => ApiError::from(anyhow::anyhow!("ARTICLE failed: {e}")),
    })?;
    let _ = conn.quit().await;

    let body = response
        .data
        .as_deref()
        .map(|b| String::from_utf8_lossy(b).into_owned());

    Ok(Json(serde_json::json!({
        "message_id": message_id, "code": response.code,
        "message": response.message, "body": body,
    })))
}

/// POST /api/groups/{id}/headers/download — Download selected as NZB.
pub async fn h_header_download(
    State(state): State<Arc<AppState>>,
    IdPath(group_id): IdPath<i64>,
    Json(input): Json<nzb_web::nzb_core::models::DownloadSelectedInput>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let group = state
        .queue_manager
        .with_db(|db| db.group_get(group_id))
        .map_err(ApiError::from)?
        .ok_or_else(|| ApiError::not_found("Group not found"))?;

    let name = input
        .name
        .unwrap_or_else(|| format!("Selected from {}", group.name));

    // Build NZB XML
    let mut nzb = String::from("<?xml version=\"1.0\" encoding=\"iso-8859-1\"?>\n");
    nzb.push_str("<!DOCTYPE nzb PUBLIC \"-//newzBin//DTD NZB 1.0//EN\" \"http://www.newzbin.com/DTD/nzb/nzb-1.0.dtd\">\n");
    nzb.push_str("<nzb xmlns=\"http://www.newzbin.com/DTD/2003/nzb\">\n");
    nzb.push_str(&format!(
        "  <head><meta type=\"name\">{name}</meta></head>\n"
    ));

    for msg_id in &input.message_ids {
        if let Ok(Some(h)) = state
            .queue_manager
            .with_db(|db| db.header_get_by_message_id(msg_id))
        {
            let esc = |s: &str| {
                s.replace('&', "&amp;")
                    .replace('<', "&lt;")
                    .replace('>', "&gt;")
                    .replace('"', "&quot;")
                    .replace('\'', "&apos;")
            };
            nzb.push_str(&format!(
                "  <file poster=\"{}\" date=\"0\" subject=\"{}\">\n    <groups><group>{}</group></groups>\n    <segments>\n      <segment bytes=\"{}\" number=\"1\">{}</segment>\n    </segments>\n  </file>\n",
                esc(&h.author), esc(&h.subject), group.name, h.bytes, h.message_id
            ));
        }
    }
    nzb.push_str("</nzb>\n");

    // Parse and queue
    let nzb_bytes = nzb.as_bytes();
    let mut job =
        nzb_web::nzb_core::nzb_parser::parse_nzb(&name, nzb_bytes).map_err(ApiError::from)?;

    if let Some(cat) = input.category {
        job.category = cat;
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

    let job_id = job.id.clone();
    tracing::info!(name = %job.name, id = %job.id, files = job.file_count, "Download from headers");

    qm.add_job(job, Some(nzb_bytes.to_vec()))
        .map_err(ApiError::from)?;

    Ok(Json(serde_json::json!({
        "status": true, "job_id": job_id,
        "message": format!("Added '{}' to queue", name),
    })))
}

#[cfg(test)]
mod tests {
    use super::{
        HEADER_FETCH_MAX_ARTICLES, ServerConfig, mark_headers_read, plan_header_fetch,
        select_browse_server,
    };

    #[test]
    fn plan_starts_after_the_watermark() {
        assert_eq!(
            plan_header_fetch(Some(100), 1, 150, 10_000),
            Some((101, 150))
        );
        assert_eq!(plan_header_fetch(Some(150), 1, 150, 10_000), None);
    }

    /// BUG-106: a watermark below the server's first article (expired range)
    /// is clamped up to the first article.
    #[test]
    fn plan_clamps_an_expired_watermark_to_the_first_article() {
        assert_eq!(
            plan_header_fetch(Some(100), 50_000, 50_100, 10_000),
            Some((50_000, 50_100))
        );
    }

    /// BUG-105: every fetch is capped, not only the first one.
    #[test]
    fn plan_caps_every_fetch_to_the_newest_articles() {
        assert_eq!(
            plan_header_fetch(None, 1, 1_000_000, HEADER_FETCH_MAX_ARTICLES),
            Some((1_000_000 - HEADER_FETCH_MAX_ARTICLES + 1, 1_000_000))
        );
        assert_eq!(
            plan_header_fetch(Some(10), 1, 1_000_000, 500),
            Some((999_501, 1_000_000))
        );
    }

    #[test]
    fn plan_handles_empty_groups() {
        assert_eq!(plan_header_fetch(None, 0, 0, 10), None);
        // RFC 3977 empty group: last < first.
        assert_eq!(plan_header_fetch(None, 51, 50, 10), None);
        assert_eq!(plan_header_fetch(Some(u64::MAX), 1, 50, 10), None);
    }

    /// BUG-107: browsing uses the highest-priority enabled server.
    #[test]
    fn browse_server_is_the_highest_priority_enabled_one() {
        let server = |id: &str, priority: u8, enabled: bool, connections: u16| {
            let mut config = ServerConfig::default();
            config.id = id.into();
            config.priority = priority;
            config.enabled = enabled;
            config.connections = connections;
            config
        };
        let servers = vec![
            server("disabled", 0, false, 8),
            server("no-connections", 0, true, 0),
            server("backup", 2, true, 8),
            server("primary", 1, true, 8),
            server("primary-tie", 1, true, 8),
        ];
        assert_eq!(select_browse_server(&servers).unwrap().id, "primary");
        assert!(select_browse_server(&servers[..2]).is_none());
    }

    #[test]
    fn mark_headers_read_counts_every_success() {
        let ids = [11, 22, 33];
        let mut seen = Vec::new();

        let marked = mark_headers_read(&ids, |id| {
            seen.push(id);
            Ok::<(), &'static str>(())
        })
        .expect("mark succeeds");

        assert_eq!(marked, 3);
        assert_eq!(seen, ids);
    }

    #[test]
    fn mark_headers_read_stops_on_first_error() {
        let ids = [11, 22, 33];
        let mut seen = Vec::new();

        let err = mark_headers_read(&ids, |id| {
            seen.push(id);
            if id == 22 {
                return Err("boom");
            }
            Ok(())
        })
        .expect_err("expected failure");

        assert_eq!(err, "boom");
        assert_eq!(seen, vec![11, 22]);
    }
}
