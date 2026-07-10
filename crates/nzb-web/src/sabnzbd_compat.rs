//! *arr-compatible API layer for Sonarr/Radarr integration.
//!
//! Implements the download client protocol that Sonarr/Radarr use:
//! addfile, addurl, queue, history, config, fullstatus, version,
//! pause, resume, delete, retry.

use std::sync::Arc;

use axum::Json;
use axum::extract::{Multipart, Query, State};
use axum::response::IntoResponse;
use serde::{Deserialize, Serialize};

use crate::nzb_core::models::*;
use crate::nzb_core::nzb_parser;

use crate::error::ApiError;
use crate::state::AppState;

/// Arr-compatible API request -- all parameters come as query strings.
#[derive(Deserialize, Default)]
pub struct SabApiRequest {
    pub mode: Option<String>,
    pub name: Option<String>,
    pub value: Option<String>,
    pub value2: Option<String>,
    pub apikey: Option<String>,
    pub output: Option<String>,
    pub cat: Option<String>,
    pub priority: Option<String>,
    pub start: Option<usize>,
    pub limit: Option<usize>,
    pub password: Option<String>,
}

/// Validate API key. Returns Err with JSON response on failure.
fn validate_api_key(
    state: &AppState,
    provided: Option<&str>,
) -> Result<(), Json<serde_json::Value>> {
    let config = state.config();
    if let Some(ref configured_key) = config.general.api_key {
        let provided_key = provided.unwrap_or("");
        if !crate::auth::constant_time_eq(provided_key.as_bytes(), configured_key.as_bytes()) {
            return Err(Json(serde_json::json!({
                "status": false,
                "error": "API Key Incorrect"
            })));
        }
    }
    Ok(())
}

/// GET /sabnzbd/api -- Handle GET requests.
pub async fn h_sabnzbd_api_get(
    State(state): State<Arc<AppState>>,
    Query(req): Query<SabApiRequest>,
) -> Result<impl IntoResponse, ApiError> {
    if let Err(resp) = validate_api_key(&state, req.apikey.as_deref()) {
        return Ok(resp);
    }

    let mode = req.mode.as_deref().unwrap_or("");
    let result = dispatch_mode(&state, mode, &req);
    Ok(result)
}

