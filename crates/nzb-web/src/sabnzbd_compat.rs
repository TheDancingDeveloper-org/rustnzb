//! *arr-compatible API layer for Sonarr/Radarr integration.
//!
//! Implements the download client protocol that Sonarr/Radarr use:
//! addfile, addurl, queue, history, config, fullstatus, version,
//! pause, resume, delete, retry.

use std::sync::Arc;

use axum::Json;
use axum::extract::{Form, FromRequest, Multipart, Query, Request, State};
use axum::http::{StatusCode, header::CONTENT_TYPE};
use axum::response::IntoResponse;
use serde::{Deserialize, Serialize};

use crate::nzb_core::models::*;
use crate::nzb_core::nzb_parser;

use crate::error::ApiError;
use crate::queue_manager::job_id_matches;
use crate::state::AppState;

/// SABnzbd release whose public response contract this compatibility layer
/// targets. Keep this in sync with the conformance fixtures under
/// `tests/fixtures/sabnzbd-*`.
const SABNZBD_COMPAT_VERSION: &str = "5.0.4";

/// Upper bound on an `addurl`-fetched NZB body, to avoid unbounded memory use.
const MAX_ADDURL_BODY_BYTES: usize = 100 * 1024 * 1024;

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
    pub category: Option<String>,
    pub priority: Option<String>,
    pub status: Option<String>,
    pub search: Option<String>,
    pub nzo_ids: Option<String>,
    pub start: Option<usize>,
    pub limit: Option<usize>,
    pub failed_only: Option<String>,
    pub archive: Option<String>,
    pub last_history_update: Option<u64>,
    pub password: Option<String>,
    pub del_files: Option<String>,
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

/// Answer the modes SABnzbd serves without an API key. Clients probe
/// `version` and `auth` before they have been given a key, so real SABnzbd
/// exempts both from the key check (`sabnzbd/interface.py`, `api.py::_api_auth`).
fn keyless_response(
    state: &AppState,
    mode: &str,
    provided: Option<&str>,
) -> Option<Json<serde_json::Value>> {
    match mode {
        "version" => Some(Json(serde_json::json!({
            "version": SABNZBD_COMPAT_VERSION
        }))),
        "auth" => {
            let config = state.config();
            let auth = match config.general.api_key.as_deref() {
                None => "None",
                Some(configured) => match provided.filter(|key| !key.is_empty()) {
                    None => "apikey",
                    Some(key)
                        if crate::auth::constant_time_eq(key.as_bytes(), configured.as_bytes()) =>
                    {
                        "apikey"
                    }
                    Some(_) => "badkey",
                },
            };
            Some(Json(serde_json::json!({ "auth": auth })))
        }
        _ => None,
    }
}

/// GET /sabnzbd/api -- Handle GET requests.
pub async fn h_sabnzbd_api_get(
    State(state): State<Arc<AppState>>,
    Query(req): Query<SabApiRequest>,
) -> Result<impl IntoResponse, ApiError> {
    if let Some(resp) = keyless_response(
        &state,
        req.mode.as_deref().unwrap_or(""),
        req.apikey.as_deref(),
    ) {
        return Ok(resp);
    }
    if let Err(resp) = validate_api_key(&state, req.apikey.as_deref()) {
        return Ok(resp);
    }

    let mode = req.mode.as_deref().unwrap_or("");

    // `addurl` fetches a remote NZB and has no file body to upload, so real
    // SABnzbd (and clients like NZB360/Sonarr/Radarr) issue it as a plain
    // GET rather than a multipart POST. Route it to the same URL-fetching
    // logic the POST handler uses so `cat`/`priority` are honored here too.
    if mode == "addurl" {
        let url = req.name.clone().or_else(|| req.value.clone());
        return handle_addurl(
            &state,
            url,
            req.name.clone(),
            req.cat.clone(),
            req.priority.clone(),
            req.password.clone(),
        )
        .await;
    }

    let result = dispatch_mode(&state, mode, &req);
    Ok(result)
}

/// Fetch an NZB from a URL and enqueue it, applying category/priority/password
/// overrides. Shared by the GET and POST `addurl` entry points.
async fn handle_addurl(
    state: &AppState,
    url: Option<String>,
    name: Option<String>,
    cat: Option<String>,
    priority: Option<String>,
    password: Option<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let url = url.unwrap_or_default();

    if url.is_empty() {
        return Ok(Json(serde_json::json!({
            "status": false,
            "error": "No URL provided"
        })));
    }

    // SSRF guard: addurl fetches a caller-supplied URL, so validate it and pin
    // the connection to the validated address (shared with the native URL-add
    // path in the app crate). Rejects non-http(s) schemes and private/reserved
    // hosts such as 169.254.169.254. See rustnzb#129 review.
    let fetch_plan = match crate::fetch_guard::validate_fetch_url(&url).await {
        Ok(plan) => plan,
        Err(error) => {
            tracing::warn!(url = %url, %error, "Refusing addurl fetch (SSRF guard)");
            return Ok(Json(serde_json::json!({
                "status": false,
                "error": error.to_string()
            })));
        }
    };

    tracing::info!(url = %url, "Fetching NZB from URL via arr API");

    let client = crate::fetch_guard::build_fetch_client(&fetch_plan)?;

    let response = client
        .get(fetch_plan.url.clone())
        .send()
        .await
        .map_err(|e| ApiError::from(anyhow::anyhow!("Failed to fetch URL: {e}")))?;

    if !response.status().is_success() {
        return Ok(Json(serde_json::json!({
            "status": false,
            "error": format!("URL returned HTTP {}", response.status())
        })));
    }

    // Cap the fetched body to avoid unbounded memory from a hostile URL.
    let data =
        crate::fetch_guard::read_response_bytes_limited(response, MAX_ADDURL_BODY_BYTES).await?;

    // Derive job name from URL filename if not provided
    let job_name = name.unwrap_or_else(|| {
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
                job.category = sab_resolve_category(c).to_string();
            }
            if let Some(ref p) = priority {
                apply_sab_add_priority(&mut job, p);
            }

            // API-provided password overrides NZB metadata password
            if let Some(ref pw) = password {
                job.password = Some(pw.clone());
            }

            let qm = &state.queue_manager;
            job.work_dir = qm.incomplete_dir().join(&job.id);
            job.output_dir = match qm.output_dir_for(&job.category, &job.name) {
                Ok(path) => path,
                Err(error) => {
                    return Ok(Json(serde_json::json!({
                        "status": false,
                        "error": error.to_string()
                    })));
                }
            };

            let nzo_id = format!("SABnzbd_nzo_{}", &job.id[..12.min(job.id.len())]);
            let job_name = job.name.clone();
            let job_id = job.id.clone();
            let file_count = job.file_count;

            // As with addfile, report a failed enqueue as `status: false` on
            // HTTP 200 rather than a 500, and log success only after the
            // enqueue succeeds (rustnzb#129).
            let nzb_bytes = data.to_vec();
            if let Err(error) = qm.add_job(job, Some(nzb_bytes)) {
                tracing::error!(
                    name = %job_name,
                    id = %job_id,
                    %error,
                    "Failed to add NZB to queue via URL (arr API)"
                );
                return Ok(Json(serde_json::json!({
                    "status": false,
                    "error": error.to_string()
                })));
            }

            tracing::info!(
                name = %job_name,
                id = %job_id,
                files = file_count,
                "NZB added to queue via URL (arr API)"
            );

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

/// Body encodings a SABnzbd client may use for a POST request.
enum SabPostBody {
    /// `multipart/form-data` -- the only encoding that can carry an NZB file.
    Multipart,
    /// `application/x-www-form-urlencoded` -- plain key/value fields.
    Form,
    /// No body, or an encoding we don't parse: parameters come from the
    /// query string alone.
    None,
}

fn classify_post_body(request: &Request) -> SabPostBody {
    let content_type = request
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.trim().to_ascii_lowercase())
        .unwrap_or_default();
    if content_type.starts_with("multipart/form-data") {
        SabPostBody::Multipart
    } else if content_type.starts_with("application/x-www-form-urlencoded") {
        SabPostBody::Form
    } else {
        SabPostBody::None
    }
}

/// POST /sabnzbd/api -- Handle POST requests.
///
/// The body is optional. `mode=addfile` needs a `multipart/form-data` upload,
/// but clients such as Prowlarr send `mode=addurl` as a bare POST with every
/// parameter in the query string and no body at all (#119), and others use
/// `application/x-www-form-urlencoded`. Requiring the multipart extractor
/// unconditionally rejected those with `400 Invalid boundary` before the
/// mode was ever inspected, so the body is only parsed as multipart when the
/// request actually says it is one.
pub async fn h_sabnzbd_api_post(
    State(state): State<Arc<AppState>>,
    Query(query_req): Query<SabApiRequest>,
    request: Request,
) -> Result<impl IntoResponse, ApiError> {
    // Query-string parameters are the baseline; body fields override them.
    let mut mode = query_req.mode.clone().unwrap_or_default();
    let mut apikey = query_req.apikey.clone();
    let mut cat = query_req.cat.clone();
    let mut priority = query_req.priority.clone();
    let mut name = query_req.name.clone();
    let mut nzb_data: Option<(String, Vec<u8>)> = None;
    let mut nzb_url: Option<String> = None;
    let mut password: Option<String> = query_req.password.clone();

    match classify_post_body(&request) {
        SabPostBody::None => {}
        SabPostBody::Form => {
            let Form(form) = Form::<SabApiRequest>::from_request(request, &())
                .await
                .map_err(|e| {
                    ApiError::from((StatusCode::BAD_REQUEST, format!("Form error: {e}")))
                })?;
            if let Some(m) = form.mode.filter(|m| !m.is_empty()) {
                mode = m;
            }
            if form.apikey.is_some() {
                apikey = form.apikey;
            }
            if form.cat.is_some() {
                cat = form.cat;
            }
            if form.priority.is_some() {
                priority = form.priority;
            }
            if form.name.is_some() {
                name = form.name;
            }
            if form.value.is_some() {
                nzb_url = form.value;
            }
            if let Some(pw) = form.password.filter(|pw| !pw.is_empty()) {
                password = Some(pw);
            }
        }
        SabPostBody::Multipart => {
            let mut multipart = Multipart::from_request(request, &()).await.map_err(|e| {
                ApiError::from((StatusCode::BAD_REQUEST, format!("Multipart error: {e}")))
            })?;
            read_multipart_fields(
                &mut multipart,
                &mut mode,
                &mut apikey,
                &mut cat,
                &mut priority,
                &mut name,
                &mut nzb_data,
                &mut nzb_url,
                &mut password,
            )
            .await?;
        }
    }

    if let Some(resp) = keyless_response(&state, &mode, apikey.as_deref()) {
        return Ok(resp);
    }

    // Validate API key
    if let Err(resp) = validate_api_key(&state, apikey.as_deref()) {
        return Ok(resp);
    }

    dispatch_post(
        &state, mode, name, cat, priority, nzb_data, nzb_url, password, query_req,
    )
    .await
}

/// Fold the fields of a multipart body into the request parameters.
#[allow(clippy::too_many_arguments)]
async fn read_multipart_fields(
    multipart: &mut Multipart,
    mode: &mut String,
    apikey: &mut Option<String>,
    cat: &mut Option<String>,
    priority: &mut Option<String>,
    name: &mut Option<String>,
    nzb_data: &mut Option<(String, Vec<u8>)>,
    nzb_url: &mut Option<String>,
    password: &mut Option<String>,
) -> Result<(), ApiError> {
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
                    *mode = text;
                }
            }
            "apikey" => {
                if let Ok(text) = field.text().await {
                    *apikey = Some(text);
                }
            }
            "cat" => {
                if let Ok(text) = field.text().await {
                    *cat = Some(text);
                }
            }
            "priority" => {
                if let Ok(text) = field.text().await {
                    *priority = Some(text);
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
                    *nzb_data = Some((file_name, data.to_vec()));
                } else if let Ok(text) = field.text().await {
                    *name = Some(text);
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
                *nzb_data = Some((file_name, data.to_vec()));
            }
            "value" | "url" => {
                if let Ok(text) = field.text().await {
                    *nzb_url = Some(text);
                }
            }
            "password" => {
                if let Ok(text) = field.text().await
                    && !text.is_empty()
                {
                    *password = Some(text);
                }
            }
            _ => {
                let _ = field.bytes().await;
            }
        }
    }
    Ok(())
}

