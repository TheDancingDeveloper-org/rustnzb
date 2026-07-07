//! HTTP handlers for newsgroup browsing.

use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, Query, State};
use serde::Deserialize;

use nzb_web::error::ApiError;
use nzb_web::state::AppState;

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

/// GET /api/groups
pub async fn h_group_list(
    State(state): State<Arc<AppState>>,
    Query(q): Query<GroupListQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let subscribed = q.subscribed.unwrap_or(false);
    let limit = q.limit.unwrap_or(100);
    let offset = q.offset.unwrap_or(0);
    let qm = &state.queue_manager;

    let groups = qm
        .with_db(|db| db.group_list(subscribed, q.search.as_deref(), limit, offset))
        .map_err(ApiError::from)?;
    let total = qm
        .with_db(|db| db.group_count(subscribed, q.search.as_deref()))
        .map_err(ApiError::from)?;

    Ok(Json(serde_json::json!({
        "groups": groups, "total": total, "limit": limit, "offset": offset,
    })))
}

/// POST /api/groups/refresh
pub async fn h_group_refresh(
    State(state): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, ApiError> {
    use nzb_web::nzb_core::nzb_nntp::connection::NntpConnection;

    let servers = state.queue_manager.get_servers();
    let server = servers
        .first()
        .ok_or_else(|| ApiError::bad_request("No servers configured"))?;

    let mut conn = NntpConnection::new("group-refresh".to_string());
    conn.connect(server)
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
    Path(id): Path<i64>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let group = state
        .queue_manager
        .with_db(|db| db.group_get(id))
        .map_err(ApiError::from)?
        .ok_or_else(|| ApiError::from(anyhow::anyhow!("Group not found")))?;
    Ok(Json(serde_json::to_value(group).map_err(|e| {
        ApiError::from(anyhow::anyhow!("Serialisation error: {e}"))
    })?))
}

/// GET /api/groups/{id}/status
pub async fn h_group_status(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let qm = &state.queue_manager;
    let group = qm
        .with_db(|db| db.group_get(id))
        .map_err(ApiError::from)?
        .ok_or_else(|| ApiError::from(anyhow::anyhow!("Group not found")))?;

    let total_headers = qm
        .with_db(|db| db.header_count(id, None))
        .map_err(ApiError::from)?;
    let unread = qm
        .with_db(|db| db.header_unread_count(id))
        .map_err(ApiError::from)?;
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
    Path(id): Path<i64>,
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
    Path(id): Path<i64>,
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
    Path(group_id): Path<i64>,
    Query(q): Query<HeaderListQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let limit = q.limit.unwrap_or(50);
    let offset = q.offset.unwrap_or(0);
    let qm = &state.queue_manager;

    let headers = qm
        .with_db(|db| db.header_list(group_id, q.search.as_deref(), limit, offset))
        .map_err(ApiError::from)?;
    let total = qm
        .with_db(|db| db.header_count(group_id, q.search.as_deref()))
        .map_err(ApiError::from)?;

    Ok(Json(serde_json::json!({
        "headers": headers, "total": total, "limit": limit, "offset": offset,
    })))
}

/// POST /api/groups/{id}/headers/fetch — Background XOVER fetch.
pub async fn h_header_fetch(
    State(state): State<Arc<AppState>>,
    Path(group_id): Path<i64>,
) -> Result<Json<serde_json::Value>, ApiError> {
    use nzb_web::nzb_core::nzb_nntp::connection::NntpConnection;

    let group = state
        .queue_manager
        .with_db(|db| db.group_get(group_id))
        .map_err(ApiError::from)?
        .ok_or_else(|| ApiError::from(anyhow::anyhow!("Group not found")))?;

    let servers = state.queue_manager.get_servers();
    let server = servers
        .first()
        .ok_or_else(|| ApiError::bad_request("No servers configured"))?
        .clone();
    let group_name = group.name.clone();
    let last_scanned = group.last_scanned;
    let qm = state.queue_manager.clone();

    tokio::spawn(async move {
        let mut conn = NntpConnection::new("header-fetch".to_string());
        if let Err(e) = conn.connect(&server).await {
            tracing::error!(error = %e, "Header fetch connect failed");
            return;
        }

        let group_info = match conn.group(&group_name).await {
            Ok(info) => info,
            Err(e) => {
                tracing::error!(error = %e, "GROUP command failed");
                return;
            }
        };

        let start = if last_scanned > 0 {
            (last_scanned as u64) + 1
        } else {
            group_info.last.saturating_sub(10000).max(group_info.first)
        };
        let end = group_info.last;

        if start > end {
            tracing::info!(group = %group_name, "No new articles");
            let _ = conn.quit().await;
            return;
        }

        let batch_size = 10000u64;
        let mut batch_start = start;
        let mut total_stored = 0u64;

        while batch_start <= end {
            let batch_end = (batch_start + batch_size - 1).min(end);
            match conn.xover(batch_start, batch_end).await {
                Ok(entries) => match qm.with_db(|db| db.header_insert_batch(group_id, &entries)) {
                    Ok(count) => {
                        total_stored += count;
                        if let Err(e) =
                            qm.with_db(|db| db.group_update_watermark(group_id, batch_end as i64))
                        {
                            tracing::error!(
                                error = %e,
                                group = %group_name,
                                batch = %format!("{batch_start}-{batch_end}"),
                                "Failed to update header fetch watermark"
                            );
                            break;
                        }
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
                },
                Err(e) => {
                    tracing::warn!(error = %e, "XOVER batch failed");
                    break;
                }
            }
            batch_start = batch_end + 1;
        }

        let _ = conn.quit().await;
        tracing::info!(group = %group_name, total = total_stored, "Header fetch complete");
    });

    Ok(Json(serde_json::json!({
        "status": true,
        "message": format!("Header fetch started for '{}'", group.name),
    })))
}

/// GET /api/groups/{id}/threads
pub async fn h_thread_list(
    State(state): State<Arc<AppState>>,
    Path(group_id): Path<i64>,
    Query(q): Query<HeaderListQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let limit = q.limit.unwrap_or(50);
    let offset = q.offset.unwrap_or(0);

    let (threads, total) = state
        .queue_manager
        .with_db(|db| db.header_list_threads(group_id, limit, offset))
        .map_err(ApiError::from)?;

    Ok(Json(serde_json::json!({
        "threads": threads, "total": total, "limit": limit, "offset": offset,
    })))
}

/// GET /api/groups/{gid}/threads/{root_msg_id}
pub async fn h_thread_get(
    State(state): State<Arc<AppState>>,
    Path((group_id, root_msg_id)): Path<(i64, String)>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let articles = state
        .queue_manager
        .with_db(|db| db.header_get_thread(group_id, &root_msg_id))
        .map_err(ApiError::from)?;

    Ok(Json(serde_json::json!({
        "root_message_id": root_msg_id, "articles": articles,
    })))
}

/// POST /api/groups/{id}/headers/mark-read
pub async fn h_header_mark_read(
    State(state): State<Arc<AppState>>,
    Path(_group_id): Path<i64>,
    Json(input): Json<nzb_web::nzb_core::models::MarkReadInput>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let mut count = 0u64;
    for id in &input.header_ids {
        if let Err(e) = state.queue_manager.with_db(|db| db.header_mark_read(*id)) {
            tracing::warn!(header_id = id, error = %e, "Failed to mark header as read");
        } else {
            count += 1;
        }
    }
    Ok(Json(serde_json::json!({ "marked": count })))
}

/// POST /api/groups/{id}/headers/mark-all-read
pub async fn h_header_mark_all_read(
    State(state): State<Arc<AppState>>,
    Path(group_id): Path<i64>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let count = state
        .queue_manager
        .with_db(|db| db.header_mark_all_read(group_id))
        .map_err(ApiError::from)?;
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

    let servers = state.queue_manager.get_servers();
    let server = servers
        .first()
        .ok_or_else(|| ApiError::bad_request("No servers configured"))?;

    let mut conn = NntpConnection::new("article-fetch".to_string());
    conn.connect(server)
        .await
        .map_err(|e| ApiError::from(anyhow::anyhow!("Connect failed: {e}")))?;

    let response = conn
        .fetch_article(&message_id)
        .await
        .map_err(|e| ApiError::from(anyhow::anyhow!("ARTICLE failed: {e}")))?;
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
    Path(group_id): Path<i64>,
    Json(input): Json<nzb_web::nzb_core::models::DownloadSelectedInput>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let group = state
        .queue_manager
        .with_db(|db| db.group_get(group_id))
        .map_err(ApiError::from)?
        .ok_or_else(|| ApiError::from(anyhow::anyhow!("Group not found")))?;

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