/// POST /sabnzbd/api -- Handle POST requests (addfile multipart, or form-encoded).
pub async fn h_sabnzbd_api_post(
    State(state): State<Arc<AppState>>,
    Query(query_req): Query<SabApiRequest>,
    mut multipart: Multipart,
) -> Result<impl IntoResponse, ApiError> {
    // Extract fields from multipart form data
    let mut mode = query_req.mode.clone().unwrap_or_default();
    let mut apikey = query_req.apikey.clone();
    let mut cat = query_req.cat.clone();
    let mut priority = query_req.priority.clone();
    let mut name = query_req.name.clone();
    let mut nzb_data: Option<(String, Vec<u8>)> = None;
    let mut nzb_url: Option<String> = None;
    let mut password: Option<String> = query_req.password.clone();

    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| ApiError::from(anyhow::anyhow!("Multipart error: {e}")))?
    {
        let field_name = field.name().unwrap_or("").to_string();
        match field_name.as_str() {
            "mode" => {
                if let Ok(text) = field.text().await
                    && !text.is_empty()
                {
                    mode = text;
                }
            }
            "apikey" => {
                if let Ok(text) = field.text().await {
                    apikey = Some(text);
                }
            }
            "cat" => {
                if let Ok(text) = field.text().await {
                    cat = Some(text);
                }
            }
            "priority" => {
                if let Ok(text) = field.text().await {
                    priority = Some(text);
                }
            }
            "name" => {
                // Sonarr sends the NZB file upload with field name "name"
                // (via AddFormUpload("name", filename, nzbData)).
                // Distinguish file upload from plain text by checking file_name().
                if field.file_name().is_some() {
                    let file_name = field
                        .file_name()
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| "unknown.nzb".into());
                    let data = field
                        .bytes()
                        .await
                        .map_err(|e| ApiError::from(anyhow::anyhow!("Read error: {e}")))?;
                    nzb_data = Some((file_name, data.to_vec()));
                } else if let Ok(text) = field.text().await {
                    name = Some(text);
                }
            }
            "nzbfile" => {
                let file_name = field
                    .file_name()
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| "unknown.nzb".into());
                let data = field
                    .bytes()
                    .await
                    .map_err(|e| ApiError::from(anyhow::anyhow!("Read error: {e}")))?;
                nzb_data = Some((file_name, data.to_vec()));
            }
            "value" | "url" => {
                if let Ok(text) = field.text().await {
                    nzb_url = Some(text);
                }
            }
            "password" => {
                if let Ok(text) = field.text().await
                    && !text.is_empty()
                {
                    password = Some(text);
                }
            }
            _ => {
                let _ = field.bytes().await;
            }
        }
    }

    // Validate API key
    if let Err(resp) = validate_api_key(&state, apikey.as_deref()) {
        return Ok(resp);
    }

    match mode.as_str() {
        "addfile" => {
            let (file_name, data) = match nzb_data {
                Some(d) => d,
                None => {
                    return Ok(Json(serde_json::json!({
                        "status": false,
                        "error": "No NZB file provided"
                    })));
                }
            };

            let job_name = name.clone().unwrap_or_else(|| {
                file_name
                    .strip_suffix(".nzb")
                    .unwrap_or(&file_name)
                    .to_string()
            });

            match nzb_parser::parse_nzb(&job_name, &data) {
                Ok(mut job) => {
                    if let Some(ref c) = cat
                        && !c.is_empty()
                    {
                        job.category = c.clone();
                    }
                    if let Some(ref p) = priority {
                        job.priority = sab_priority_to_priority(p);
                    }

                    // API-provided password overrides NZB metadata password
                    if let Some(ref pw) = password {
                        job.password = Some(pw.clone());
                    }

                    let qm = &state.queue_manager;
                    job.work_dir = qm.incomplete_dir().join(&job.id);
                    job.output_dir = qm.complete_dir().join(&job.category).join(&job.name);

                    let nzo_id = format!("SABnzbd_nzo_{}", &job.id[..12.min(job.id.len())]);

                    tracing::info!(
                        name = %job.name,
                        id = %job.id,
                        files = job.file_count,
                        "NZB added to queue via arr API"
                    );

                    let nzb_bytes = data.clone();
                    qm.add_job(job, Some(nzb_bytes)).map_err(ApiError::from)?;

                    Ok(Json(serde_json::json!({
                        "status": true,
                        "nzo_ids": [nzo_id]
                    })))
                }
                Err(e) => Ok(Json(serde_json::json!({
                    "status": false,
                    "error": format!("Failed to parse NZB: {e}")
                }))),
            }
        }

        "addurl" => {
            let url = nzb_url.or(name.clone()).unwrap_or_default();

            if url.is_empty() {
                return Ok(Json(serde_json::json!({
                    "status": false,
                    "error": "No URL provided"
                })));
            }

            tracing::info!(url = %url, "Fetching NZB from URL via arr API");

            // Fetch the NZB from the URL
            let client = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .map_err(|e| ApiError::from(anyhow::anyhow!("HTTP client error: {e}")))?;

            let response = client
                .get(&url)
                .send()
                .await
                .map_err(|e| ApiError::from(anyhow::anyhow!("Failed to fetch URL: {e}")))?;

            if !response.status().is_success() {
                return Ok(Json(serde_json::json!({
                    "status": false,
                    "error": format!("URL returned HTTP {}", response.status())
                })));
            }

            let data = response
                .bytes()
                .await
                .map_err(|e| ApiError::from(anyhow::anyhow!("Failed to read response: {e}")))?;

            // Derive job name from URL filename if not provided
            let job_name = name.clone().unwrap_or_else(|| {
                url.rsplit('/')
                    .next()
                    .and_then(|s| s.split('?').next())
                    .unwrap_or("unknown")
                    .strip_suffix(".nzb")
                    .unwrap_or(
                        url.rsplit('/')
                            .next()
                            .and_then(|s| s.split('?').next())
                            .unwrap_or("unknown"),
                    )
                    .to_string()
            });

            match nzb_parser::parse_nzb(&job_name, &data) {
                Ok(mut job) => {
                    if let Some(ref c) = cat
                        && !c.is_empty()
                    {
                        job.category = c.clone();
                    }
                    if let Some(ref p) = priority {
                        job.priority = sab_priority_to_priority(p);
                    }

                    // API-provided password overrides NZB metadata password
                    if let Some(ref pw) = password {
                        job.password = Some(pw.clone());
                    }

                    let qm = &state.queue_manager;
                    job.work_dir = qm.incomplete_dir().join(&job.id);
                    job.output_dir = qm.complete_dir().join(&job.category).join(&job.name);

                    let nzo_id = format!("SABnzbd_nzo_{}", &job.id[..12.min(job.id.len())]);

                    tracing::info!(
                        name = %job.name,
                        id = %job.id,
                        files = job.file_count,
                        "NZB added to queue via URL (arr API)"
                    );

                    let nzb_bytes = data.to_vec();
                    qm.add_job(job, Some(nzb_bytes)).map_err(ApiError::from)?;

                    Ok(Json(serde_json::json!({
                        "status": true,
                        "nzo_ids": [nzo_id]
                    })))
                }
                Err(e) => Ok(Json(serde_json::json!({
                    "status": false,
                    "error": format!("Failed to parse NZB: {e}")
                }))),
            }
        }

        _ => {
            let req = SabApiRequest {
                mode: Some(mode),
                name,
                value: None,
                value2: None,
                apikey,
                output: None,
                cat,
                priority,
                start: query_req.start,
                limit: query_req.limit,
                password,
            };
            Ok(dispatch_mode(
                &state,
                req.mode.as_deref().unwrap_or(""),
                &req,
            ))
        }
    }
}