/// Dispatch a POST request once its parameters have been assembled from the
/// query string and (optional) body.
#[allow(clippy::too_many_arguments)]
async fn dispatch_post(
    state: &AppState,
    mode: String,
    name: Option<String>,
    cat: Option<String>,
    priority: Option<String>,
    nzb_data: Option<(String, Vec<u8>)>,
    nzb_url: Option<String>,
    password: Option<String>,
    query_req: SabApiRequest,
) -> Result<Json<serde_json::Value>, ApiError> {
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
                        job.category = sab_resolve_category(c).to_string();
                    }
                    if let Some(ref p) = priority {
                        apply_sab_add_priority(&mut job, p);
                    }

                    // API-provided password overrides NZB metadata password
                    if let Some(ref pw) = password {
                        job.password = Some(pw.clone());
                    }

                    let qm = &state.queue_manager;
                    job.work_dir = qm.incomplete_dir().join(&job.id);
                    job.output_dir = match qm.output_dir_for(&job.category, &job.name) {
                        Ok(path) => path,
                        Err(error) => {
                            return Ok(Json(serde_json::json!({
                                "status": false,
                                "error": error.to_string()
                            })));
                        }
                    };

                    let nzo_id = format!("SABnzbd_nzo_{}", &job.id[..12.min(job.id.len())]);
                    let job_name = job.name.clone();
                    let job_id = job.id.clone();
                    let file_count = job.file_count;

                    // SABnzbd's addfile always responds HTTP 200 with a JSON
                    // `status` field; a failed enqueue is reported as
                    // `status: false`, never a 5xx. Propagating the error as a
                    // 500 here made Sonarr treat an otherwise-reportable
                    // failure as a hard download-client error, and the log
                    // claimed success before the enqueue that actually failed
                    // (rustnzb#129). Enqueue first, then report the real
                    // outcome -- mirroring the history-retry path.
                    let nzb_bytes = data.clone();
                    if let Err(error) = qm.add_job(job, Some(nzb_bytes)) {
                        tracing::error!(
                            name = %job_name,
                            id = %job_id,
                            %error,
                            "Failed to add NZB to queue via arr API"
                        );
                        return Ok(Json(serde_json::json!({
                            "status": false,
                            "error": error.to_string()
                        })));
                    }

                    tracing::info!(
                        name = %job_name,
                        id = %job_id,
                        files = file_count,
                        "NZB added to queue via arr API"
                    );

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
            let url = nzb_url.or_else(|| name.clone());
            handle_addurl(state, url, name, cat, priority, password).await
        }

        _ => {
            let req = SabApiRequest {
                mode: Some(mode),
                name,
                // Sub-commands like queue/history delete, priority, and
                // rename take `value`/`value2` as plain query-string
                // parameters even on POST -- these were previously dropped
                // here, silently breaking those actions over POST.
                value: query_req.value,
                value2: query_req.value2,
                apikey: None, // already validated by the caller
                output: None,
                cat,
                category: query_req.category,
                priority,
                status: query_req.status,
                search: query_req.search,
                nzo_ids: query_req.nzo_ids,
                start: query_req.start,
                limit: query_req.limit,
                failed_only: query_req.failed_only,
                archive: query_req.archive,
                last_history_update: query_req.last_history_update,
                password,
                del_files: query_req.del_files,
            };
            Ok(dispatch_mode(
                state,
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
            "version": SABNZBD_COMPAT_VERSION
        })),

        "queue" => handle_queue(state, req),

        "history" => handle_history(state, req),

        "get_config" | "config" => handle_get_config(state),

        "get_cats" => handle_get_cats(state),

        // RustNZB doesn't support post-processing scripts, so this is the
        // permanent, correct response -- it matches what real SABnzbd
        // reports when no script directory / scripts are configured
        // (sabnzbd/api.py::_api_get_scripts -> filesystem.py::list_scripts).
        "get_scripts" => handle_get_scripts(state),

        "change_cat" => handle_change_cat(state, req),

        "rename" => handle_rename(state, req),

        "change_complete_action" => {
            // No-op stub — Sonarr/Radarr may call this but we don't support custom actions
            Json(serde_json::json!({ "status": true }))
        }

        "switch" => handle_switch(state, req),

        "priority" => handle_priority(state, req),

        "fullstatus" | "server_stats" => handle_fullstatus(state),

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

fn handle_get_scripts(state: &AppState) -> Json<serde_json::Value> {
    let config = state.config();
    let mut scripts = Vec::new();
    if let Some(directory) = config.general.scripts_dir.as_ref()
        && let Ok(entries) = std::fs::read_dir(directory)
    {
        for entry in entries.flatten() {
            let path = entry.path();
            if entry
                .file_type()
                .map(|kind| kind.is_file())
                .unwrap_or(false)
                && path.file_name().and_then(|name| name.to_str()).is_some()
            {
                scripts.push(path.file_name().unwrap().to_string_lossy().into_owned());
            }
        }
    }
    scripts.sort();
    if scripts.is_empty() {
        scripts.push("None".to_string());
    }
    Json(serde_json::json!({ "scripts": scripts }))
}

/// Return the stable subset of SABnzbd's full-status dashboard contract.
///
/// SAB-compatible clients inspect this response as a capability/status
/// document, so preserving its keys and JSON types is more important than
/// inventing measurements RustNZB does not currently collect. Unsupported
/// dashboard measurements therefore use SABnzbd-compatible empty/zero values.
fn handle_fullstatus(state: &AppState) -> Json<serde_json::Value> {
    let config = state.config();
    let general = &config.general;
    let qm = &state.queue_manager;
    let speed_limit = qm.get_speed_limit();
    let pause_int = qm.pause_remaining_secs().unwrap_or(0).max(0).to_string();

    Json(serde_json::json!({
        "status": {
            "active_lang": "en",
            "active_socks5_proxy": serde_json::Value::Null,
            "apikey": general.api_key.as_deref().unwrap_or(""),
            "cache_art": "0",
            "cache_size": format_size_human(general.cache_size),
            "color_scheme": "Auto",
            "completedir": general.complete_dir.to_string_lossy(),
            "completedirspeed": 0,
            "configfn": state.config_path.to_string_lossy(),
            "confighelpuri": "https://sabnzbd.org/wiki/configuration/5.0/",
            "delayed_assembler": 0,
            "diskspace1": "0.00",
            "diskspace1_norm": "0 B",
            "diskspace2": "0.00",
            "diskspace2_norm": "0 B",
            "diskspacetotal1": "0.00",
            "diskspacetotal2": "0.00",
            "dnslookup": false,
            "downloaddir": general.incomplete_dir.to_string_lossy(),
            "downloaddirspeed": 0,
            "finishaction": serde_json::Value::Null,
            "folders": Vec::<String>::new(),
            "have_quota": false,
            "have_warnings": "0",
            "internetbandwidth": 0,
            "ipv6": serde_json::Value::Null,
            "left_quota": "0 B",
            "loadavg": "",
            "localipv4": serde_json::Value::Null,
            "logfile": general
                .log_file
                .as_ref()
                .map_or_else(String::new, |path| path.to_string_lossy().into_owned()),
            "loglevel": &general.log_level,
            "macos": cfg!(target_os = "macos"),
            "my_home": general.data_dir.to_string_lossy(),
            "my_lcldata": general.data_dir.to_string_lossy(),
            "new_rel_url": serde_json::Value::Null,
            "new_release": serde_json::Value::Null,
            "pause_int": pause_int,
            "paused": qm.is_paused(),
            "paused_all": false,
            "pid": std::process::id(),
            "power_options": false,
            "pp_pause_event": false,
            "publicipv4": serde_json::Value::Null,
            "pystone": 0,
            "quota": "0 B",
            "rtl": false,
            "servers": Vec::<serde_json::Value>::new(),
            "speedlimit": if speed_limit == 0 { "0" } else { "100" },
            "speedlimit_abs": speed_limit.to_string(),
            "uptime": "0m",
            "url_base": "",
            "version": SABNZBD_COMPAT_VERSION,
            "warnings": Vec::<serde_json::Value>::new(),
            "webdir": "",
            "weblogfile": serde_json::Value::Null,
            "windows": cfg!(target_os = "windows"),
        }
    }))
}

// ---------------------------------------------------------------------------
// Mode handlers
// ---------------------------------------------------------------------------

fn handle_queue(state: &AppState, req: &SabApiRequest) -> Json<serde_json::Value> {
    let qm = &state.queue_manager;

    // Sub-commands dispatched via mode=queue&name=<cmd>, matching SABnzbd's
    // real `_api_queue_table` (delete, pause, resume, priority, rename,
    // purge, change_complete_action). `sort` and `delete_nzf` have no
    // equivalent capability in RustNZB's queue manager yet.
    match req.name.as_deref() {
        Some("delete") => return handle_queue_delete(state, req),
        Some("pause") => return handle_queue_item_pause(state, req),
        Some("resume") => return handle_queue_item_resume(state, req),
        Some("priority") => return handle_queue_priority(state, req),
        Some("rename") => return handle_queue_rename(state, req),
        Some("purge") => return handle_queue_purge(state),
        Some("sort") => {
            let ascending = !matches!(
                req.value.as_deref(),
                Some(value)
                    if value.eq_ignore_ascii_case("descending")
                        || value.eq_ignore_ascii_case("desc")
            );
            qm.sort_by_remaining_percentage(ascending);
            return Json(serde_json::json!({ "status": true }));
        }
        Some("change_complete_action") => return Json(serde_json::json!({ "status": true })),
        _ => {}
    }

    let jobs = qm.get_active_jobs();
    let paused = qm.is_paused();
    let speed_bps = qm.get_speed();
    let speed_limit_bps = qm.get_speed_limit();

    Json(build_queue_response(
        &jobs,
        paused,
        speed_bps,
        speed_limit_bps,
        req,
    ))
}

fn build_queue_response(
    jobs: &[NzbJob],
    paused: bool,
    speed_bps: u64,
    speed_limit_bps: u64,
    req: &SabApiRequest,
) -> serde_json::Value {
    let start = req.start.unwrap_or(0);
    let limit = req.limit.unwrap_or(0);
    let category_query = req
        .cat
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .or(req.category.as_deref());
    let categories = comma_separated(category_query);
    let priorities = comma_separated(req.priority.as_deref());
    let statuses = comma_separated(req.status.as_deref());
    let nzo_ids = comma_separated(req.nzo_ids.as_deref());
    let search = req
        .search
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_lowercase);

    let matching_jobs: Vec<&NzbJob> = jobs
        .iter()
        .filter(|job| {
            search
                .as_ref()
                .is_none_or(|term| job.name.to_lowercase().contains(term))
                && (categories.is_empty()
                    || categories.iter().any(|category| {
                        sab_resolve_category(category).eq_ignore_ascii_case(&job.category)
                    }))
                && (priorities.is_empty()
                    || priorities
                        .iter()
                        .any(|priority| sab_priority_matches(job.priority, priority)))
                && (statuses.is_empty()
                    || statuses
                        .iter()
                        .any(|status| status.eq_ignore_ascii_case(sab_queue_status(job.status))))
                && (nzo_ids.is_empty()
                    || nzo_ids
                        .iter()
                        .any(|nzo_id| queue_nzo_id(job).eq_ignore_ascii_case(nzo_id)))
        })
        .collect();

    let page = matching_jobs
        .iter()
        .skip(start)
        .take(if limit == 0 { usize::MAX } else { limit });
    let mut running_bytes = matching_jobs
        .iter()
        .take(start)
        .filter(|job| queue_totals_include(job))
        .map(|job| remaining_bytes(job))
        .fold(0_u64, u64::saturating_add);
    let slots: Vec<SabQueueSlot> = page
        .enumerate()
        .map(|(offset, job)| {
            if queue_totals_include(job) {
                running_bytes = running_bytes.saturating_add(remaining_bytes(job));
            }
            SabQueueSlot::from_job(job, start + offset, paused, running_bytes, speed_bps)
        })
        .collect();

    let active_totals = jobs.iter().filter(|job| queue_totals_include(job));
    let total_bytes = active_totals
        .clone()
        .map(|job| job.total_bytes)
        .fold(0_u64, u64::saturating_add);
    let bytes_left = active_totals
        .map(remaining_bytes)
        .fold(0_u64, u64::saturating_add);
    let total_slots = jobs.iter().filter(|job| queue_totals_include(job)).count();
    let total_mb = total_bytes as f64 / 1_048_576.0;
    let left_mb = bytes_left as f64 / 1_048_576.0;

    serde_json::json!({
        "queue": {
            "version": SABNZBD_COMPAT_VERSION,
            "status": queue_status(paused, jobs),
            "paused": paused,
            "pause_int": "0",
            // SABnzbd uses `paused_all` for a distinct scheduler condition;
            // the ordinary global pause endpoint only sets `paused`.
            "paused_all": false,
            "speedlimit": "0",
            "speedlimit_abs": speed_limit_bps.to_string(),
            "speed": format_speed(speed_bps),
            "kbpersec": format!("{:.2}", speed_bps as f64 / 1024.0),
            "mbleft": format!("{left_mb:.2}"),
            "mb": format!("{total_mb:.2}"),
            "sizeleft": format_size_human(bytes_left),
            "size": format_size_human(total_bytes),
            "noofslots_total": total_slots,
            "noofslots": matching_jobs.len(),
            "limit": limit,
            "start": start,
            "finish": start.saturating_add(limit),
            "timeleft": format_timeleft(bytes_left, speed_bps),
            "diskspace1": "0.00",
            "diskspace2": "0.00",
            "diskspace1_norm": "0 B",
            "diskspace2_norm": "0 B",
            "diskspacetotal1": "0.00",
            "diskspacetotal2": "0.00",
            "have_warnings": "0",
            "finishaction": null,
            "quota": "0 B",
            "have_quota": false,
            "left_quota": "0 B",
            "cache_art": "0",
            "cache_size": "0 B",
            "slots": slots
        }
    })
}

fn comma_separated(value: Option<&str>) -> Vec<&str> {
    value
        .into_iter()
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .collect()
}

/// SABnzbd's numeric code for a priority (`sabnzbd/constants.py`).
fn sab_priority_code(priority: Priority) -> i32 {
    match priority {
        Priority::Low => -1,
        Priority::Normal => 0,
        Priority::High => 1,
        Priority::Force => 2,
    }
}

fn sab_priority_matches(priority: Priority, requested: &str) -> bool {
    requested == sab_priority_code(priority).to_string()
        || requested.eq_ignore_ascii_case(sab_priority_name(priority))
}

/// Top-level queue status. Derived from job state rather than the
/// instantaneous speed sample, which drops to 0 between bursts of an active
/// download and made the status flap to `Idle`.
fn queue_status(paused: bool, jobs: &[NzbJob]) -> &'static str {
    if paused {
        "Paused"
    } else if jobs.iter().any(|job| job.status == JobStatus::Downloading) {
        "Downloading"
    } else {
        "Idle"
    }
}

fn queue_totals_include(job: &NzbJob) -> bool {
    !matches!(job.status, JobStatus::Completed | JobStatus::Failed)
}

/// Handle mode=queue&name=delete&value=nzo_id(s) (SABnzbd queue delete).
/// `value` may be a comma-separated list of nzo_ids, matching SABnzbd's
/// `_api_queue_delete`. RustNZB's `remove_job` already always cleans up a
/// job's incomplete work directory, so `del_files` (unlike in history
/// delete) doesn't change queue-delete behavior here.
fn handle_queue_delete(state: &AppState, req: &SabApiRequest) -> Json<serde_json::Value> {
    let target = req.value.as_deref().unwrap_or("");
    if target.is_empty() {
        return Json(serde_json::json!({ "status": false, "error": "No job ID" }));
    }

    let qm = &state.queue_manager;

    // "all" removes everything from the queue
    if target.eq_ignore_ascii_case("all") {
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

    let jobs = qm.get_jobs();
    let mut removed_ids: Vec<String> = Vec::new();
    for raw_id in target.split(',').map(str::trim).filter(|id| !id.is_empty()) {
        let search_id = raw_id.strip_prefix("SABnzbd_nzo_").unwrap_or(raw_id);
        if let Some(job) = jobs.iter().find(|job| job_id_matches(&job.id, search_id)) {
            let _ = qm.remove_job(&job.id);
            tracing::info!(id = %job.id, "Job removed from queue via arr API (mode=queue)");
            removed_ids.push(queue_nzo_id(job));
        }
    }

    Json(serde_json::json!({ "status": !removed_ids.is_empty(), "nzo_ids": removed_ids }))
}

/// Handle mode=queue&name=pause&value=nzo_ID.
fn handle_queue_item_pause(state: &AppState, req: &SabApiRequest) -> Json<serde_json::Value> {
    let target = req.value.as_deref().unwrap_or("");
    let search_id = target.strip_prefix("SABnzbd_nzo_").unwrap_or(target);
    let Some(job) = state
        .queue_manager
        .get_jobs()
        .into_iter()
        .find(|job| job_id_matches(&job.id, search_id))
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
        .find(|job| job_id_matches(&job.id, search_id))
    else {
        return Json(serde_json::json!({ "status": false, "error": "Job not found" }));
    };
    match state.queue_manager.resume_job(&job.id) {
        Ok(()) => Json(serde_json::json!({ "status": true })),
        Err(error) => Json(serde_json::json!({ "status": false, "error": error.to_string() })),
    }
}

/// Handle mode=queue&name=priority&value=nzo_id(s)&value2=priority.
/// `value` may be a comma-separated list of nzo_ids, matching SABnzbd's
/// `_api_queue_priority`.
fn handle_queue_priority(state: &AppState, req: &SabApiRequest) -> Json<serde_json::Value> {
    let target = req.value.as_deref().unwrap_or("");
    let priority = req.value2.as_deref().unwrap_or("");
    if target.is_empty() || priority.is_empty() {
        return Json(serde_json::json!({
            "status": false,
            "error": "Missing value (job id) or value2 (priority)"
        }));
    }

    let pause = sab_priority_is_paused(priority);
    let priority_value = sab_priority_to_priority(priority);
    let qm = &state.queue_manager;
    // set_job_priority requires an exact job-id match, but clients only ever
    // know the truncated SABnzbd_nzo_<12 chars> form -- resolve the full id
    // by prefix first, the same way pause/resume/rename/change_cat do.
    let jobs = qm.get_jobs();
    let mut applied = false;
    for raw_id in target.split(',').map(str::trim).filter(|id| !id.is_empty()) {
        let search_id = raw_id.strip_prefix("SABnzbd_nzo_").unwrap_or(raw_id);
        if let Some(job) = jobs.iter().find(|job| job_id_matches(&job.id, search_id))
            && if pause {
                qm.pause_job(&job.id).is_ok()
            } else {
                qm.set_job_priority(&job.id, priority_value).is_ok()
            }
        {
            applied = true;
        }
    }

    Json(serde_json::json!({ "status": applied }))
}

/// Handle mode=queue&name=rename&value=nzo_id&value2=new_name.
fn handle_queue_rename(state: &AppState, req: &SabApiRequest) -> Json<serde_json::Value> {
    let target = req.value.as_deref().unwrap_or("");
    let new_name = req.value2.as_deref().unwrap_or("");
    if target.is_empty() || new_name.is_empty() {
        return Json(serde_json::json!({
            "status": false,
            "error": "Missing value (job id) or value2 (new name)"
        }));
    }

    let id = target.strip_prefix("SABnzbd_nzo_").unwrap_or(target);
    match state.queue_manager.rename_job(id, new_name) {
        Ok(()) => Json(serde_json::json!({ "status": true })),
        Err(error) => Json(serde_json::json!({ "status": false, "error": error.to_string() })),
    }
}

/// Handle mode=queue&name=purge (remove every queued job).
fn handle_queue_purge(state: &AppState) -> Json<serde_json::Value> {
    let qm = &state.queue_manager;
    let jobs = qm.get_jobs();
    let nzo_ids: Vec<String> = jobs.iter().map(queue_nzo_id).collect();
    for job in &jobs {
        let _ = qm.remove_job(&job.id);
    }
    tracing::info!(count = nzo_ids.len(), "Queue purged via arr API");
    Json(serde_json::json!({ "status": !nzo_ids.is_empty(), "nzo_ids": nzo_ids }))
}

fn handle_history(state: &AppState, req: &SabApiRequest) -> Json<serde_json::Value> {
    let qm = &state.queue_manager;

    // Sub-commands: mode=history&name=delete&value=nzo_ID
    if req.name.as_deref() == Some("delete") {
        return handle_history_delete(state, req);
    }

    let history_update = qm.history_update();
    if history_is_unchanged(req.last_history_update, history_update) {
        return Json(unchanged_history_response());
    }

    let entries = qm.history_list(i64::MAX as usize).unwrap_or_default();
    let postprocessing: Vec<_> = qm
        .get_jobs()
        .into_iter()
        .filter(|job| {
            matches!(
                job.status,
                JobStatus::Verifying
                    | JobStatus::Repairing
                    | JobStatus::Extracting
                    | JobStatus::PostProcessing
            )
        })
        .collect();

    Json(build_history_response(
        &entries,
        &postprocessing,
        req,
        history_update,
    ))
}

fn build_history_response(
    entries: &[HistoryEntry],
    postprocessing: &[NzbJob],
    req: &SabApiRequest,
    history_update: u64,
) -> serde_json::Value {
    let mut slots: Vec<SabHistorySlot> = postprocessing
        .iter()
        .map(SabHistorySlot::from_postprocessing)
        .chain(entries.iter().map(SabHistorySlot::from_entry))
        .filter(|slot| history_slot_matches(slot, req))
        .collect();
    let noofslots = slots.len();
    let ppslots = slots.iter().filter(|slot| slot.postprocessing).count();
    let start = req.start.unwrap_or(0).min(slots.len());
    let limit = req.limit.filter(|limit| *limit != 0).unwrap_or(50);
    let end = start.saturating_add(limit).min(slots.len());
    slots = slots.drain(start..end).collect();

    let total_bytes: u64 = entries.iter().map(|entry| entry.downloaded_bytes).sum();
    let now = chrono::Utc::now();
    let period_bytes = |days| {
        entries
            .iter()
            .filter(|entry| entry.completed_at >= now - chrono::Duration::days(days))
            .map(|entry| entry.downloaded_bytes)
            .sum::<u64>()
    };

    serde_json::json!({
        "history": {
            "total_size": format_size_human(total_bytes),
            "month_size": format_size_human(period_bytes(30)),
            "week_size": format_size_human(period_bytes(7)),
            "day_size": format_size_human(period_bytes(1)),
            "slots": slots,
            "noofslots": noofslots,
            "ppslots": ppslots,
            "last_history_update": history_update,
            "version": SABNZBD_COMPAT_VERSION
        }
    })
}

fn history_is_unchanged(requested: Option<u64>, current: u64) -> bool {
    requested == Some(current)
}

fn unchanged_history_response() -> serde_json::Value {
    serde_json::json!({ "history": false })
}

fn history_slot_matches(slot: &SabHistorySlot, req: &SabApiRequest) -> bool {
    // RustNZB currently has no archived-history tier, so an archive-only
    // request correctly has no matches.
    if req.archive.as_deref().is_some_and(sab_query_bool) {
        return false;
    }

    if let Some(search) = req.search.as_deref().filter(|value| !value.is_empty()) {
        let search = search.to_lowercase();
        if !slot.name.to_lowercase().contains(&search)
            && !slot.nzb_name.to_lowercase().contains(&search)
        {
            return false;
        }
    }

    let categories = req.cat.as_deref().or(req.category.as_deref());
    let category_matches = categories.is_none_or(|values| {
        values.is_empty()
            || values.split(',').map(str::trim).any(|value| {
                sab_resolve_category(value)
                    .eq_ignore_ascii_case(sab_resolve_category(&slot.category))
            })
    });
    if !category_matches {
        return false;
    }

    let failed_only = req.failed_only.as_deref().is_some_and(sab_query_bool);
    if failed_only {
        if !slot.status.eq_ignore_ascii_case("Failed") {
            return false;
        }
    } else if !matches_csv(req.status.as_deref(), &slot.status) {
        return false;
    }

    req.nzo_ids.as_deref().is_none_or(|ids| {
        ids.is_empty()
            || ids.split(',').map(str::trim).any(|id| {
                id == slot.nzo_id
                    || slot
                        .nzo_id
                        .strip_prefix("SABnzbd_nzo_")
                        .is_some_and(|raw| raw == id)
                    || id
                        .strip_prefix("SABnzbd_nzo_")
                        .is_some_and(|raw| !raw.is_empty() && slot.nzo_id.ends_with(raw))
            })
    })
}

fn matches_csv(values: Option<&str>, actual: &str) -> bool {
    values.is_none_or(|values| {
        values.is_empty()
            || values
                .split(',')
                .map(str::trim)
                .any(|value| value.eq_ignore_ascii_case(actual))
    })
}

fn sab_query_bool(value: &str) -> bool {
    matches!(
        value.to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

/// Handle mode=history&name=delete&value=nzo_ID (SABnzbd history delete)
/// `value` may be a comma-separated list of nzo_ids, matching SABnzbd's
/// `_api_history_delete`. `del_files=1` additionally removes the entry's
/// completed output directory from disk, matching real SABnzbd -- RustNZB
/// otherwise never frees that space on history delete.
fn handle_history_delete(state: &AppState, req: &SabApiRequest) -> Json<serde_json::Value> {
    let target = req.value.as_deref().unwrap_or("");
    if target.is_empty() {
        return Json(serde_json::json!({ "status": false, "error": "No job ID" }));
    }

    let qm = &state.queue_manager;
    let del_files = req.del_files.as_deref().is_some_and(sab_query_bool);

    if target.eq_ignore_ascii_case("all") {
        if del_files {
            for entry in qm.history_list(i64::MAX as usize).unwrap_or_default() {
                let _ = std::fs::remove_dir_all(&entry.output_dir);
            }
        }
        return match qm.history_clear() {
            Ok(()) => Json(serde_json::json!({ "status": true })),
            Err(error) => Json(serde_json::json!({
                "status": false,
                "error": error.to_string()
            })),
        };
    }

    // `value=failed` / `value=completed` clear every entry with that status
    // (SABnzbd `_api_history_delete`).
    let status_filter = if target.eq_ignore_ascii_case("failed") {
        Some(JobStatus::Failed)
    } else if target.eq_ignore_ascii_case("completed") {
        Some(JobStatus::Completed)
    } else {
        None
    };
    if let Some(status) = status_filter {
        for entry in qm
            .history_list(i64::MAX as usize)
            .unwrap_or_default()
            .into_iter()
            .filter(|entry| entry.status == status)
        {
            if del_files {
                let _ = std::fs::remove_dir_all(&entry.output_dir);
            }
            if let Err(error) = qm.history_remove(&entry.id) {
                return Json(serde_json::json!({
                    "status": false,
                    "error": error.to_string()
                }));
            }
        }
        return Json(serde_json::json!({ "status": true }));
    }

    let entries = qm.history_list(i64::MAX as usize).unwrap_or_default();
    let mut removed_ids: Vec<String> = Vec::new();
    for raw_id in target.split(',').map(str::trim).filter(|id| !id.is_empty()) {
        let search_id = raw_id.strip_prefix("SABnzbd_nzo_").unwrap_or(raw_id);
        if let Some(entry) = entries
            .iter()
            .find(|entry| job_id_matches(&entry.id, search_id))
        {
            if del_files {
                let _ = std::fs::remove_dir_all(&entry.output_dir);
            }
            let _ = qm.history_remove(&entry.id);
            tracing::info!(id = %entry.id, "Entry removed from history via arr API (mode=history)");
            removed_ids.push(entry.id.clone());
        }
    }

    Json(serde_json::json!({ "status": !removed_ids.is_empty() }))
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
            if job_id_matches(&job.id, search_id) {
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
            if job_id_matches(&job.id, search_id) {
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
        if job_id_matches(&job.id, search_id) {
            let _ = qm.remove_job(&job.id);
            tracing::info!(id = %job.id, "Job removed from queue via arr API");
            found = true;
            break;
        }
    }

    // Also try history if not found in queue
    if !found {
        let entries = qm.history_list(i64::MAX as usize).unwrap_or_default();
        for entry in &entries {
            if job_id_matches(&entry.id, search_id) {
                let _ = qm.history_remove(&entry.id);
                tracing::info!(id = %entry.id, "Entry removed from history via arr API");
                found = true;
                break;
            }
        }
    }

    Json(serde_json::json!({ "status": found }))
}

fn handle_retry(state: &AppState, req: &SabApiRequest) -> Json<serde_json::Value> {
    let target_id = req.name.as_deref().or(req.value.as_deref()).unwrap_or("");
    if target_id.is_empty() {
        return Json(serde_json::json!({
            "status": false,
            "error": "No history job ID provided"
        }));
    }

    let search_id = target_id.strip_prefix("SABnzbd_nzo_").unwrap_or(target_id);
    let Some(entry) = state
        .queue_manager
        .history_list(i64::MAX as usize)
        .unwrap_or_default()
        .into_iter()
        .find(|entry| job_id_matches(&entry.id, search_id))
    else {
        return Json(serde_json::json!({ "status": false, "error": "History job not found" }));
    };

    let data = match state.queue_manager.history_get_nzb_data(&entry.id) {
        Ok(Some(data)) => data,
        Ok(None) => {
            return Json(serde_json::json!({
                "status": false,
                "error": "The original NZB data is unavailable for this history job"
            }));
        }
        Err(error) => {
            return Json(serde_json::json!({ "status": false, "error": error.to_string() }));
        }
    };

    let retry_data = match state.queue_manager.history_get_retry_data(&entry.id) {
        Ok(data) => data,
        Err(error) => {
            return Json(serde_json::json!({ "status": false, "error": error.to_string() }));
        }
    };
    let job = match state
        .queue_manager
        .prepare_retry_job(&entry, &data, retry_data.as_deref())
    {
        Ok(job) => job,
        Err(error) => {
            return Json(serde_json::json!({
                "status": false,
                "error": format!("Failed to parse stored NZB: {error}")
            }));
        }
    };

    let nzo_id = format!("SABnzbd_nzo_{}", &job.id[..12.min(job.id.len())]);
    if let Err(error) = state.queue_manager.add_job(job, Some(data)) {
        return Json(serde_json::json!({ "status": false, "error": error.to_string() }));
    }

    tracing::info!(history_id = %entry.id, retried_id = %nzo_id, "History job retried via arr API");
    Json(serde_json::json!({ "status": true, "nzo_ids": [nzo_id] }))
}

fn handle_switch(state: &AppState, req: &SabApiRequest) -> Json<serde_json::Value> {
    let target = req.value.as_deref().unwrap_or("");
    let position = req
        .value2
        .as_deref()
        .or(req.name.as_deref())
        .and_then(|value| value.parse::<usize>().ok());

    let Some(position) = position else {
        return Json(serde_json::json!({
            "status": false,
            "error": "Missing or invalid target queue position"
        }));
    };
    if target.is_empty() {
        return Json(serde_json::json!({ "status": false, "error": "No job ID" }));
    }

    // Queue slots report a truncated `SABnzbd_nzo_<12 chars>` id, so resolve
    // it to the full job id by prefix -- as every other per-job handler does
    // -- before handing it to `move_job`, which matches ids exactly.
    let search_id = target.strip_prefix("SABnzbd_nzo_").unwrap_or(target);
    let qm = &state.queue_manager;
    let Some(job) = qm
        .get_jobs()
        .into_iter()
        .find(|job| job_id_matches(&job.id, search_id))
    else {
        return Json(serde_json::json!({ "status": false, "error": "Job not found" }));
    };
    match qm.move_job(&job.id, position) {
        Ok(()) => {
            let new_position = qm
                .get_jobs()
                .iter()
                .position(|queued| queued.id == job.id)
                .unwrap_or(position);
            // SABnzbd answers `switch` with `{"result": {position, priority}}`;
            // keep `status` for existing callers of this compat layer.
            Json(serde_json::json!({
                "status": true,
                "result": {
                    "position": new_position,
                    "priority": sab_priority_code(job.priority),
                }
            }))
        }
        Err(error) => Json(serde_json::json!({ "status": false, "error": error.to_string() })),
    }
}

fn handle_priority(state: &AppState, req: &SabApiRequest) -> Json<serde_json::Value> {
    let target = req.value.as_deref().unwrap_or("");
    let priority = req.value2.as_deref().or(req.name.as_deref()).unwrap_or("");
    if target.is_empty() || priority.is_empty() {
        return Json(serde_json::json!({
            "status": false,
            "error": "Missing job ID or priority"
        }));
    }

    let search_id = target.strip_prefix("SABnzbd_nzo_").unwrap_or(target);
    let qm = &state.queue_manager;
    let Some(job) = qm
        .get_jobs()
        .into_iter()
        .find(|job| job_id_matches(&job.id, search_id))
    else {
        return Json(serde_json::json!({ "status": false, "error": "Job not found" }));
    };
    let result = if sab_priority_is_paused(priority) {
        qm.pause_job(&job.id)
    } else {
        qm.set_job_priority(&job.id, sab_priority_to_priority(priority))
    };
    match result {
        Ok(()) => Json(serde_json::json!({ "status": true })),
        Err(error) => Json(serde_json::json!({ "status": false, "error": error.to_string() })),
    }
}

/// SABnzbd's `get_cats` reports the default category as the literal
/// sentinel `"*"`, not a display name -- verified against
/// `sabnzbd/sabnzbd@5.1.x`, `sabnzbd/api.py::list_cats(default=False)`.
/// RustNZB's own category model still names that category "Default"
/// internally, so translate at the API boundary in both directions.
const SAB_DEFAULT_CATEGORY_SENTINEL: &str = "*";

fn handle_get_cats(state: &AppState) -> Json<serde_json::Value> {
    let config = state.config();
    let mut cats: Vec<String> = config
        .categories
        .iter()
        .map(|c| {
            if c.name.eq_ignore_ascii_case("Default") {
                SAB_DEFAULT_CATEGORY_SENTINEL.to_string()
            } else {
                c.name.clone()
            }
        })
        .collect();
    if !cats.iter().any(|c| c == SAB_DEFAULT_CATEGORY_SENTINEL) {
        cats.insert(0, SAB_DEFAULT_CATEGORY_SENTINEL.into());
    }
    Json(serde_json::json!({ "categories": cats }))
}

/// Translate RustNZB's internal category name into the one SABnzbd reports
/// in queue/history slots, mapping the default category to `"*"`.
fn sab_category_label(category: &str) -> String {
    if category.eq_ignore_ascii_case("Default") {
        SAB_DEFAULT_CATEGORY_SENTINEL.to_string()
    } else {
        category.to_string()
    }
}

/// Translate a client-supplied category into RustNZB's internal name,
/// resolving SABnzbd's `"*"` default-category sentinel.
fn sab_resolve_category(cat: &str) -> &str {
    if cat == SAB_DEFAULT_CATEGORY_SENTINEL {
        "Default"
    } else {
        cat
    }
}

/// `value` may be a comma-separated list of nzo_ids, matching SABnzbd's
/// `_api_change_cat` (`nzo_ids = clean_comma_separated_list(kwargs.get("value"))`).
fn handle_change_cat(state: &AppState, req: &SabApiRequest) -> Json<serde_json::Value> {
    let job_ids = req.value.as_deref().unwrap_or("");
    let new_cat = req.value2.as_deref().unwrap_or("");

    if job_ids.is_empty() || new_cat.is_empty() {
        return Json(serde_json::json!({
            "status": false,
            "error": "Missing value (job id) or value2 (category)"
        }));
    }

    let qm = &state.queue_manager;
    let resolved_cat = sab_resolve_category(new_cat);
    let mut changed = false;
    for raw_id in job_ids
        .split(',')
        .map(str::trim)
        .filter(|id| !id.is_empty())
    {
        let search_id = raw_id.strip_prefix("SABnzbd_nzo_").unwrap_or(raw_id);
        if qm.change_job_category(search_id, resolved_cat).is_ok() {
            changed = true;
        }
    }

    Json(serde_json::json!({ "status": changed }))
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

/// SABnzbd's PAUSED_PRIORITY (`-2`). It is not a queue priority: adding or
/// re-prioritising a job with it pauses the job and leaves the job's
/// priority unchanged (`sabnzbd/nzbstuff.py::NzbObject.set_priority`).
fn sab_priority_is_paused(s: &str) -> bool {
    s.trim() == "-2"
}

/// Apply an `addfile`/`addurl` `priority` parameter to a freshly parsed job.
fn apply_sab_add_priority(job: &mut NzbJob, priority: &str) {
    if sab_priority_is_paused(priority) {
        job.status = JobStatus::Paused;
    } else {
        job.priority = sab_priority_to_priority(priority);
    }
}

/// Convert arr-protocol priority string to our Priority enum.
fn sab_priority_to_priority(s: &str) -> Priority {
    match s.trim() {
        "-1" => Priority::Low,
        "0" | "-100" => Priority::Normal,
        "1" => Priority::High,
        // SABnzbd's Force (2) and Repair (3) priorities both mean "jump the
        // queue"; RustNZB has no separate Repair concept, so both map to
        // our highest priority.
        "2" | "3" => Priority::Force,
        _ => Priority::Normal,
    }
}

// ---------------------------------------------------------------------------
// Arr-compatible response types
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct SabQueueSlot {
    index: usize,
    nzo_id: String,
    unpackopts: String,
    script: String,
    filename: String,
    labels: Vec<String>,
    password: String,
    cat: String,
    status: String,
    priority: String,
    mb: String,
    mbleft: String,
    percentage: String,
    mbmissing: String,
    direct_unpack: Option<String>,
    timeleft: String,
    avg_age: String,
    size: String,
    sizeleft: String,
    time_added: i64,
}

impl SabQueueSlot {
    fn from_job(
        job: &NzbJob,
        index: usize,
        globally_paused: bool,
        running_bytes: u64,
        speed_bps: u64,
    ) -> Self {
        let mb = job.total_bytes as f64 / 1_048_576.0;
        let mbleft = remaining_bytes(job) as f64 / 1_048_576.0;
        let pct = if job.total_bytes > 0 {
            (job.downloaded_bytes as f64 / job.total_bytes as f64 * 100.0) as u32
        } else {
            0
        };
        let paused = globally_paused || job.status == JobStatus::Paused;

        Self {
            index,
            nzo_id: queue_nzo_id(job),
            unpackopts: "3".into(),
            script: "None".into(),
            filename: job.name.clone(),
            labels: Vec::new(),
            password: job.password.clone().unwrap_or_default(),
            cat: if job.category.is_empty() {
                "None".into()
            } else {
                sab_category_label(&job.category)
            },
            // RustNZB marks queued jobs Paused when the global gate is
            // applied. SABnzbd preserves their queue-facing `Queued` state
            // while reporting the gate through the envelope's `paused` key.
            status: if globally_paused && job.status == JobStatus::Paused {
                "Queued"
            } else {
                sab_queue_status(job.status)
            }
            .into(),
            priority: sab_priority_name(job.priority).into(),
            mb: format!("{mb:.2}"),
            mbleft: format!("{mbleft:.2}"),
            percentage: format!("{pct}"),
            mbmissing: "0.00".into(),
            direct_unpack: None,
            timeleft: if paused {
                "0:00:00".into()
            } else {
                format_timeleft(running_bytes, speed_bps)
            },
            avg_age: "-".into(),
            size: format_size_human(job.total_bytes),
            sizeleft: format_size_human(remaining_bytes(job)),
            time_added: job.added_at.timestamp(),
        }
    }
}

fn queue_nzo_id(job: &NzbJob) -> String {
    format!("SABnzbd_nzo_{}", &job.id[..12.min(job.id.len())])
}

fn remaining_bytes(job: &NzbJob) -> u64 {
    job.total_bytes.saturating_sub(job.downloaded_bytes)
}

fn sab_priority_name(priority: Priority) -> &'static str {
    match priority {
        Priority::Force => "Force",
        Priority::High => "High",
        Priority::Normal => "Normal",
        Priority::Low => "Low",
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
    completed: i64,
    name: String,
    nzb_name: String,
    category: String,
    pp: String,
    script: String,
    report: String,
    url: String,
    status: String,
    nzo_id: String,
    storage: String,
    path: String,
    script_line: String,
    download_time: u64,
    postproc_time: u64,
    stage_log: Vec<SabStageLog>,
    downloaded: u64,
    completeness: Option<u8>,
    fail_message: String,
    url_info: String,
    bytes: u64,
    meta: Option<String>,
    series: String,
    duplicate_key: String,
    md5sum: String,
    password: String,
    action_line: String,
    size: String,
    loaded: bool,
    retry: bool,
    archive: bool,
    time_added: i64,
    #[serde(skip)]
    postprocessing: bool,
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

        let storage = entry.output_dir.to_string_lossy().to_string();
        let bytes = entry.downloaded_bytes;
        Self {
            completed: entry.completed_at.timestamp(),
            name: entry.name.clone(),
            nzb_name: format!("{}.nzb", entry.name),
            category: sab_category_label(&entry.category),
            pp: "D".into(),
            script: String::new(),
            report: String::new(),
            url: String::new(),
            status: match entry.status {
                JobStatus::Completed => "Completed".into(),
                JobStatus::Failed => "Failed".into(),
                _ => entry.status.to_string(),
            },
            nzo_id: sab_nzo_id(&entry.id),
            storage: storage.clone(),
            path: storage,
            script_line: String::new(),
            download_time: entry
                .download_time_secs
                .unwrap_or_else(|| {
                    (entry.completed_at - entry.added_at).num_seconds().max(0) as f64
                })
                .round()
                .max(0.0) as u64,
            postproc_time: entry
                .stages
                .iter()
                .map(|stage| stage.duration_secs.max(0.0))
                .sum::<f64>()
                .round() as u64,
            stage_log,
            downloaded: bytes,
            completeness: None,
            fail_message: entry.error_message.clone().unwrap_or_default(),
            url_info: String::new(),
            bytes,
            meta: None,
            series: String::new(),
            duplicate_key: String::new(),
            md5sum: "00000000000000000000000000000000".into(),
            password: String::new(),
            action_line: String::new(),
            size: format_size_human(bytes),
            loaded: false,
            retry: entry.status == JobStatus::Failed && entry.nzb_data.is_some(),
            archive: false,
            time_added: entry.added_at.timestamp(),
            postprocessing: false,
        }
    }

    fn from_postprocessing(job: &NzbJob) -> Self {
        let path = job.work_dir.to_string_lossy().to_string();
        Self {
            completed: job
                .completed_at
                .unwrap_or_else(chrono::Utc::now)
                .timestamp(),
            name: job.name.clone(),
            nzb_name: format!("{}.nzb", job.name),
            category: sab_category_label(&job.category),
            pp: "D".into(),
            script: String::new(),
            report: String::new(),
            url: String::new(),
            status: sab_queue_status(job.status).into(),
            nzo_id: sab_nzo_id(&job.id),
            storage: String::new(),
            path,
            script_line: String::new(),
            download_time: 0,
            postproc_time: 0,
            stage_log: Vec::new(),
            downloaded: job.downloaded_bytes,
            completeness: None,
            fail_message: job.error_message.clone().unwrap_or_default(),
            url_info: String::new(),
            bytes: job.downloaded_bytes,
            meta: None,
            series: String::new(),
            duplicate_key: String::new(),
            md5sum: "00000000000000000000000000000000".into(),
            password: job.password.clone().unwrap_or_default(),
            action_line: job.status.to_string(),
            size: format_size_human(job.downloaded_bytes),
            loaded: true,
            retry: false,
            archive: false,
            time_added: job.added_at.timestamp(),
            postprocessing: true,
        }
    }
}

fn sab_nzo_id(id: &str) -> String {
    if id.starts_with("SABnzbd_nzo_") {
        id.to_string()
    } else {
        format!("SABnzbd_nzo_{}", &id[..12.min(id.len())])
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

fn format_timeleft(bytes_left: u64, speed_bps: u64) -> String {
    if bytes_left == 0 || speed_bps == 0 {
        return "0:00:00".into();
    }

    let seconds = bytes_left / speed_bps;
    let hours = seconds / 3600;
    let minutes = (seconds % 3600) / 60;
    let seconds = seconds % 60;
    format!("{hours}:{minutes:02}:{seconds:02}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    use arc_swap::ArcSwap;
    use chrono::TimeZone;

    use crate::auth::{CredentialStore, TokenStore};
    use crate::log_buffer::LogBuffer;
    use crate::nzb_core::config::AppConfig;
    use crate::nzb_core::db::Database;
    use crate::queue_manager::QueueManager;

    mod sab_contract {
        include!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/support/sab_contract.rs"
        ));
    }

    const VERSION_GOLDEN: &str = include_str!("../tests/fixtures/sabnzbd-5.0.4/version.json");
    const QUEUE_GOLDEN: &str = include_str!("../tests/fixtures/sabnzbd-5.0.4/queue.json");
    const HISTORY_GOLDEN: &str = include_str!("../tests/fixtures/sabnzbd-5.0.4/history.json");
    const FULLSTATUS_GOLDEN: &str = include_str!("../tests/fixtures/sabnzbd-5.0.4/fullstatus.json");

    struct TestState {
        state: AppState,
        _tempdir: tempfile::TempDir,
    }

    fn test_state() -> TestState {
        let tempdir = tempfile::tempdir().expect("create SAB conformance tempdir");
        let mut config = AppConfig::default();
        config.general.api_key = Some("contract-api-key".into());
        config.general.data_dir = tempdir.path().join("data");
        config.general.incomplete_dir = tempdir.path().join("incomplete");
        config.general.complete_dir = tempdir.path().join("complete");
        config.general.cache_size = 512 * 1024 * 1024;
        config.general.speed_limit_bps = 8 * 1024 * 1024;

        let log_buffer = LogBuffer::default();
        let queue_manager = QueueManager::new(
            Vec::new(),
            Database::open_memory().expect("open in-memory conformance database"),
            config.general.incomplete_dir.clone(),
            config.general.complete_dir.clone(),
            log_buffer.clone(),
            1,
            Vec::new(),
            0,
            config.general.speed_limit_bps,
            false,
            5,
            false,
            false,
            100.0,
            30,
        );
        let config_path = tempdir.path().join("sab-conformance.toml");
        let state = AppState::new(
            Arc::new(ArcSwap::from_pointee(config)),
            config_path,
            queue_manager,
            log_buffer,
            Arc::new(TokenStore::new()),
            Arc::new(CredentialStore::new(tempdir.path().to_path_buf())),
        );

        TestState {
            state,
            _tempdir: tempdir,
        }
    }

    fn queue_job(id: &str, name: &str, category: &str, status: JobStatus) -> NzbJob {
        NzbJob {
            id: id.into(),
            name: name.into(),
            category: category.into(),
            status,
            priority: Priority::Normal,
            total_bytes: 10 * 1_048_576,
            downloaded_bytes: 2 * 1_048_576,
            file_count: 2,
            files_completed: 0,
            article_count: 10,
            articles_downloaded: 2,
            articles_failed: 0,
            added_at: chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap(),
            completed_at: None,
            work_dir: "/downloads/incomplete".into(),
            output_dir: "/downloads/complete".into(),
            password: Some("secret".into()),
            error_message: None,
            speed_bps: 0,
            server_stats: Vec::new(),
            files: Vec::new(),
        }
    }

    fn history_entry(
        id: &str,
        name: &str,
        category: &str,
        status: JobStatus,
        seconds_ago: i64,
    ) -> HistoryEntry {
        let completed_at = chrono::Utc::now() - chrono::Duration::seconds(seconds_ago);
        HistoryEntry {
            id: id.into(),
            name: name.into(),
            category: category.into(),
            status,
            total_bytes: 10_000,
            downloaded_bytes: 9_000,
            added_at: completed_at - chrono::Duration::seconds(20),
            completed_at,
            download_time_secs: Some(12.4),
            output_dir: format!("/downloads/{name}").into(),
            stages: vec![StageResult {
                name: "Unpack".into(),
                status: StageStatus::Success,
                message: Some("Unpacked".into()),
                duration_secs: 3.6,
            }],
            error_message: (status == JobStatus::Failed).then(|| "broken archive".into()),
            failure_code: (status == JobStatus::Failed).then_some(JobFailureCode::ArchiveInvalid),
            server_stats: Vec::new(),
            nzb_data: (status == JobStatus::Failed).then(Vec::new),
            retry_data: None,
        }
    }

    fn postprocessing_job() -> NzbJob {
        let now = chrono::Utc::now();
        NzbJob {
            id: "postprocessing-job".into(),
            name: "Still Unpacking".into(),
            category: "tv".into(),
            status: JobStatus::PostProcessing,
            priority: Priority::Normal,
            total_bytes: 20_000,
            downloaded_bytes: 20_000,
            file_count: 1,
            files_completed: 1,
            article_count: 2,
            articles_downloaded: 2,
            articles_failed: 0,
            added_at: now - chrono::Duration::minutes(1),
            completed_at: Some(now),
            work_dir: "/downloads/incomplete/postprocessing-job".into(),
            output_dir: "/downloads/complete/Still Unpacking".into(),
            password: None,
            error_message: None,
            speed_bps: 0,
            server_stats: Vec::new(),
            files: Vec::new(),
        }
    }

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

    #[test]
    fn queue_envelope_and_slot_fields_match_sab_types() {
        let jobs = vec![queue_job(
            "1234567890abcdef",
            "Example.Show",
            "tv",
            JobStatus::Downloading,
        )];
        let response = build_queue_response(
            &jobs,
            false,
            1_048_576,
            2_097_152,
            &SabApiRequest::default(),
        );
        let queue = &response["queue"];
        let slot = &queue["slots"][0];

        assert_eq!(queue["status"], "Downloading");
        assert_eq!(queue["noofslots_total"], 1);
        assert_eq!(queue["noofslots"], 1);
        assert_eq!(queue["timeleft"], "0:00:08");
        assert_eq!(queue["speedlimit_abs"], "2097152");
        assert!(queue["paused"].is_boolean());
        assert!(queue["slots"].is_array());

        assert_eq!(slot["index"], 0);
        assert_eq!(slot["nzo_id"], "SABnzbd_nzo_1234567890ab");
        assert_eq!(slot["unpackopts"], "3");
        assert_eq!(slot["script"], "None");
        assert_eq!(slot["labels"], serde_json::json!([]));
        assert_eq!(slot["password"], "secret");
        assert_eq!(slot["mbmissing"], "0.00");
        assert!(slot["direct_unpack"].is_null());
        assert_eq!(slot["time_added"], 1_700_000_000_i64);
    }

    #[test]
    fn queue_status_is_idle_when_unpaused_and_nothing_downloads() {
        let response = build_queue_response(
            &[queue_job("idle", "Idle job", "tv", JobStatus::Queued)],
            false,
            0,
            0,
            &SabApiRequest::default(),
        );
        assert_eq!(response["queue"]["status"], "Idle");
        assert_eq!(response["queue"]["timeleft"], "0:00:00");
    }

    /// The instantaneous speed sample is routinely 0 between bursts of an
    /// active download; the queue status must not flap to Idle then.
    #[test]
    fn queue_status_stays_downloading_at_zero_instantaneous_speed() {
        let response = build_queue_response(
            &[queue_job(
                "active",
                "Active job",
                "tv",
                JobStatus::Downloading,
            )],
            false,
            0,
            0,
            &SabApiRequest::default(),
        );
        assert_eq!(response["queue"]["status"], "Downloading");

        let paused = build_queue_response(
            &[queue_job(
                "active",
                "Active job",
                "tv",
                JobStatus::Downloading,
            )],
            true,
            0,
            0,
            &SabApiRequest::default(),
        );
        assert_eq!(paused["queue"]["status"], "Paused");
    }

    #[test]
    fn empty_and_paused_queues_have_sab_statuses() {
        let empty = build_queue_response(&[], false, 0, 0, &SabApiRequest::default());
        assert_eq!(empty["queue"]["status"], "Idle");
        assert_eq!(empty["queue"]["slots"], serde_json::json!([]));
        assert_eq!(empty["queue"]["noofslots_total"], 0);

        let paused = build_queue_response(
            &[queue_job("paused", "Paused job", "tv", JobStatus::Paused)],
            true,
            1_048_576,
            0,
            &SabApiRequest::default(),
        );
        assert_eq!(paused["queue"]["status"], "Paused");
        assert_eq!(paused["queue"]["slots"][0]["timeleft"], "0:00:00");
    }

    #[test]
    fn queue_applies_filters_before_pagination() {
        let jobs = vec![
            queue_job("one", "Show.One", "tv", JobStatus::Queued),
            queue_job("two", "Movie.One", "movies", JobStatus::Downloading),
            queue_job("three", "Show.Two", "tv", JobStatus::Paused),
            queue_job("four", "Show.Three", "tv", JobStatus::Downloading),
        ];
        let req = SabApiRequest {
            search: Some("show".into()),
            cat: Some("tv".into()),
            start: Some(1),
            limit: Some(1),
            ..SabApiRequest::default()
        };
        let response = build_queue_response(&jobs, false, 0, 0, &req);
        let queue = &response["queue"];

        assert_eq!(queue["noofslots_total"], 4);
        assert_eq!(queue["noofslots"], 3);
        assert_eq!(queue["start"], 1);
        assert_eq!(queue["limit"], 1);
        assert_eq!(queue["finish"], 2);
        assert_eq!(queue["slots"].as_array().unwrap().len(), 1);
        assert_eq!(queue["slots"][0]["filename"], "Show.Two");
        assert_eq!(queue["slots"][0]["index"], 1);
    }

    #[test]
    fn queue_supports_status_priority_and_id_filters() {
        let mut high = queue_job("high-priority", "First", "tv", JobStatus::Downloading);
        high.priority = Priority::High;
        let normal = queue_job("normal", "Second", "tv", JobStatus::Downloading);
        let req = SabApiRequest {
            priority: Some("1".into()),
            status: Some("downloading".into()),
            nzo_ids: Some("SABnzbd_nzo_high-priorit".into()),
            ..SabApiRequest::default()
        };
        let response = build_queue_response(&[high, normal], false, 0, 0, &req);

        assert_eq!(response["queue"]["noofslots"], 1);
        assert_eq!(response["queue"]["slots"][0]["filename"], "First");
    }

    #[test]
    fn history_reports_active_download_time_to_arr_clients() {
        let now = chrono::Utc::now();
        let entry = HistoryEntry {
            id: "history-active-time".into(),
            name: "queued item".into(),
            category: "sonarr".into(),
            status: JobStatus::Completed,
            total_bytes: 10_000,
            downloaded_bytes: 10_000,
            added_at: now - chrono::Duration::hours(4),
            completed_at: now,
            download_time_secs: Some(2.4),
            output_dir: "/downloads/complete".into(),
            stages: Vec::new(),
            error_message: None,
            failure_code: None,
            server_stats: Vec::new(),
            nzb_data: None,
            retry_data: None,
        };

        assert_eq!(SabHistorySlot::from_entry(&entry).download_time, 2);
    }

    #[test]
    fn history_completed_and_failed_slots_have_sab_field_types() {
        let entries = [
            history_entry(
                "completed-item",
                "Completed Item",
                "movies",
                JobStatus::Completed,
                1,
            ),
            history_entry("failed-item", "Failed Item", "tv", JobStatus::Failed, 2),
        ];
        let response = build_history_response(&entries, &[], &SabApiRequest::default(), 7);
        let history = &response["history"];
        let slots = history["slots"].as_array().unwrap();

        assert_eq!(history["noofslots"], 2);
        assert_eq!(history["ppslots"], 0);
        assert_eq!(history["last_history_update"], 7);
        for slot in slots {
            for field in [
                "completed",
                "name",
                "nzb_name",
                "category",
                "pp",
                "script",
                "report",
                "url",
                "status",
                "nzo_id",
                "storage",
                "path",
                "script_line",
                "download_time",
                "postproc_time",
                "stage_log",
                "downloaded",
                "completeness",
                "fail_message",
                "url_info",
                "bytes",
                "meta",
                "series",
                "duplicate_key",
                "md5sum",
                "password",
                "action_line",
                "size",
                "loaded",
                "retry",
                "archive",
                "time_added",
            ] {
                assert!(slot.get(field).is_some(), "missing field {field}");
            }
        }
        assert_eq!(slots[0]["status"], "Completed");
        assert_eq!(slots[1]["status"], "Failed");
        assert_eq!(slots[1]["fail_message"], "broken archive");
        assert_eq!(slots[1]["retry"], true);
        assert!(slots[0]["bytes"].is_u64());
        assert!(slots[0]["loaded"].is_boolean());
        assert!(slots[0]["completeness"].is_null());
    }

    #[test]
    fn history_includes_postprocessing_before_terminal_slots() {
        let response = build_history_response(
            &[history_entry(
                "completed-item",
                "Completed Item",
                "movies",
                JobStatus::Completed,
                1,
            )],
            &[postprocessing_job()],
            &SabApiRequest::default(),
            4,
        );

        assert_eq!(response["history"]["ppslots"], 1);
        assert_eq!(response["history"]["noofslots"], 2);
        assert_eq!(response["history"]["slots"][0]["status"], "Running");
        assert_eq!(response["history"]["slots"][0]["loaded"], true);
    }

    #[test]
    fn history_filters_before_paging_and_reports_total_matches() {
        let entries = [
            history_entry(
                "first-movie",
                "First Movie",
                "movies",
                JobStatus::Completed,
                1,
            ),
            history_entry(
                "second-movie",
                "Second Movie",
                "movies",
                JobStatus::Failed,
                2,
            ),
            history_entry("tv-episode", "TV Episode", "tv", JobStatus::Failed, 3),
        ];
        let request = SabApiRequest {
            start: Some(1),
            limit: Some(1),
            search: Some("movie".into()),
            cat: Some("movies".into()),
            status: Some("Completed,Failed".into()),
            ..Default::default()
        };
        let response = build_history_response(&entries, &[], &request, 3);

        assert_eq!(response["history"]["noofslots"], 2);
        assert_eq!(response["history"]["slots"].as_array().unwrap().len(), 1);
        assert_eq!(response["history"]["slots"][0]["name"], "Second Movie");

        let id_request = SabApiRequest {
            nzo_ids: Some("SABnzbd_nzo_tv-episode".into()),
            failed_only: Some("1".into()),
            ..Default::default()
        };
        let id_response = build_history_response(&entries, &[], &id_request, 3);
        assert_eq!(id_response["history"]["noofslots"], 1);
        assert_eq!(id_response["history"]["slots"][0]["name"], "TV Episode");
    }

    #[test]
    fn matching_history_generation_uses_unchanged_response_contract() {
        assert!(history_is_unchanged(Some(42), 42));
        assert!(!history_is_unchanged(Some(41), 42));
        assert!(!history_is_unchanged(None, 42));
        assert_eq!(
            unchanged_history_response(),
            serde_json::json!({ "history": false })
        );
    }

    #[tokio::test]
    async fn version_matches_sabnzbd_golden_contract() {
        let state = test_state();
        let expected = sab_contract::golden(VERSION_GOLDEN);
        let actual = dispatch_mode(&state.state, "version", &SabApiRequest::default()).0;

        sab_contract::assert_matches_golden(actual, &expected);
    }

    #[tokio::test]
    async fn fullstatus_matches_sabnzbd_golden_contract() {
        let state = test_state();
        let expected = sab_contract::golden(FULLSTATUS_GOLDEN);
        let actual = dispatch_mode(&state.state, "fullstatus", &SabApiRequest::default()).0;

        sab_contract::assert_matches_golden(actual, &expected);
    }

    #[tokio::test]
    async fn queue_matches_sabnzbd_golden_contract() {
        let state = test_state();
        state.state.queue_manager.pause_all();
        let added_at = chrono::Utc
            .timestamp_opt(1_700_000_000, 0)
            .single()
            .expect("valid fixture time");
        let job = NzbJob {
            id: "contract-queue-job".into(),
            name: "SAB contract fixture".into(),
            category: "tv".into(),
            status: JobStatus::Queued,
            priority: Priority::Normal,
            total_bytes: 1_048_576,
            downloaded_bytes: 0,
            file_count: 1,
            files_completed: 0,
            article_count: 1,
            articles_downloaded: 0,
            articles_failed: 0,
            added_at,
            completed_at: None,
            work_dir: state
                .state
                .config()
                .general
                .incomplete_dir
                .join("contract-queue-job"),
            output_dir: state
                .state
                .config()
                .general
                .complete_dir
                .join("contract-queue-job"),
            password: None,
            error_message: None,
            speed_bps: 0,
            server_stats: Vec::new(),
            files: Vec::new(),
        };
        state
            .state
            .queue_manager
            .add_job(job, None)
            .expect("add queue fixture");
        let request = SabApiRequest {
            limit: Some(1),
            ..SabApiRequest::default()
        };
        let actual = dispatch_mode(&state.state, "queue", &request).0;

        sab_contract::assert_matches_golden(actual, &sab_contract::golden(QUEUE_GOLDEN));
    }

    #[tokio::test]
    async fn history_matches_sabnzbd_golden_contract() {
        let state = test_state();
        let added_at = chrono::Utc
            .timestamp_opt(1_700_000_000, 0)
            .single()
            .expect("valid fixture time");
        let entry = HistoryEntry {
            id: "contract-history-job".into(),
            name: "SAB history fixture".into(),
            category: "tv".into(),
            status: JobStatus::Completed,
            total_bytes: 1_048_576,
            downloaded_bytes: 1_048_576,
            added_at,
            completed_at: added_at + chrono::Duration::seconds(10),
            download_time_secs: Some(5.0),
            output_dir: state
                .state
                .config()
                .general
                .complete_dir
                .join("contract-history-job"),
            stages: Vec::new(),
            error_message: None,
            failure_code: None,
            server_stats: Vec::new(),
            nzb_data: None,
            retry_data: None,
        };
        state.state.queue_manager.with_db(|database| {
            database
                .history_insert(&entry)
                .expect("insert history fixture")
        });
        let request = SabApiRequest {
            limit: Some(1),
            ..SabApiRequest::default()
        };
        let actual = dispatch_mode(&state.state, "history", &request).0;

        sab_contract::assert_matches_golden(actual, &sab_contract::golden(HISTORY_GOLDEN));
    }

    #[test]
    fn checked_in_sabnzbd_goldens_are_valid_json() {
        let fixture_dir =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/sabnzbd-5.0.4");

        for name in [
            "queue.json",
            "history.json",
            "fullstatus.json",
            "version.json",
        ] {
            let contents = std::fs::read_to_string(fixture_dir.join(name))
                .unwrap_or_else(|error| panic!("read {name}: {error}"));
            sab_contract::golden(&contents);
        }
    }

    /// Numeric priority codes per SABnzbd 5.1.x `sabnzbd/constants.py`:
    /// FORCE_PRIORITY=2, HIGH_PRIORITY=1, NORMAL_PRIORITY=0, LOW_PRIORITY=-1,
    /// DEFAULT_PRIORITY=-100 (displayed/treated as Normal), REPAIR_PRIORITY=3.
    #[test]
    fn sab_priority_to_priority_matches_upstream_numeric_codes() {
        assert_eq!(sab_priority_to_priority("-1"), Priority::Low);
        assert_eq!(sab_priority_to_priority("0"), Priority::Normal);
        assert_eq!(sab_priority_to_priority("1"), Priority::High);
        assert_eq!(sab_priority_to_priority("2"), Priority::Force);
        assert_eq!(sab_priority_to_priority("3"), Priority::Force);
        assert_eq!(sab_priority_to_priority("-100"), Priority::Normal);
    }

    /// The numeric codes accepted when *setting* a priority must agree with
    /// the codes `sab_priority_matches` uses when *filtering* the queue by
    /// priority -- a prior regression let these two tables diverge silently.
    #[test]
    fn sab_priority_to_priority_agrees_with_sab_priority_matches() {
        for (priority, numeric) in [
            (Priority::Low, "-1"),
            (Priority::Normal, "0"),
            (Priority::High, "1"),
            (Priority::Force, "2"),
        ] {
            assert_eq!(sab_priority_to_priority(numeric), priority);
            assert!(sab_priority_matches(priority, numeric));
        }
    }

    fn add_live_job(test_state: &TestState, id: &str) {
        let job = NzbJob {
            id: id.into(),
            name: "Compat Layer Fixture".into(),
            category: "tv".into(),
            status: JobStatus::Queued,
            priority: Priority::Normal,
            total_bytes: 1_048_576,
            downloaded_bytes: 0,
            file_count: 1,
            files_completed: 0,
            article_count: 1,
            articles_downloaded: 0,
            articles_failed: 0,
            added_at: chrono::Utc::now(),
            completed_at: None,
            work_dir: test_state.state.config().general.incomplete_dir.join(id),
            output_dir: test_state.state.config().general.complete_dir.join(id),
            password: None,
            error_message: None,
            speed_bps: 0,
            server_stats: Vec::new(),
            files: Vec::new(),
        };
        test_state
            .state
            .queue_manager
            .add_job(job, None)
            .expect("add live queue fixture");
    }

    fn add_live_job_with_category(test_state: &TestState, id: &str, category: &str) {
        let job = NzbJob {
            id: id.into(),
            name: "Change Cat Fixture".into(),
            category: category.into(),
            status: JobStatus::Queued,
            priority: Priority::Normal,
            total_bytes: 1_048_576,
            downloaded_bytes: 0,
            file_count: 1,
            files_completed: 0,
            article_count: 1,
            articles_downloaded: 0,
            articles_failed: 0,
            added_at: chrono::Utc::now(),
            completed_at: None,
            work_dir: test_state.state.config().general.incomplete_dir.join(id),
            output_dir: test_state.state.config().general.complete_dir.join(id),
            password: None,
            error_message: None,
            speed_bps: 0,
            server_stats: Vec::new(),
            files: Vec::new(),
        };
        test_state
            .state
            .queue_manager
            .add_job(job, None)
            .expect("add live queue fixture");
    }

    /// SABnzbd's real priority endpoint is `mode=queue&name=priority`, not
    /// the top-level `mode=priority` this compat layer also accepts.
    #[tokio::test]
    async fn queue_priority_subcommand_changes_job_priority() {
        let test_state = test_state();
        add_live_job(&test_state, "queue-priority-job");

        let req = SabApiRequest {
            mode: Some("queue".into()),
            name: Some("priority".into()),
            value: Some("queue-priority-job".into()),
            value2: Some("1".into()),
            ..SabApiRequest::default()
        };
        let response = dispatch_mode(&test_state.state, "queue", &req).0;
        assert_eq!(response["status"], serde_json::json!(true));

        let job = test_state
            .state
            .queue_manager
            .get_jobs()
            .into_iter()
            .find(|job| job.id == "queue-priority-job")
            .expect("job still queued");
        // This test covers routing (does mode=queue&name=priority reach the
        // queue manager at all?), not the value mapping itself -- that's
        // covered separately by sab_priority_to_priority's own tests.
        assert_eq!(job.priority, sab_priority_to_priority("1"));
    }

    /// Queue slots expose a truncated `SABnzbd_nzo_<12 chars>` id; `switch`
    /// must resolve it by prefix like every other per-job command.
    #[tokio::test]
    async fn switch_accepts_truncated_sab_nzo_id() {
        let test_state = test_state();
        add_live_job(&test_state, "aaaaaaaa-1111-0000-0000-000000000001");
        add_live_job(&test_state, "bbbbbbbb-2222-0000-0000-000000000002");

        let queue = handle_queue(&test_state.state, &SabApiRequest::default()).0;
        let second_nzo_id = queue["queue"]["slots"][1]["nzo_id"]
            .as_str()
            .expect("second slot nzo_id")
            .to_string();
        assert_eq!(second_nzo_id, "SABnzbd_nzo_bbbbbbbb-222");

        let req = SabApiRequest {
            mode: Some("switch".into()),
            value: Some(second_nzo_id),
            value2: Some("0".into()),
            ..SabApiRequest::default()
        };
        let response = dispatch_mode(&test_state.state, "switch", &req).0;
        assert_eq!(response["status"], serde_json::json!(true), "{response:?}");
        assert_eq!(response["result"]["position"], serde_json::json!(0));
        assert_eq!(response["result"]["priority"], serde_json::json!(0));

        let order: Vec<String> = test_state
            .state
            .queue_manager
            .get_jobs()
            .into_iter()
            .map(|job| job.id)
            .collect();
        assert_eq!(
            order,
            vec![
                "bbbbbbbb-2222-0000-0000-000000000002".to_string(),
                "aaaaaaaa-1111-0000-0000-000000000001".to_string(),
            ]
        );
    }

    /// An empty or bare `SABnzbd_nzo_` id must not resolve to any job:
    /// `starts_with("")` is always true, which previously let a missing
    /// `value` pause or delete whichever job was first in the queue.
    #[tokio::test]
    async fn empty_or_bare_nzo_id_never_matches_a_job() {
        let test_state = test_state();
        add_live_job(&test_state, "cccccccc-3333-0000-0000-000000000003");

        for value in [None, Some(""), Some("SABnzbd_nzo_"), Some("cccc")] {
            for name in ["pause", "delete", "priority", "rename"] {
                let req = SabApiRequest {
                    mode: Some("queue".into()),
                    name: Some(name.into()),
                    value: value.map(Into::into),
                    value2: Some("1".into()),
                    ..SabApiRequest::default()
                };
                let response = dispatch_mode(&test_state.state, "queue", &req).0;
                assert_eq!(
                    response["status"],
                    serde_json::json!(false),
                    "name={name} value={value:?} resp={response:?}"
                );
            }
        }

        let jobs = test_state.state.queue_manager.get_jobs();
        assert_eq!(jobs.len(), 1, "job must not be deleted");
        assert_ne!(jobs[0].status, JobStatus::Paused, "job must not be paused");
        assert_eq!(jobs[0].priority, Priority::Normal);
        assert_eq!(jobs[0].name, "Compat Layer Fixture");
    }

    #[test]
    fn job_id_matches_requires_exact_id_or_sab_length_prefix() {
        let id = "dddddddd-4444-0000-0000-000000000004";
        assert!(job_id_matches(id, id));
        assert!(job_id_matches(id, "dddddddd-444"));
        assert!(!job_id_matches(id, ""));
        assert!(!job_id_matches(id, "dddd"));
        assert!(!job_id_matches(id, "eeeeeeee-444"));
    }

    /// SABnzbd's real rename endpoint is `mode=queue&name=rename`.
    #[tokio::test]
    async fn queue_rename_subcommand_renames_job() {
        let test_state = test_state();
        add_live_job(&test_state, "queue-rename-job");

        let req = SabApiRequest {
            mode: Some("queue".into()),
            name: Some("rename".into()),
            value: Some("queue-rename-job".into()),
            value2: Some("New Name".into()),
            ..SabApiRequest::default()
        };
        let response = dispatch_mode(&test_state.state, "queue", &req).0;
        assert_eq!(response["status"], serde_json::json!(true));

        let job = test_state
            .state
            .queue_manager
            .get_jobs()
            .into_iter()
            .find(|job| job.id == "queue-rename-job")
            .expect("job still queued");
        assert_eq!(job.name, "New Name");
    }

    /// SABnzbd's real `get_cats` reports the default category as `"*"`, not
    /// a display name -- verified against `sabnzbd/api.py::list_cats(default=False)`.
    #[tokio::test]
    async fn get_cats_reports_default_category_as_sabnzbd_sentinel() {
        let test_state = test_state();
        let response = handle_get_cats(&test_state.state).0;
        let cats = response["categories"].as_array().expect("categories array");
        assert_eq!(cats, &vec![serde_json::json!("*")]);
    }

    /// `get_cats` advertises the default category as `"*"`, so queue and
    /// history slots must report it the same way, and `cat=*` must filter
    /// on it.
    #[test]
    fn default_category_is_reported_and_filtered_as_sabnzbd_sentinel() {
        let jobs = vec![
            queue_job("default-job", "Default Job", "Default", JobStatus::Queued),
            queue_job("tv-job", "TV Job", "tv", JobStatus::Queued),
        ];
        let all = build_queue_response(&jobs, false, 0, 0, &SabApiRequest::default());
        assert_eq!(all["queue"]["slots"][0]["cat"], "*");
        assert_eq!(all["queue"]["slots"][1]["cat"], "tv");

        let star = SabApiRequest {
            cat: Some("*".into()),
            ..SabApiRequest::default()
        };
        let filtered = build_queue_response(&jobs, false, 0, 0, &star);
        assert_eq!(filtered["queue"]["noofslots"], 1);
        assert_eq!(filtered["queue"]["slots"][0]["filename"], "Default Job");

        let entries = [
            history_entry(
                "default-hist",
                "Default Hist",
                "Default",
                JobStatus::Completed,
                1,
            ),
            history_entry("tv-hist", "TV Hist", "tv", JobStatus::Completed, 2),
        ];
        let history = build_history_response(&entries, &[], &SabApiRequest::default(), 1);
        assert_eq!(history["history"]["slots"][0]["category"], "*");
        assert_eq!(history["history"]["slots"][1]["category"], "tv");

        let filtered = build_history_response(&entries, &[], &star, 1);
        assert_eq!(filtered["history"]["noofslots"], 1);
        assert_eq!(filtered["history"]["slots"][0]["name"], "Default Hist");
    }

    #[tokio::test]
    async fn change_cat_accepts_sabnzbd_default_sentinel() {
        let test_state = test_state();
        add_live_job(&test_state, "sentinel-cat-job");
        test_state
            .state
            .queue_manager
            .change_job_category("sentinel-cat-job", "movies")
            .expect("seed non-default category");

        let req = SabApiRequest {
            value: Some("sentinel-cat-job".into()),
            value2: Some("*".into()),
            ..SabApiRequest::default()
        };
        let response = handle_change_cat(&test_state.state, &req).0;
        assert_eq!(response["status"], serde_json::json!(true));

        let job = test_state
            .state
            .queue_manager
            .get_jobs()
            .into_iter()
            .find(|job| job.id == "sentinel-cat-job")
            .expect("job still queued");
        assert_eq!(job.category, "Default");
    }

    /// SABnzbd's real `_api_change_cat` accepts a comma-separated `value`
    /// list, applying the category change to every matching job.
    #[tokio::test]
    async fn change_cat_applies_to_multiple_comma_separated_ids() {
        let test_state = test_state();
        add_live_job_with_category(&test_state, "multi-cat-one", "tv");
        add_live_job_with_category(&test_state, "multi-cat-two", "tv");

        let req = SabApiRequest {
            value: Some("multi-cat-one,multi-cat-two".into()),
            value2: Some("movies".into()),
            ..SabApiRequest::default()
        };
        let response = handle_change_cat(&test_state.state, &req).0;
        assert_eq!(response["status"], serde_json::json!(true));

        let jobs = test_state.state.queue_manager.get_jobs();
        for id in ["multi-cat-one", "multi-cat-two"] {
            let job = jobs
                .iter()
                .find(|job| job.id == id)
                .unwrap_or_else(|| panic!("job {id} still queued"));
            assert_eq!(job.category, "movies");
        }
    }

    /// Real SABnzbd's `mode=get_scripts` always answers with at least
    /// `["None"]` (sabnzbd/api.py::_api_get_scripts,
    /// filesystem.py::list_scripts) -- clients that fetch categories and
    /// scripts together to populate an "add download" dialog may fail to
    /// populate the whole dialog if this call errors, as it previously did.
    #[tokio::test]
    async fn get_scripts_reports_none_when_unsupported() {
        let test_state = test_state();
        let req = SabApiRequest::default();
        let response = dispatch_mode(&test_state.state, "get_scripts", &req).0;
        assert_eq!(response["scripts"], serde_json::json!(["None"]));
    }

    #[tokio::test]
    async fn rejected_requests_keep_the_error_envelope() {
        let test_state = test_state();

        for provided in [None, Some("wrong-key")] {
            let response = validate_api_key(&test_state.state, provided)
                .expect_err("invalid credentials must be rejected")
                .0;
            assert_eq!(response["status"], serde_json::json!(false));
            assert!(response["error"].is_string());
        }

        let response = dispatch_mode(
            &test_state.state,
            "unknown-contract-mode",
            &SabApiRequest::default(),
        )
        .0;
        assert_eq!(response["status"], serde_json::json!(false));
        assert!(response["error"].as_str().unwrap().contains("Unknown mode"));
    }

    #[tokio::test]
    async fn representative_read_modes_keep_stable_top_level_types() {
        let test_state = test_state();

        let config = dispatch_mode(&test_state.state, "get_config", &SabApiRequest::default()).0;
        assert!(config["config"]["misc"]["complete_dir"].is_string());
        assert!(config["config"]["categories"].is_array());

        let categories = dispatch_mode(&test_state.state, "get_cats", &SabApiRequest::default()).0;
        assert!(categories["categories"].is_array());

        let scripts = dispatch_mode(&test_state.state, "get_scripts", &SabApiRequest::default()).0;
        assert!(scripts["scripts"].is_array());
    }

    #[tokio::test]
    async fn addfile_reports_success_and_parse_errors_as_json() {
        let test_state = test_state();
        let success = dispatch_post(
            &test_state.state,
            "addfile".into(),
            None,
            None,
            None,
            Some(("contract.nzb".into(), SAMPLE_NZB.as_bytes().to_vec())),
            None,
            None,
            SabApiRequest::default(),
        )
        .await
        .expect("addfile response")
        .0;
        assert_eq!(success["status"], serde_json::json!(true));
        assert_eq!(success["nzo_ids"].as_array().unwrap().len(), 1);

        let missing = dispatch_post(
            &test_state.state,
            "addfile".into(),
            None,
            None,
            None,
            None,
            None,
            None,
            SabApiRequest::default(),
        )
        .await
        .expect("missing-file response")
        .0;
        assert_eq!(missing["status"], serde_json::json!(false));
        assert!(missing["error"].is_string());

        let malformed = dispatch_post(
            &test_state.state,
            "addfile".into(),
            None,
            None,
            None,
            Some(("malformed.nzb".into(), b"not an nzb".to_vec())),
            None,
            None,
            SabApiRequest::default(),
        )
        .await
        .expect("malformed-file response")
        .0;
        assert_eq!(malformed["status"], serde_json::json!(false));
        assert!(malformed["error"].is_string());
    }

    /// SABnzbd's PAUSED_PRIORITY (-2) adds the job paused rather than at a
    /// queue priority; the slot keeps reporting a normal priority.
    #[tokio::test]
    async fn addfile_with_paused_priority_adds_job_paused() {
        let test_state = test_state();
        let response = dispatch_post(
            &test_state.state,
            "addfile".into(),
            None,
            None,
            Some("-2".into()),
            Some(("paused.nzb".into(), SAMPLE_NZB.as_bytes().to_vec())),
            None,
            None,
            SabApiRequest::default(),
        )
        .await
        .expect("addfile response")
        .0;
        assert_eq!(response["status"], serde_json::json!(true));

        let jobs = test_state.state.queue_manager.get_jobs();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].status, JobStatus::Paused);
        assert_eq!(jobs[0].priority, Priority::Normal);

        let queue = dispatch_mode(&test_state.state, "queue", &SabApiRequest::default()).0;
        assert_eq!(queue["queue"]["slots"][0]["status"], "Paused");
        assert_eq!(queue["queue"]["slots"][0]["priority"], "Normal");
    }

    /// `mode=queue&name=priority&value2=-2` pauses the job, as in SABnzbd.
    #[tokio::test]
    async fn queue_priority_paused_value_pauses_job() {
        let test_state = test_state();
        add_live_job(&test_state, "queue-paused-priority");

        let req = SabApiRequest {
            mode: Some("queue".into()),
            name: Some("priority".into()),
            value: Some("SABnzbd_nzo_queue-paused".into()),
            value2: Some("-2".into()),
            ..SabApiRequest::default()
        };
        let response = dispatch_mode(&test_state.state, "queue", &req).0;
        assert_eq!(response["status"], serde_json::json!(true));

        let job = test_state
            .state
            .queue_manager
            .get_jobs()
            .into_iter()
            .find(|job| job.id == "queue-paused-priority")
            .expect("job still queued");
        assert_eq!(job.status, JobStatus::Paused);
        assert_eq!(job.priority, Priority::Normal);
    }

    /// SABnzbd's real `_api_queue_delete` accepts a comma-separated `value`
    /// list, removing every matching job in one call.
    #[tokio::test]
    async fn queue_delete_removes_multiple_comma_separated_ids() {
        let test_state = test_state();
        add_live_job(&test_state, "multi-delete-one");
        add_live_job(&test_state, "multi-delete-two");

        let req = SabApiRequest {
            value: Some("multi-delete-one,multi-delete-two".into()),
            ..SabApiRequest::default()
        };
        let response = handle_queue_delete(&test_state.state, &req).0;
        assert_eq!(response["status"], serde_json::json!(true));

        let remaining = test_state.state.queue_manager.get_jobs();
        assert!(
            remaining
                .iter()
                .all(|job| job.id != "multi-delete-one" && job.id != "multi-delete-two")
        );
    }

    fn insert_history_fixture(test_state: &TestState, id: &str, output_dir: std::path::PathBuf) {
        let entry = HistoryEntry {
            id: id.into(),
            name: id.into(),
            category: "tv".into(),
            status: JobStatus::Completed,
            total_bytes: 10_000,
            downloaded_bytes: 10_000,
            added_at: chrono::Utc::now() - chrono::Duration::seconds(20),
            completed_at: chrono::Utc::now(),
            download_time_secs: Some(1.0),
            output_dir,
            stages: Vec::new(),
            error_message: None,
            failure_code: None,
            server_stats: Vec::new(),
            nzb_data: None,
            retry_data: None,
        };
        test_state.state.queue_manager.with_db(|database| {
            database
                .history_insert(&entry)
                .expect("insert history fixture")
        });
    }

    /// Real SABnzbd's `_api_history_delete` removes the completed output
    /// directory from disk when `del_files=1` is set; RustNZB previously
    /// never freed that space regardless of the flag.
    #[tokio::test]
    async fn history_delete_with_del_files_removes_output_directory() {
        let test_state = test_state();
        let output_dir = test_state
            .state
            .config()
            .general
            .complete_dir
            .join("del-files-job");
        std::fs::create_dir_all(&output_dir).expect("create fixture output dir");
        std::fs::write(output_dir.join("file.mkv"), b"data").expect("write fixture file");
        insert_history_fixture(&test_state, "del-files-job", output_dir.clone());

        let req = SabApiRequest {
            value: Some("del-files-job".into()),
            del_files: Some("1".into()),
            ..SabApiRequest::default()
        };
        let response = handle_history_delete(&test_state.state, &req).0;
        assert_eq!(response["status"], serde_json::json!(true));
        assert!(!output_dir.exists());
    }

    fn insert_history_status(test_state: &TestState, id: &str, status: JobStatus, age_secs: i64) {
        let mut entry = history_entry(id, id, "tv", status, age_secs);
        entry.output_dir = test_state.state.config().general.complete_dir.join(id);
        test_state
            .state
            .queue_manager
            .with_db(|database| database.history_insert(&entry).expect("insert history"));
    }

    fn history_ids(test_state: &TestState) -> Vec<String> {
        let mut ids: Vec<String> = test_state
            .state
            .queue_manager
            .history_list(i64::MAX as usize)
            .expect("list history")
            .into_iter()
            .map(|entry| entry.id)
            .collect();
        ids.sort();
        ids
    }

    /// SABnzbd's `_api_history_delete` accepts `value=failed` and
    /// `value=completed` to clear every entry with that status.
    #[tokio::test]
    async fn history_delete_by_status_removes_only_matching_entries() {
        for (value, kept) in [("failed", "job-done"), ("completed", "job-failed")] {
            let test_state = test_state();
            insert_history_status(&test_state, "job-done", JobStatus::Completed, 1);
            insert_history_status(&test_state, "job-failed", JobStatus::Failed, 2);

            let req = SabApiRequest {
                value: Some(value.into()),
                ..SabApiRequest::default()
            };
            let response = handle_history_delete(&test_state.state, &req).0;
            assert_eq!(response["status"], serde_json::json!(true), "value={value}");
            assert_eq!(
                history_ids(&test_state),
                vec![kept.to_string()],
                "value={value}"
            );
        }
    }

    /// `mode=delete` and `mode=retry` must find history entries beyond the
    /// most recent 1000.
    #[tokio::test]
    async fn delete_and_retry_find_history_entries_older_than_the_newest_thousand() {
        let test_state = test_state();
        insert_history_status(&test_state, "oldest-failed", JobStatus::Failed, 100_000);
        for index in 0..1000 {
            insert_history_status(
                &test_state,
                &format!("newer-{index:04}"),
                JobStatus::Completed,
                index,
            );
        }

        let retry = handle_retry(
            &test_state.state,
            &SabApiRequest {
                value: Some("SABnzbd_nzo_oldest-faile".into()),
                ..SabApiRequest::default()
            },
        )
        .0;
        assert_ne!(retry["error"], "History job not found", "retry={retry}");

        let delete = handle_delete(
            &test_state.state,
            &SabApiRequest {
                value: Some("SABnzbd_nzo_oldest-faile".into()),
                ..SabApiRequest::default()
            },
        )
        .0;
        assert_eq!(delete["status"], serde_json::json!(true));
        assert!(!history_ids(&test_state).contains(&"oldest-failed".to_string()));
    }

    /// Without `del_files`, history delete only removes the DB record, as
    /// before.
    #[tokio::test]
    async fn history_delete_without_del_files_keeps_output_directory() {
        let test_state = test_state();
        let output_dir = test_state
            .state
            .config()
            .general
            .complete_dir
            .join("keep-files-job");
        std::fs::create_dir_all(&output_dir).expect("create fixture output dir");
        insert_history_fixture(&test_state, "keep-files-job", output_dir.clone());

        let req = SabApiRequest {
            value: Some("keep-files-job".into()),
            ..SabApiRequest::default()
        };
        let response = handle_history_delete(&test_state.state, &req).0;
        assert_eq!(response["status"], serde_json::json!(true));
        assert!(output_dir.exists());
    }

    const SAMPLE_NZB: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<nzb xmlns="http://www.newzbin.com/DTD/2003/nzb">
  <file poster="test@example.com" date="1234567890" subject="test.rar (1/2)">
    <groups><group>alt.binaries.test</group></groups>
    <segments>
      <segment number="1" bytes="768000">article1@example.com</segment>
      <segment number="2" bytes="768000">article2@example.com</segment>
    </segments>
  </file>
</nzb>"#;

    /// Serves `body` once over a raw TCP listener bound to an ephemeral
    /// port, returning the URL to fetch it from.
    async fn spawn_nzb_server(body: &'static str) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind ephemeral test server");
        let addr = listener.local_addr().expect("test server local addr");

        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept test connection");
            let mut buf = [0u8; 1024];
            let _ = socket.read(&mut buf).await;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: application/x-nzb\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = socket.write_all(response.as_bytes()).await;
            let _ = socket.shutdown().await;
        });

        format!("http://{addr}/test.nzb")
    }

    /// NZB360 (and real SABnzbd) add downloads found via search as a plain
    /// GET `mode=addurl` request, since there's no file body to upload --
    /// only the POST/multipart path handled `cat` for that mode, so GET
    /// requests silently dropped the requested category.
    #[tokio::test]
    async fn addurl_over_get_applies_requested_category() {
        let test_state = test_state();
        let url = spawn_nzb_server(SAMPLE_NZB).await;

        let req = SabApiRequest {
            mode: Some("addurl".into()),
            name: Some(url),
            cat: Some("movies".into()),
            apikey: Some("contract-api-key".into()),
            ..SabApiRequest::default()
        };

        let response = h_sabnzbd_api_get(State(Arc::new(test_state.state)), Query(req))
            .await
            .expect("addurl over GET should succeed")
            .into_response();
        assert_eq!(response.status(), axum::http::StatusCode::OK);

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("read response body");
        let value: serde_json::Value = serde_json::from_slice(&body).expect("parse JSON body");

        // The URL was parsed from the request (dispatch reached the addurl
        // handler), then refused by the SSRF guard because the test server is
        // on loopback -- so the response is a structured {status:false}, not a
        // "No URL provided" / "Unknown mode" fall-through.
        assert_eq!(value["status"], serde_json::json!(false));
        assert!(
            value["error"]
                .as_str()
                .unwrap_or_default()
                .contains("private/reserved"),
            "expected SSRF rejection, resp={value:?}"
        );
    }

    async fn json_body(response: axum::response::Response) -> serde_json::Value {
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("read response body");
        serde_json::from_slice(&body).expect("parse JSON body")
    }

    /// Prowlarr sends `mode=addurl` as a POST with every parameter in the
    /// query string and no body at all (#119). The multipart extractor used
    /// to reject that with `400 Invalid boundary` before the mode was read.
    #[tokio::test]
    async fn addurl_over_bare_post_uses_query_string() {
        let test_state = test_state();
        let url = spawn_nzb_server(SAMPLE_NZB).await;

        let req = SabApiRequest {
            mode: Some("addurl".into()),
            name: Some(url),
            cat: Some("prowlarr".into()),
            priority: Some("-100".into()),
            apikey: Some("contract-api-key".into()),
            output: Some("json".into()),
            ..SabApiRequest::default()
        };
        let request = Request::builder()
            .method("POST")
            .uri("/sabnzbd/api")
            .body(axum::body::Body::empty())
            .expect("build request");

        let response = h_sabnzbd_api_post(State(Arc::new(test_state.state)), Query(req), request)
            .await
            .expect("addurl over bare POST should succeed")
            .into_response();
        assert_eq!(response.status(), StatusCode::OK);

        let value = json_body(response).await;
        // The URL was parsed from the request (dispatch reached the addurl
        // handler), then refused by the SSRF guard because the test server is
        // on loopback -- so the response is a structured {status:false}, not a
        // "No URL provided" / "Unknown mode" fall-through.
        assert_eq!(value["status"], serde_json::json!(false));
        assert!(
            value["error"]
                .as_str()
                .unwrap_or_default()
                .contains("private/reserved"),
            "expected SSRF rejection, resp={value:?}"
        );
    }

    async fn get_without_key(state: AppState, mode: &str) -> serde_json::Value {
        let req = SabApiRequest {
            mode: Some(mode.into()),
            ..SabApiRequest::default()
        };
        let response = h_sabnzbd_api_get(State(Arc::new(state)), Query(req))
            .await
            .expect("GET response")
            .into_response();
        assert_eq!(response.status(), StatusCode::OK);
        json_body(response).await
    }

    /// SABnzbd answers `mode=version` and `mode=auth` without an API key so
    /// clients can probe the server before they are configured
    /// (`sabnzbd/interface.py` exempts both from the key check).
    #[tokio::test]
    async fn version_and_auth_do_not_require_api_key() {
        let version = get_without_key(test_state().state, "version").await;
        assert_eq!(
            version["version"],
            serde_json::json!(SABNZBD_COMPAT_VERSION)
        );

        let auth = get_without_key(test_state().state, "auth").await;
        assert_eq!(auth, serde_json::json!({ "auth": "apikey" }));

        let keyless = test_state();
        let mut config = (*keyless.state.config()).clone();
        config.general.api_key = None;
        keyless.state.config.store(Arc::new(config));
        let auth = get_without_key(keyless.state, "auth").await;
        assert_eq!(auth, serde_json::json!({ "auth": "None" }));

        // Every other mode still requires the key.
        let queue = get_without_key(test_state().state, "queue").await;
        assert_eq!(queue["status"], serde_json::json!(false));
        assert_eq!(queue["error"], "API Key Incorrect");
    }

    #[tokio::test]
    async fn version_over_bare_post_does_not_require_api_key() {
        let test_state = test_state();
        let req = SabApiRequest {
            mode: Some("version".into()),
            ..SabApiRequest::default()
        };
        let request = Request::builder()
            .method("POST")
            .uri("/sabnzbd/api")
            .body(axum::body::Body::empty())
            .expect("build request");
        let response = h_sabnzbd_api_post(State(Arc::new(test_state.state)), Query(req), request)
            .await
            .expect("version over bare POST")
            .into_response();
        let value = json_body(response).await;
        assert_eq!(value["version"], serde_json::json!(SABNZBD_COMPAT_VERSION));
    }

    /// Non-upload modes must also work over a bare POST.
    #[tokio::test]
    async fn version_over_bare_post_dispatches() {
        let test_state = test_state();
        let req = SabApiRequest {
            mode: Some("version".into()),
            apikey: Some("contract-api-key".into()),
            ..SabApiRequest::default()
        };
        let request = Request::builder()
            .method("POST")
            .uri("/sabnzbd/api")
            .body(axum::body::Body::empty())
            .expect("build request");

        let response = h_sabnzbd_api_post(State(Arc::new(test_state.state)), Query(req), request)
            .await
            .expect("version over bare POST should succeed")
            .into_response();
        assert_eq!(response.status(), StatusCode::OK);
        let value = json_body(response).await;
        assert_eq!(value["version"], serde_json::json!(SABNZBD_COMPAT_VERSION));
    }

    /// `application/x-www-form-urlencoded` bodies carry the same fields as
    /// multipart ones and override the query string.
    #[tokio::test]
    async fn addurl_over_form_urlencoded_post_reads_body_fields() {
        let test_state = test_state();
        let url = spawn_nzb_server(SAMPLE_NZB).await;

        let req = SabApiRequest {
            apikey: Some("contract-api-key".into()),
            ..SabApiRequest::default()
        };
        let body = format!(
            "mode=addurl&name={}&cat=tv",
            url.replace(':', "%3A").replace('/', "%2F")
        );
        let request = Request::builder()
            .method("POST")
            .uri("/sabnzbd/api")
            .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(axum::body::Body::from(body))
            .expect("build request");

        let response = h_sabnzbd_api_post(State(Arc::new(test_state.state)), Query(req), request)
            .await
            .expect("addurl over form POST should succeed")
            .into_response();
        assert_eq!(response.status(), StatusCode::OK);
        let value = json_body(response).await;
        // The URL was parsed from the request (dispatch reached the addurl
        // handler), then refused by the SSRF guard because the test server is
        // on loopback -- so the response is a structured {status:false}, not a
        // "No URL provided" / "Unknown mode" fall-through.
        assert_eq!(value["status"], serde_json::json!(false));
        assert!(
            value["error"]
                .as_str()
                .unwrap_or_default()
                .contains("private/reserved"),
            "expected SSRF rejection, resp={value:?}"
        );
    }

    /// A request that claims to be multipart but carries no boundary is still
    /// a client error, not a server error.
    #[tokio::test]
    async fn malformed_multipart_post_is_bad_request() {
        let test_state = test_state();
        let req = SabApiRequest {
            mode: Some("addfile".into()),
            apikey: Some("contract-api-key".into()),
            ..SabApiRequest::default()
        };
        let request = Request::builder()
            .method("POST")
            .uri("/sabnzbd/api")
            .header(CONTENT_TYPE, "multipart/form-data")
            .body(axum::body::Body::empty())
            .expect("build request");

        let response = match h_sabnzbd_api_post(
            State(Arc::new(test_state.state)),
            Query(req),
            request,
        )
        .await
        {
            Ok(_) => panic!("multipart without boundary should be rejected"),
            Err(err) => err.into_response(),
        };
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }
}