/// Dispatch an API mode to the appropriate handler.
fn dispatch_mode(state: &AppState, mode: &str, req: &SabApiRequest) -> Json<serde_json::Value> {
    match mode {
        "version" => Json(serde_json::json!({
            "version": "4.3.3"
        })),

        "queue" => handle_queue(state, req),

        "history" => handle_history(state, req),

        "get_config" | "config" => handle_get_config(state),

        "get_cats" => handle_get_cats(state),

        "change_cat" => handle_change_cat(state, req),

        "rename" => handle_rename(state, req),

        "change_complete_action" => {
            // No-op stub — Sonarr/Radarr may call this but we don't support custom actions
            Json(serde_json::json!({ "status": true }))
        }

        "switch" => {
            // TODO: implement queue reordering when priority queue is added
            Json(serde_json::json!({ "status": true }))
        }

        "priority" => {
            // TODO: implement priority changes for queued jobs
            Json(serde_json::json!({ "status": true }))
        }

        "fullstatus" | "server_stats" => {
            let qm = &state.queue_manager;
            Json(serde_json::json!({
                "status": {
                    "version": "4.3.3",
                    "paused": qm.is_paused(),
                    "speed": format!("{}", qm.get_speed()),
                }
            }))
        }

        "pause" => handle_pause(state, req),

        "resume" => handle_resume(state, req),

        "delete" => handle_delete(state, req),

        "retry" => handle_retry(state, req),

        _ => Json(serde_json::json!({
            "status": false,
            "error": format!("Unknown mode: {mode}")
        })),
    }
}

// ---------------------------------------------------------------------------
// Mode handlers
// ---------------------------------------------------------------------------

fn handle_queue(state: &AppState, req: &SabApiRequest) -> Json<serde_json::Value> {
    let qm = &state.queue_manager;

    // Sub-commands: mode=queue&name=delete|pause|resume&value=nzo_ID
    match req.name.as_deref() {
        Some("delete") => return handle_queue_delete(state, req),
        Some("pause") => return handle_queue_item_pause(state, req),
        Some("resume") => return handle_queue_item_resume(state, req),
        _ => {}
    }

    let jobs = qm.get_jobs();
    let paused = qm.is_paused();
    let speed_bps = qm.get_speed();

    let slots: Vec<SabQueueSlot> = jobs.iter().map(SabQueueSlot::from_job).collect();

    let total_mb: f64 = jobs.iter().map(|j| j.total_bytes as f64).sum::<f64>() / 1_048_576.0;
    let left_mb: f64 = jobs
        .iter()
        .map(|j| (j.total_bytes.saturating_sub(j.downloaded_bytes)) as f64)
        .sum::<f64>()
        / 1_048_576.0;

    Json(serde_json::json!({
        "queue": {
            "status": if paused { "Paused" } else { "Downloading" },
            "speedlimit": "",
            "speed": format_speed(speed_bps),
            "kbpersec": format!("{:.2}", speed_bps as f64 / 1024.0),
            "mbleft": format!("{left_mb:.2}"),
            "mb": format!("{total_mb:.2}"),
            "noofslots_total": jobs.len(),
            "noofslots": slots.len(),
            "paused": paused,
            "limit": req.limit.unwrap_or(0),
            "start": req.start.unwrap_or(0),
            "timeleft": "0:00:00",
            "eta": "unknown",
            "slots": slots
        }
    }))
}

/// Handle mode=queue&name=delete&value=nzo_ID (SABnzbd queue delete)
fn handle_queue_delete(state: &AppState, req: &SabApiRequest) -> Json<serde_json::Value> {
    let target = req.value.as_deref().unwrap_or("");
    if target.is_empty() {
        return Json(serde_json::json!({ "status": false, "error": "No job ID" }));
    }

    let qm = &state.queue_manager;

    // "all" removes everything from the queue
    if target == "all" {
        let jobs = qm.get_jobs();
        for job in &jobs {
            let _ = qm.remove_job(&job.id);
        }
        tracing::info!(
            count = jobs.len(),
            "All jobs removed from queue via arr API"
        );
        return Json(serde_json::json!({ "status": true }));
    }

    // Strip SABnzbd prefix and match by ID prefix
    let search_id = target.strip_prefix("SABnzbd_nzo_").unwrap_or(target);
    let jobs = qm.get_jobs();
    for job in &jobs {
        if job.id == search_id || job.id.starts_with(search_id) {
            let _ = qm.remove_job(&job.id);
            tracing::info!(id = %job.id, "Job removed from queue via arr API (mode=queue)");
            return Json(serde_json::json!({ "status": true }));
        }
    }

    tracing::warn!(search = %search_id, "Queue delete: job not found");
    Json(serde_json::json!({ "status": false }))
}

/// Handle mode=queue&name=pause&value=nzo_ID.
fn handle_queue_item_pause(state: &AppState, req: &SabApiRequest) -> Json<serde_json::Value> {
    let target = req.value.as_deref().unwrap_or("");
    let search_id = target.strip_prefix("SABnzbd_nzo_").unwrap_or(target);
    let Some(job) = state
        .queue_manager
        .get_jobs()
        .into_iter()
        .find(|job| job.id == search_id || job.id.starts_with(search_id))
    else {
        return Json(serde_json::json!({ "status": false, "error": "Job not found" }));
    };
    match state.queue_manager.pause_job(&job.id) {
        Ok(()) => Json(serde_json::json!({ "status": true })),
        Err(error) => Json(serde_json::json!({ "status": false, "error": error.to_string() })),
    }
}

/// Handle mode=queue&name=resume&value=nzo_ID.
fn handle_queue_item_resume(state: &AppState, req: &SabApiRequest) -> Json<serde_json::Value> {
    if state.queue_manager.is_paused() {
        return Json(serde_json::json!({
            "status": false,
            "error": "Cannot resume an individual job while downloads are globally paused"
        }));
    }
    let target = req.value.as_deref().unwrap_or("");
    let search_id = target.strip_prefix("SABnzbd_nzo_").unwrap_or(target);
    let Some(job) = state
        .queue_manager
        .get_jobs()
        .into_iter()
        .find(|job| job.id == search_id || job.id.starts_with(search_id))
    else {
        return Json(serde_json::json!({ "status": false, "error": "Job not found" }));
    };
    match state.queue_manager.resume_job(&job.id) {
        Ok(()) => Json(serde_json::json!({ "status": true })),
        Err(error) => Json(serde_json::json!({ "status": false, "error": error.to_string() })),
    }
}

fn handle_history(state: &AppState, req: &SabApiRequest) -> Json<serde_json::Value> {
    let qm = &state.queue_manager;

    // Sub-commands: mode=history&name=delete&value=nzo_ID
    if req.name.as_deref() == Some("delete") {
        return handle_history_delete(state, req);
    }

    let limit = req.limit.unwrap_or(50);
    let entries = qm.history_list(limit).unwrap_or_default();
    let slots: Vec<SabHistorySlot> = entries.iter().map(SabHistorySlot::from_entry).collect();

    Json(serde_json::json!({
        "history": {
            "noofslots": entries.len(),
            "last_history_update": chrono::Utc::now().timestamp(),
            "slots": slots
        }
    }))
}

/// Handle mode=history&name=delete&value=nzo_ID (SABnzbd history delete)
fn handle_history_delete(state: &AppState, req: &SabApiRequest) -> Json<serde_json::Value> {
    let target = req.value.as_deref().unwrap_or("");
    if target.is_empty() {
        return Json(serde_json::json!({ "status": false, "error": "No job ID" }));
    }

    let qm = &state.queue_manager;

    if target == "all" {
        // Note: SABnzbd supports "all" to clear history — we don't expose this
        // to avoid accidental data loss, but acknowledge the request.
        tracing::warn!("History delete-all requested via arr API (not implemented)");
        return Json(serde_json::json!({ "status": true }));
    }

    let search_id = target.strip_prefix("SABnzbd_nzo_").unwrap_or(target);
    let entries = qm.history_list(1000).unwrap_or_default();
    for entry in &entries {
        if entry.id == search_id || entry.id.starts_with(search_id) {
            let _ = qm.history_remove(&entry.id);
            tracing::info!(id = %entry.id, "Entry removed from history via arr API (mode=history)");
            return Json(serde_json::json!({ "status": true }));
        }
    }

    tracing::warn!(search = %search_id, "History delete: entry not found");
    Json(serde_json::json!({ "status": false }))
}

fn handle_get_config(state: &AppState) -> Json<serde_json::Value> {
    let config = state.config();
    let categories: Vec<serde_json::Value> = config
        .categories
        .iter()
        .map(|c| {
            serde_json::json!({
                "name": c.name,
                "dir": c.output_dir.as_deref().unwrap_or(std::path::Path::new("")).to_string_lossy(),
                "pp": c.post_processing.to_string(),
                "order": 0,
                "newzbin": "",
                "priority": 0,
            })
        })
        .collect();

    Json(serde_json::json!({
        "config": {
            "misc": {
                "complete_dir": config.general.complete_dir,
            },
            "categories": categories,
        }
    }))
}

fn handle_pause(state: &AppState, req: &SabApiRequest) -> Json<serde_json::Value> {
    let qm = &state.queue_manager;

    // If `name` or `value` contains a specific nzo_id, pause just that job
    let target_id = req.name.as_deref().or(req.value.as_deref());

    if let Some(nzo_id) = target_id
        && !nzo_id.is_empty()
    {
        let search_id = nzo_id.strip_prefix("SABnzbd_nzo_").unwrap_or(nzo_id);

        // Try to find and pause the job
        let jobs = qm.get_jobs();
        for job in &jobs {
            if job.id == search_id || job.id.starts_with(search_id) {
                let _ = qm.pause_job(&job.id);
                tracing::info!(id = %job.id, "Job paused via arr API");
                break;
            }
        }

        return Json(serde_json::json!({ "status": true }));
    }

    // No specific ID -- pause all
    qm.pause_all();
    tracing::info!("All jobs paused via arr API");

    Json(serde_json::json!({ "status": true }))
}

fn handle_resume(state: &AppState, req: &SabApiRequest) -> Json<serde_json::Value> {
    let qm = &state.queue_manager;

    let target_id = req.name.as_deref().or(req.value.as_deref());

    if let Some(nzo_id) = target_id
        && !nzo_id.is_empty()
    {
        if qm.is_paused() {
            return Json(serde_json::json!({
                "status": false,
                "error": "Cannot resume an individual job while downloads are globally paused"
            }));
        }
        let search_id = nzo_id.strip_prefix("SABnzbd_nzo_").unwrap_or(nzo_id);

        let jobs = qm.get_jobs();
        for job in &jobs {
            if job.id == search_id || job.id.starts_with(search_id) {
                let _ = qm.resume_job(&job.id);
                tracing::info!(id = %job.id, "Job resumed via arr API");
                break;
            }
        }

        return Json(serde_json::json!({ "status": true }));
    }

    // Resume all
    qm.resume_all();
    tracing::info!("All jobs resumed via arr API");

    Json(serde_json::json!({ "status": true }))
}

fn handle_delete(state: &AppState, req: &SabApiRequest) -> Json<serde_json::Value> {
    let qm = &state.queue_manager;

    let target_id = req.name.as_deref().or(req.value.as_deref()).unwrap_or("");

    if target_id.is_empty() {
        return Json(serde_json::json!({
            "status": false,
            "error": "No job ID provided"
        }));
    }

    let search_id = target_id.strip_prefix("SABnzbd_nzo_").unwrap_or(target_id);

    // Try to remove from queue
    let jobs = qm.get_jobs();
    let mut found = false;
    for job in &jobs {
        if job.id == search_id || job.id.starts_with(search_id) {
            let _ = qm.remove_job(&job.id);
            tracing::info!(id = %job.id, "Job removed from queue via arr API");
            found = true;
            break;
        }
    }

    // Also try history if not found in queue
    if !found {
        let entries = qm.history_list(1000).unwrap_or_default();
        for entry in &entries {
            if entry.id == search_id || entry.id.starts_with(search_id) {
                let _ = qm.history_remove(&entry.id);
                tracing::info!(id = %entry.id, "Entry removed from history via arr API");
                found = true;
                break;
            }
        }
    }

    Json(serde_json::json!({ "status": found }))
}

fn handle_retry(_state: &AppState, _req: &SabApiRequest) -> Json<serde_json::Value> {
    // Retry is complex — requires re-parsing the NZB which we don't store.
    // For now, return a stub.
    Json(serde_json::json!({
        "status": false,
        "error": "Retry not yet implemented"
    }))
}

fn handle_get_cats(state: &AppState) -> Json<serde_json::Value> {
    let config = state.config();
    let mut cats: Vec<String> = config.categories.iter().map(|c| c.name.clone()).collect();
    if !cats.iter().any(|c| c == "Default") {
        cats.insert(0, "Default".into());
    }
    Json(serde_json::json!({ "categories": cats }))
}

fn handle_change_cat(state: &AppState, req: &SabApiRequest) -> Json<serde_json::Value> {
    let job_id = req.value.as_deref().unwrap_or("");
    let new_cat = req.value2.as_deref().unwrap_or("");

    if job_id.is_empty() || new_cat.is_empty() {
        return Json(serde_json::json!({
            "status": false,
            "error": "Missing value (job id) or value2 (category)"
        }));
    }

    let search_id = job_id.strip_prefix("SABnzbd_nzo_").unwrap_or(job_id);

    let qm = &state.queue_manager;
    match qm.change_job_category(search_id, new_cat) {
        Ok(()) => Json(serde_json::json!({ "status": true })),
        Err(e) => Json(serde_json::json!({
            "status": false,
            "error": format!("{e}")
        })),
    }
}

fn handle_rename(state: &AppState, req: &SabApiRequest) -> Json<serde_json::Value> {
    let job_id = req.value.as_deref().unwrap_or("");
    let new_name = req.value2.as_deref().or(req.name.as_deref()).unwrap_or("");

    if job_id.is_empty() || new_name.is_empty() {
        return Json(serde_json::json!({
            "status": false,
            "error": "Missing value (job id) or value2/name (new name)"
        }));
    }

    let search_id = job_id.strip_prefix("SABnzbd_nzo_").unwrap_or(job_id);

    let qm = &state.queue_manager;
    match qm.rename_job(search_id, new_name) {
        Ok(()) => Json(serde_json::json!({ "status": true })),
        Err(e) => Json(serde_json::json!({
            "status": false,
            "error": format!("{e}")
        })),
    }
}

/// Convert arr-protocol priority string to our Priority enum.
fn sab_priority_to_priority(s: &str) -> Priority {
    match s.trim() {
        "-100" | "3" => Priority::Force,
        "2" => Priority::High,
        "1" => Priority::Normal,
        "0" => Priority::Low,
        _ => Priority::Normal,
    }
}

// ---------------------------------------------------------------------------
// Arr-compatible response types
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct SabQueueSlot {
    nzo_id: String,
    filename: String,
    cat: String,
    status: String,
    priority: String,
    mb: String,
    mbleft: String,
    percentage: String,
    timeleft: String,
    eta: String,
    avg_age: String,
    size: String,
    sizeleft: String,
}

impl SabQueueSlot {
    fn from_job(job: &NzbJob) -> Self {
        let mb = job.total_bytes as f64 / 1_048_576.0;
        let mbleft = (job.total_bytes.saturating_sub(job.downloaded_bytes)) as f64 / 1_048_576.0;
        let pct = if job.total_bytes > 0 {
            (job.downloaded_bytes as f64 / job.total_bytes as f64 * 100.0) as u32
        } else {
            0
        };

        Self {
            nzo_id: format!("SABnzbd_nzo_{}", &job.id[..12.min(job.id.len())]),
            filename: job.name.clone(),
            cat: job.category.clone(),
            status: sab_queue_status(job.status).into(),
            priority: match job.priority {
                Priority::Force => "Force".into(),
                Priority::High => "High".into(),
                Priority::Normal => "Normal".into(),
                Priority::Low => "Low".into(),
            },
            mb: format!("{mb:.2}"),
            mbleft: format!("{mbleft:.2}"),
            percentage: format!("{pct}"),
            timeleft: "0:00:00".into(),
            eta: "unknown".into(),
            avg_age: "0d".into(),
            size: format_size_human(job.total_bytes),
            sizeleft: format_size_human(job.total_bytes.saturating_sub(job.downloaded_bytes)),
        }
    }
}

/// Map internal lifecycle states to the status vocabulary accepted by the
/// SABnzbd clients in Sonarr and Radarr. In particular, `PostProcessing` is an
/// internal rustnzb state; SABnzbd reports custom post-processing as `Running`.
fn sab_queue_status(status: JobStatus) -> &'static str {
    match status {
        JobStatus::Queued => "Queued",
        JobStatus::Downloading => "Downloading",
        JobStatus::Paused => "Paused",
        JobStatus::Verifying => "Verifying",
        JobStatus::Repairing => "Repairing",
        JobStatus::Extracting => "Extracting",
        JobStatus::PostProcessing => "Running",
        JobStatus::Completed => "Completed",
        JobStatus::Failed => "Failed",
    }
}

#[derive(Serialize)]
struct SabHistorySlot {
    nzo_id: String,
    name: String,
    category: String,
    status: String,
    bytes: u64,
    storage: String,
    completed: i64,
    fail_message: String,
    download_time: u64,
    pp: String,
    nzb_name: String,
    stage_log: Vec<SabStageLog>,
}

#[derive(Serialize)]
struct SabStageLog {
    name: String,
    actions: Vec<String>,
}

impl SabHistorySlot {
    fn from_entry(entry: &HistoryEntry) -> Self {
        let stage_log: Vec<SabStageLog> = entry
            .stages
            .iter()
            .map(|s| SabStageLog {
                name: s.name.clone(),
                actions: vec![s.message.clone().unwrap_or_default()],
            })
            .collect();

        Self {
            nzo_id: format!("SABnzbd_nzo_{}", &entry.id[..12.min(entry.id.len())]),
            name: entry.name.clone(),
            category: entry.category.clone(),
            status: match entry.status {
                JobStatus::Completed => "Completed".into(),
                JobStatus::Failed => "Failed".into(),
                _ => entry.status.to_string(),
            },
            bytes: entry.downloaded_bytes,
            storage: entry.output_dir.to_string_lossy().to_string(),
            completed: entry.completed_at.timestamp(),
            fail_message: entry.error_message.clone().unwrap_or_default(),
            download_time: (entry.completed_at - entry.added_at).num_seconds().max(0) as u64,
            pp: "D".into(),
            nzb_name: format!("{}.nzb", entry.name),
            stage_log,
        }
    }
}

/// Format bytes to human-readable size string.
fn format_size_human(bytes: u64) -> String {
    if bytes == 0 {
        return "0 B".into();
    }
    let units = ["B", "KB", "MB", "GB", "TB"];
    let mut val = bytes as f64;
    let mut i = 0;
    while val >= 1024.0 && i < units.len() - 1 {
        val /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{val:.0} {}", units[i])
    } else {
        format!("{val:.1} {}", units[i])
    }
}

/// Format speed as a human-readable string.
fn format_speed(bps: u64) -> String {
    if bps >= 1_073_741_824 {
        format!("{:.1} GB/s", bps as f64 / 1_073_741_824.0)
    } else if bps >= 1_048_576 {
        format!("{:.1} MB/s", bps as f64 / 1_048_576.0)
    } else if bps >= 1024 {
        format!("{:.1} KB/s", bps as f64 / 1024.0)
    } else {
        format!("{bps} B/s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queue_statuses_use_sabnzbd_vocabulary() {
        let cases = [
            (JobStatus::Queued, "Queued"),
            (JobStatus::Downloading, "Downloading"),
            (JobStatus::Paused, "Paused"),
            (JobStatus::Verifying, "Verifying"),
            (JobStatus::Repairing, "Repairing"),
            (JobStatus::Extracting, "Extracting"),
            (JobStatus::PostProcessing, "Running"),
            (JobStatus::Completed, "Completed"),
            (JobStatus::Failed, "Failed"),
        ];

        for (status, expected) in cases {
            assert_eq!(sab_queue_status(status), expected);
        }
    }
}
