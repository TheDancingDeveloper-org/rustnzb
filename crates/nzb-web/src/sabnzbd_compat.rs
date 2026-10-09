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
    /// Job name override for `addfile`/`addurl` (SABnzbd `nzbname`).
    pub nzbname: Option<String>,
    /// Post-processing override for `addfile`/`addurl` (SABnzbd `pp`, 0-3).
    /// SABnzbd's `script` parameter is accepted and ignored (unknown query
    /// fields are not rejected); RustNZB has no per-job scripts.
    pub pp: Option<String>,
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
        // `name` (or `value`) is the URL; the job name comes from `nzbname`
        // or is derived from the fetched NZB, never from the URL string.
        let url = req.name.clone().or_else(|| req.value.clone());
        return handle_addurl(
            &state,
            url,
            req.nzbname.clone(),
            req.cat.clone(),
            req.priority.clone(),
            req.password.clone(),
            req.pp.clone(),
        )
        .await;
    }

    let result = dispatch_mode(&state, mode, &req);
    Ok(result)
}

/// Fetch an NZB from a URL and enqueue it, applying category/priority/password
/// overrides. Shared by the GET and POST `addurl` entry points. `nzbname`
/// is the optional job-name override.
async fn handle_addurl(
    state: &AppState,
    url: Option<String>,
    nzbname: Option<String>,
    cat: Option<String>,
    priority: Option<String>,
    password: Option<String>,
    pp: Option<String>,
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
    let fetch_plan = match crate::fetch_guard::validate_fetch_url_with(
        &url,
        &crate::fetch_guard::FetchPolicy::from_config(&state.config().general),
    )
    .await
    {
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

    let content_disposition = response
        .headers()
        .get(reqwest::header::CONTENT_DISPOSITION)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);

    // Cap the fetched body to avoid unbounded memory from a hostile URL.
    let data =
        crate::fetch_guard::read_response_bytes_limited(response, MAX_ADDURL_BODY_BYTES).await?;

    // Unpack compressed NZBs (.nzb.gz, .nzb.bz2, .zip) the way uploads are.
    // A multi-NZB zip is handed to addfile, which enqueues each NZB.
    let url_file_name = url
        .rsplit('/')
        .next()
        .and_then(|s| s.split('?').next())
        .unwrap_or("unknown")
        .to_string();
    let mut nzbs = match crate::nzb_archive::extract_nzbs(&url_file_name, &data) {
        Ok(nzbs) => nzbs,
        Err(error) => {
            return Ok(Json(serde_json::json!({
                "status": false,
                "error": error.to_string()
            })));
        }
    };
    if nzbs.len() > 1 {
        return Box::pin(dispatch_post(
            state,
            "addfile".into(),
            None,
            cat,
            priority,
            Some((url_file_name, data)),
            None,
            password,
            SabApiRequest {
                nzbname,
                pp,
                ..SabApiRequest::default()
            },
        ))
        .await;
    }
    let (nzb_file_name, data) = nzbs.pop().expect("extract_nzbs returns at least one NZB");

    // Adapted: job naming keeps our SABnzbd order (nzbname, then
    // Content-Disposition, then the URL path), but when neither override nor
    // disposition names the job the decompressed member is used, so a fetched
    // .nzb.gz is named after the inner NZB rather than the archive.
    let job_name = if nzbname.as_deref().and_then(clean_nzb_name).is_none()
        && content_disposition
            .as_deref()
            .and_then(content_disposition_filename)
            .is_none()
    {
        clean_nzb_name(&nzb_file_name).unwrap_or_else(|| "unknown".to_string())
    } else {
        addurl_job_name(
            nzbname.as_deref(),
            content_disposition.as_deref(),
            &fetch_plan.url,
        )
    };

    match nzb_parser::parse_nzb(&job_name, &data) {
        Ok(mut job) => {
            if let Some(ref c) = cat
                && !c.is_empty()
            {
                job.category = sab_add_category(state, c);
            }
            if let Some(ref p) = priority {
                apply_sab_add_priority(&mut job, p);
            }

            // API-provided password overrides NZB metadata password
            if let Some(ref pw) = password {
                job.password = Some(pw.clone());
            }
            job.pp_override = sab_pp_override(pp.as_deref());

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

            let nzo_id = format!("SABnzbd_nzo_{}", sab_id_prefix(&job.id));
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

/// Job name for an `addurl` fetch, in SABnzbd's order of preference: the
/// `nzbname` parameter, else the `Content-Disposition` filename, else the
/// last path segment of the URL (query string excluded). The `.nzb`
/// extension is dropped. The URL itself is never used: it is not a valid
/// single path component, so every such job failed to enqueue.
fn addurl_job_name(
    nzbname: Option<&str>,
    content_disposition: Option<&str>,
    url: &reqwest::Url,
) -> String {
    nzbname
        .and_then(clean_nzb_name)
        .or_else(|| {
            content_disposition
                .and_then(content_disposition_filename)
                .as_deref()
                .and_then(clean_nzb_name)
        })
        .or_else(|| {
            url.path_segments()
                .and_then(|mut segments| segments.next_back())
                .map(percent_decode_lossy)
                .as_deref()
                .and_then(clean_nzb_name)
        })
        .unwrap_or_else(|| "unknown".to_string())
}

/// Reduce a client- or server-supplied NZB name to a bare job name: last
/// path component, without a `.nzb` extension. `None` if nothing is left.
fn clean_nzb_name(raw: &str) -> Option<String> {
    let base = raw.rsplit(['/', '\\']).next().unwrap_or(raw).trim();
    let name = if base.len() > 4 && base[base.len() - 4..].eq_ignore_ascii_case(".nzb") {
        &base[..base.len() - 4]
    } else {
        base
    };
    let name = name.trim();
    (!name.is_empty() && name != "." && name != "..").then(|| name.to_string())
}

/// The filename of a `Content-Disposition` header, preferring the RFC 5987
/// `filename*` form over plain `filename`.
fn content_disposition_filename(header: &str) -> Option<String> {
    let mut plain = None;
    for part in header.split(';') {
        let Some((key, value)) = part.split_once('=') else {
            continue;
        };
        let value = value.trim().trim_matches('"');
        match key.trim().to_ascii_lowercase().as_str() {
            "filename*" => {
                // charset'language'percent-encoded-name
                let encoded = value.splitn(3, '\'').nth(2).unwrap_or(value);
                return Some(percent_decode_lossy(encoded));
            }
            "filename" => plain = Some(value.to_string()),
            _ => {}
        }
    }
    plain
}

/// Decode `%XX` escapes, replacing invalid UTF-8 with U+FFFD.
fn percent_decode_lossy(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && let Some(byte) = value
                .get(index + 1..index + 3)
                .and_then(|hex| u8::from_str_radix(hex, 16).ok())
        {
            decoded.push(byte);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8_lossy(&decoded).into_owned()
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
    Query(mut query_req): Query<SabApiRequest>,
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
            if form.nzbname.is_some() {
                query_req.nzbname = form.nzbname;
            }
            if form.pp.is_some() {
                query_req.pp = form.pp;
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
                &mut query_req.nzbname,
                &mut query_req.pp,
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
    nzbname: &mut Option<String>,
    pp: &mut Option<String>,
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
            "nzbname" => {
                if let Ok(text) = field.text().await {
                    *nzbname = Some(text);
                }
            }
            "pp" => {
                if let Ok(text) = field.text().await {
                    *pp = Some(text);
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
/// query string and (optional) body. `query_req.nzbname` carries the merged
/// `nzbname` job-name override and `query_req.pp` the merged `pp`
/// post-processing override.
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

            // Unpack compressed uploads (.nzb.gz, .nzb.bz2, .zip) with the
            // same code and limits as the native add endpoint.
            let mut nzbs = match crate::nzb_archive::extract_nzbs(&file_name, &data) {
                Ok(nzbs) => nzbs,
                Err(error) => {
                    return Ok(Json(serde_json::json!({
                        "status": false,
                        "error": error.to_string()
                    })));
                }
            };
            if nzbs.len() > 1 {
                return add_each_nzb(
                    state,
                    nzbs,
                    cat,
                    priority,
                    password,
                    query_req.nzbname.clone(),
                    query_req.pp.clone(),
                )
                .await;
            }
            let (entry_name, data) = nzbs.pop().expect("extract_nzbs returns at least one NZB");
            let file_name = nzb_entry_file_name(&entry_name);

            let job_name = query_req
                .nzbname
                .as_deref()
                .and_then(clean_nzb_name)
                .or_else(|| name.clone())
                .unwrap_or_else(|| {
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
                        job.category = sab_add_category(state, c);
                    }
                    if let Some(ref p) = priority {
                        apply_sab_add_priority(&mut job, p);
                    }

                    // API-provided password overrides NZB metadata password
                    if let Some(ref pw) = password {
                        job.password = Some(pw.clone());
                    }
                    job.pp_override = sab_pp_override(query_req.pp.as_deref());

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

                    let nzo_id = format!("SABnzbd_nzo_{}", sab_id_prefix(&job.id));
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
            // `value`/`url` (or `name`) is the URL to fetch, not a job name.
            let url = nzb_url.or_else(|| name.clone());
            handle_addurl(
                state,
                url,
                query_req.nzbname,
                cat,
                priority,
                password,
                query_req.pp,
            )
            .await
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
                nzbname: query_req.nzbname,
                pp: query_req.pp,
            };
            Ok(dispatch_mode(
                state,
                req.mode.as_deref().unwrap_or(""),
                &req,
            ))
        }
    }
}

/// Zip entries may sit in folders; a job is named after the file alone.
fn nzb_entry_file_name(entry_name: &str) -> String {
    entry_name
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(entry_name)
        .to_string()
}

/// Enqueue every NZB of a multi-NZB archive as its own job (SABnzbd adds
/// each NZB in a zip separately), reporting all resulting nzo_ids.
async fn add_each_nzb(
    state: &AppState,
    nzbs: Vec<(String, Vec<u8>)>,
    cat: Option<String>,
    priority: Option<String>,
    password: Option<String>,
    nzbname: Option<String>,
    pp: Option<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let mut nzo_ids = Vec::new();
    let mut errors = Vec::new();
    for (entry_name, data) in nzbs {
        let file_name = nzb_entry_file_name(&entry_name);
        let response = Box::pin(dispatch_post(
            state,
            "addfile".into(),
            None,
            cat.clone(),
            priority.clone(),
            Some((file_name, data)),
            None,
            password.clone(),
            SabApiRequest {
                // Only the first member keeps the explicit nzbname; the rest
                // are named after their own files.
                nzbname: if nzo_ids.is_empty() {
                    nzbname.clone()
                } else {
                    None
                },
                pp: pp.clone(),
                ..SabApiRequest::default()
            },
        ))
        .await?
        .0;
        match response["nzo_ids"].as_array() {
            Some(ids) => nzo_ids.extend(ids.iter().cloned()),
            None => errors.push(format!(
                "{entry_name}: {}",
                response["error"].as_str().unwrap_or("failed to add")
            )),
        }
    }
    if nzo_ids.is_empty() {
        return Ok(Json(serde_json::json!({
            "status": false,
            "error": errors.join("; ")
        })));
    }
    Ok(Json(
        serde_json::json!({ "status": true, "nzo_ids": nzo_ids }),
    ))
}

/// Dispatch an API mode to the appropriate handler.
fn dispatch_mode(state: &AppState, mode: &str, req: &SabApiRequest) -> Json<serde_json::Value> {
    match mode {
        "version" => Json(serde_json::json!({
            "version": SABNZBD_COMPAT_VERSION
        })),

        "queue" => handle_queue(state, req),

        "history" => handle_history(state, req),

        "get_config" => handle_get_config(state),

        "config" => handle_config(state, req),

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

        "fullstatus" => handle_fullstatus(state),

        "server_stats" => handle_server_stats(state),

        "pause" => handle_pause(state, req),

        "resume" => handle_resume(state, req),

        "delete" => handle_delete(state, req),

        "retry" => handle_retry(state, req),

        // SABnzbd `_api_warnings`. RustNZB does not keep a SABnzbd-style
        // warnings list (queue/fullstatus report `have_warnings: "0"` and
        // `warnings: []`), so this is the same empty list; `name=clear`
        // is accepted.
        "warnings" => match req.name.as_deref() {
            Some("clear") => Json(serde_json::json!({ "status": true })),
            _ => Json(serde_json::json!({ "warnings": Vec::<serde_json::Value>::new() })),
        },

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
    let (free1, total1, free1_norm) = sab_disk_space(&qm.incomplete_dir());
    let (free2, total2, free2_norm) = sab_disk_space(&qm.complete_dir());

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
            "diskspace1": free1,
            "diskspace1_norm": free1_norm,
            "diskspace2": free2,
            "diskspace2_norm": free2_norm,
            "diskspacetotal1": total1,
            "diskspacetotal2": total2,
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
            "servers": sab_status_servers(state),
            "speedlimit": sab_speedlimit_percent(speed_limit),
            "speedlimit_abs": speed_limit.to_string(),
            "uptime": format_sab_age(state.started_at.elapsed().as_secs()),
            "url_base": "",
            "version": SABNZBD_COMPAT_VERSION,
            "warnings": Vec::<serde_json::Value>::new(),
            "webdir": "",
            "weblogfile": serde_json::Value::Null,
            "windows": cfg!(target_os = "windows"),
        }
    }))
}

/// `mode=server_stats`, in SABnzbd's shape (`api.py::_api_server_stats`):
/// downloaded-byte totals overall and per server, keyed by server name.
/// The data is the same as the native `GET /api/config/servers/stats`.
/// RustNZB keeps rolling 1/7/30-day windows rather than calendar ones, and
/// no per-day timeline, so `daily` is empty.
fn handle_server_stats(state: &AppState) -> Json<serde_json::Value> {
    let config = state.config();
    let stats = state.queue_manager.server_stats_get_all(&config.servers);

    let mut servers = serde_json::Map::new();
    let (mut total, mut month, mut week, mut day) = (0_u64, 0_u64, 0_u64, 0_u64);
    for server in &stats {
        total = total.saturating_add(server.total_bytes);
        month = month.saturating_add(server.month_bytes);
        week = week.saturating_add(server.week_bytes);
        day = day.saturating_add(server.today_bytes);

        let mut key = if server.server_name.is_empty() {
            server.server_id.clone()
        } else {
            server.server_name.clone()
        };
        if servers.contains_key(&key) {
            // Two servers share a display name; keep both, keyed apart.
            key = format!("{key} ({})", server.server_id);
        }
        servers.insert(
            key,
            serde_json::json!({
                "total": server.total_bytes,
                "month": server.month_bytes,
                "week": server.week_bytes,
                "day": server.today_bytes,
                "daily": serde_json::Map::new(),
                "articles_tried": server.total_ok.saturating_add(server.total_fail),
                "articles_success": server.total_ok,
            }),
        );
    }

    Json(serde_json::json!({
        "total": total,
        "month": month,
        "week": week,
        "day": day,
        "servers": servers,
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

    let mut response = build_queue_response(&jobs, paused, speed_bps, speed_limit_bps, req);
    let queue = &mut response["queue"];
    // Seconds left of a timed pause (POST /api/queue/pause-for), else "0".
    queue["pause_int"] =
        serde_json::Value::from(qm.pause_remaining_secs().unwrap_or(0).max(0).to_string());
    let (free1, total1, free1_norm) = sab_disk_space(&qm.incomplete_dir());
    let (free2, total2, free2_norm) = sab_disk_space(&qm.complete_dir());
    queue["diskspace1"] = free1.into();
    queue["diskspacetotal1"] = total1.into();
    queue["diskspace1_norm"] = free1_norm.into();
    queue["diskspace2"] = free2.into();
    queue["diskspacetotal2"] = total2.into();
    queue["diskspace2_norm"] = free2_norm.into();
    Json(response)
}

/// SABnzbd's `speedlimit` is a percentage of the configured maximum
/// bandwidth. RustNZB has no maximum-bandwidth setting, so an active limit
/// is reported as 100% and no limit as "0"; the absolute value is carried
/// by `speedlimit_abs`.
fn sab_speedlimit_percent(speed_limit_bps: u64) -> &'static str {
    if speed_limit_bps == 0 { "0" } else { "100" }
}

/// SABnzbd's disk-space values for the filesystem holding `dir`: free and
/// total space in GiB formatted `%.2f` (`diskspaceN` / `diskspacetotalN`),
/// and the free space in human units (`diskspaceN_norm`).
fn sab_disk_space(dir: &std::path::Path) -> (String, String, String) {
    const GIB: f64 = 1024.0 * 1024.0 * 1024.0;
    let (free, total) = crate::queue_manager::disk_space(dir);
    (
        format!("{:.2}", free as f64 / GIB),
        format!("{:.2}", total as f64 / GIB),
        format_size_human(free),
    )
}

/// Format an elapsed time like SABnzbd's `calc_age` (used for `uptime`):
/// whole days, else whole hours, else whole minutes.
fn format_sab_age(seconds: u64) -> String {
    if seconds >= 86_400 {
        format!("{}d", seconds / 86_400)
    } else if seconds >= 3_600 {
        format!("{}h", seconds / 3_600)
    } else {
        format!("{}m", seconds / 60)
    }
}

/// fullstatus `servers` entries for the configured news servers, using
/// SABnzbd's key names. Only values RustNZB tracks are populated; the rest
/// use SABnzbd's idle defaults.
fn sab_status_servers(state: &AppState) -> Vec<serde_json::Value> {
    let connections = state.queue_manager.connection_snapshot();
    state
        .config()
        .servers
        .iter()
        .map(|server| {
            let active = connections
                .iter()
                .find(|(id, _, _)| *id == server.id)
                .map_or(0, |(_, active, _)| *active);
            serde_json::json!({
                "servername": if server.name.is_empty() { &server.host } else { &server.name },
                "serveractiveconn": active,
                "servertotalconn": server.connections,
                "serverconnections": Vec::<serde_json::Value>::new(),
                "serverssl": server.ssl,
                "serveractive": server.enabled,
                "servererror": "",
                "serverpriority": server.priority,
                "serveroptional": server.optional,
            })
        })
        .collect()
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
            "speedlimit": sab_speedlimit_percent(speed_limit_bps),
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
                let raw = slot
                    .nzo_id
                    .strip_prefix("SABnzbd_nzo_")
                    .unwrap_or(&slot.nzo_id);
                let query = id.strip_prefix("SABnzbd_nzo_").unwrap_or(id);
                id == slot.nzo_id || job_id_matches(raw, query)
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
        let entries = qm.history_list(i64::MAX as usize).unwrap_or_default();
        return match qm.history_clear() {
            Ok(()) => {
                if del_files {
                    delete_history_files(qm, &entries, &[]);
                }
                Json(serde_json::json!({ "status": true }))
            }
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
        let entries = qm.history_list(i64::MAX as usize).unwrap_or_default();
        let (removed, kept): (Vec<_>, Vec<_>) = entries
            .iter()
            .cloned()
            .partition(|entry| entry.status == status);
        let mut removed_ok = Vec::new();
        for entry in &removed {
            if let Err(error) = qm.history_remove(&entry.id) {
                if del_files {
                    delete_history_files(qm, &removed_ok, &kept);
                }
                return Json(serde_json::json!({
                    "status": false,
                    "error": error.to_string()
                }));
            }
            removed_ok.push(entry.clone());
        }
        if del_files {
            delete_history_files(qm, &removed, &kept);
        }
        return Json(serde_json::json!({ "status": true }));
    }

    let entries = qm.history_list(i64::MAX as usize).unwrap_or_default();
    let mut removed = Vec::new();
    for raw_id in target.split(',').map(str::trim).filter(|id| !id.is_empty()) {
        let search_id = raw_id.strip_prefix("SABnzbd_nzo_").unwrap_or(raw_id);
        if let Some(entry) = entries
            .iter()
            .find(|entry| job_id_matches(&entry.id, search_id))
            && qm.history_remove(&entry.id).is_ok()
        {
            tracing::info!(id = %entry.id, "Entry removed from history via arr API (mode=history)");
            removed.push(entry.clone());
        }
    }
    let removed_ids: Vec<String> = removed.iter().map(|entry| entry.id.clone()).collect();
    let kept: Vec<_> = entries
        .iter()
        .filter(|entry| !removed_ids.contains(&entry.id))
        .cloned()
        .collect();
    if del_files {
        delete_history_files(qm, &removed, &kept);
    }

    Json(serde_json::json!({ "status": !removed_ids.is_empty() }))
}

/// Remove the output directories of `removed`, but never a directory that a
/// surviving history entry or a queued job still uses, and never a path that
/// is not strictly inside the complete directory or a category directory.
fn delete_history_files(
    qm: &crate::queue_manager::QueueManager,
    removed: &[crate::nzb_core::models::HistoryEntry],
    kept: &[crate::nzb_core::models::HistoryEntry],
) {
    let mut protected: Vec<std::path::PathBuf> = kept
        .iter()
        .map(|entry| entry.output_dir.clone())
        .chain(qm.get_jobs().into_iter().map(|job| job.output_dir))
        .collect();
    for entry in removed {
        if !history_output_dir_is_deletable(qm, &entry.output_dir, &protected) {
            tracing::warn!(
                dir = %entry.output_dir.display(),
                "Skipping history file deletion: directory is shared or outside the download roots"
            );
            continue;
        }
        if let Err(error) = std::fs::remove_dir_all(&entry.output_dir) {
            tracing::warn!(dir = %entry.output_dir.display(), %error, "History file deletion failed");
        }
        protected.push(entry.output_dir.clone());
    }
}

fn history_output_dir_is_deletable(
    qm: &crate::queue_manager::QueueManager,
    dir: &std::path::Path,
    protected: &[std::path::PathBuf],
) -> bool {
    if dir.as_os_str().is_empty() || protected.iter().any(|used| used == dir) {
        return false;
    }
    let Ok(canonical) = std::fs::canonicalize(dir) else {
        return false;
    };
    let mut roots = vec![qm.complete_dir()];
    for category in qm.categories() {
        if let Some(root) = category.output_dir {
            roots.push(if root.is_absolute() {
                root
            } else {
                qm.complete_dir().join(root)
            });
        }
    }
    roots.iter().any(|root| {
        std::fs::canonicalize(root)
            .is_ok_and(|root| canonical.starts_with(&root) && canonical != root)
    })
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
        if let Some(job) = qm
            .get_jobs()
            .into_iter()
            .find(|job| job_id_matches(&job.id, search_id))
        {
            let _ = qm.pause_job(&job.id);
            tracing::info!(id = %job.id, "Job paused via arr API");
            return Json(serde_json::json!({ "status": true }));
        }
        // SABnzbd's `mode=pause` takes no job id; a value that names no job
        // (e.g. a duration) must still pause the queue, not be ignored.
    }

    // No specific ID -- pause all. Like SABnzbd's `_api_pause`
    // (`plan_resume(0)`), this is an indefinite pause: pause_all cancels
    // any pending timed resume.
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

    let nzo_id = format!("SABnzbd_nzo_{}", sab_id_prefix(&job.id));
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

/// Resolve the `cat` of an `addfile`/`addurl` request to a configured
/// category. SABnzbd matches category names case-insensitively and falls
/// back to the default category for an unknown one, instead of storing the
/// client's string verbatim.
fn sab_add_category(state: &AppState, cat: &str) -> String {
    let config = state.config();
    let requested = sab_resolve_category(cat);
    config
        .categories
        .iter()
        .find(|category| category.name.eq_ignore_ascii_case(requested))
        .or_else(|| {
            config
                .categories
                .iter()
                .find(|category| category.name.eq_ignore_ascii_case("Default"))
        })
        .or_else(|| config.categories.first())
        .map_or_else(|| "Default".to_string(), |category| category.name.clone())
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

/// Handle `mode=config`. With a `name`, this is SABnzbd's setter table
/// (`sabnzbd/api.py::_api_config_table`); without one it keeps returning
/// the configuration, as `mode=config` always has here.
fn handle_config(state: &AppState, req: &SabApiRequest) -> Json<serde_json::Value> {
    let value = req.value.as_deref().unwrap_or("").trim();
    match req.name.as_deref().unwrap_or("") {
        "" => handle_get_config(state),
        "speedlimit" => handle_config_speedlimit(state, value),
        "set_pause" => {
            // SABnzbd `plan_resume(minutes)`: a positive value pauses and
            // schedules a resume, 0 cancels the timed pause and resumes.
            let qm = &state.queue_manager;
            match value.parse::<u64>().unwrap_or(0) {
                0 => qm.resume_all(),
                minutes => qm.pause_for(minutes.saturating_mul(60)),
            }
            Json(serde_json::json!({ "status": true }))
        }
        _ => Json(serde_json::json!({ "status": false, "error": "not implemented" })),
    }
}

/// `mode=config&name=speedlimit&value=...`, following SABnzbd's
/// `Downloader.limit_speed`: a value ending in `%`, or a bare number from 1
/// to 100, is a percentage of the maximum bandwidth; anything else is an
/// absolute bytes/sec value with an optional K/M/G/T (1024-based) suffix.
/// An empty value or 0 removes the limit.
fn handle_config_speedlimit(state: &AppState, value: &str) -> Json<serde_json::Value> {
    let qm = &state.queue_manager;
    let (number, explicit_percent) = match value.strip_suffix('%') {
        Some(number) => (number.trim(), true),
        None => (value, false),
    };
    let amount = if number.is_empty() {
        Some(0.0)
    } else {
        parse_sab_units(number)
    };
    let Some(amount) = amount else {
        return Json(serde_json::json!({
            "status": false,
            "error": format!("Invalid speed limit: {value}")
        }));
    };

    if explicit_percent || (amount > 0.0 && amount < 101.0) {
        // A percentage of the maximum bandwidth. RustNZB has no maximum
        // bandwidth setting, so only "no limit" (0% / 100%) can be applied.
        if amount == 0.0 || amount >= 100.0 {
            qm.set_speed_limit(0);
            return Json(serde_json::json!({ "status": true }));
        }
        return Json(serde_json::json!({
            "status": false,
            "error": "A percentage speed limit requires a maximum bandwidth, which RustNZB \
                      does not have; use an absolute value such as 2M"
        }));
    }

    // The bandwidth limiter stores the limit as a u32.
    qm.set_speed_limit(amount.min(u32::MAX as f64) as u64);
    Json(serde_json::json!({ "status": true }))
}

/// Parse a SABnzbd `from_units` value: a number with an optional
/// K/M/G/T suffix (powers of 1024).
fn parse_sab_units(value: &str) -> Option<f64> {
    let value = value.trim();
    let (number, multiplier) = match value.chars().last()?.to_ascii_uppercase() {
        'K' => (&value[..value.len() - 1], 1024.0),
        'M' => (&value[..value.len() - 1], 1024.0 * 1024.0),
        'G' => (&value[..value.len() - 1], 1024.0 * 1024.0 * 1024.0),
        'T' => (&value[..value.len() - 1], 1024.0 * 1024.0 * 1024.0 * 1024.0),
        _ => (value, 1.0),
    };
    number
        .trim()
        .parse::<f64>()
        .ok()
        .filter(|amount| amount.is_finite() && *amount >= 0.0)
        .map(|amount| amount * multiplier)
}

/// Map SABnzbd's `pp` add parameter onto RustNZB's post-processing level.
/// SABnzbd's scale is cumulative (0=download only, 1=+repair,
/// 2=+repair/unpack, 3=+repair/unpack/delete); RustNZB's is 0=none,
/// 1=repair, 2=unpack only, 3=repair+unpack, with source cleanup governed
/// separately. So SABnzbd 2 and 3 both map to 3. Anything else (absent,
/// `-1`/default, out of range) leaves the category's setting in force.
fn sab_pp_override(pp: Option<&str>) -> Option<u8> {
    match pp?.trim().parse::<i32>().ok()? {
        0 => Some(0),
        1 => Some(1),
        2 | 3 => Some(3),
        _ => None,
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
    format!("SABnzbd_nzo_{}", sab_id_prefix(&job.id))
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
        format!("SABnzbd_nzo_{}", sab_id_prefix(id))
    }
}

/// The first 12 bytes of an id, without splitting a multibyte character.
fn sab_id_prefix(id: &str) -> &str {
    let mut end = 12.min(id.len());
    while !id.is_char_boundary(end) {
        end -= 1;
    }
    &id[..end]
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
    use crate::nzb_core::config::{AppConfig, CategoryConfig};
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
            pp_override: None,
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
            pp_override: None,
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

    /// The queue must report the live speed limit and the seconds left of
    /// a timed pause, as fullstatus already does.
    #[tokio::test]
    async fn queue_reports_speed_limit_and_timed_pause_remaining() {
        let test_state = test_state();
        let qm = &test_state.state.queue_manager;
        qm.set_speed_limit(2 * 1024 * 1024);
        qm.pause_for(600);

        let queue = dispatch_mode(&test_state.state, "queue", &SabApiRequest::default()).0;
        let fullstatus =
            dispatch_mode(&test_state.state, "fullstatus", &SabApiRequest::default()).0;
        assert_eq!(queue["queue"]["speedlimit_abs"], "2097152");
        assert_eq!(
            queue["queue"]["speedlimit"],
            fullstatus["status"]["speedlimit"]
        );
        assert_ne!(queue["queue"]["speedlimit"], "0");
        let pause_int: i64 = queue["queue"]["pause_int"]
            .as_str()
            .expect("pause_int is a string")
            .parse()
            .expect("pause_int is numeric");
        assert!((590..=600).contains(&pause_int), "pause_int={pause_int}");

        qm.resume_all();
        qm.set_speed_limit(0);
        let queue = dispatch_mode(&test_state.state, "queue", &SabApiRequest::default()).0;
        assert_eq!(queue["queue"]["pause_int"], "0");
        assert_eq!(queue["queue"]["speedlimit"], "0");
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
            pp_override: None,
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
            pp_override: None,
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
            pp_override: None,
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

    async fn addfile_with_category(test_state: &TestState, cat: &str) -> String {
        let response = dispatch_post(
            &test_state.state,
            "addfile".into(),
            None,
            Some(cat.into()),
            None,
            Some((format!("{cat}.nzb"), SAMPLE_NZB.as_bytes().to_vec())),
            None,
            None,
            SabApiRequest::default(),
        )
        .await
        .expect("addfile response")
        .0;
        assert_eq!(
            response["status"],
            serde_json::json!(true),
            "resp={response}"
        );
        let nzo_id = response["nzo_ids"][0].as_str().expect("nzo_id").to_string();
        let raw = nzo_id.strip_prefix("SABnzbd_nzo_").expect("nzo prefix");
        test_state
            .state
            .queue_manager
            .get_jobs()
            .into_iter()
            .find(|job| job.id.starts_with(raw))
            .expect("added job")
            .category
    }

    fn gzip(data: &[u8]) -> Vec<u8> {
        use std::io::Write as _;
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(data).unwrap();
        encoder.finish().unwrap()
    }

    fn bzip2(data: &[u8]) -> Vec<u8> {
        use std::io::Write as _;
        let mut encoder = bzip2::write::BzEncoder::new(Vec::new(), bzip2::Compression::default());
        encoder.write_all(data).unwrap();
        encoder.finish().unwrap()
    }

    fn zip_of(entries: &[(&str, &[u8])]) -> Vec<u8> {
        use std::io::Write as _;
        let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        for (name, data) in entries {
            writer
                .start_file(*name, zip::write::SimpleFileOptions::default())
                .unwrap();
            writer.write_all(data).unwrap();
        }
        writer.finish().unwrap().into_inner()
    }

    async fn addfile_upload(
        test_state: &TestState,
        file_name: &str,
        data: Vec<u8>,
    ) -> serde_json::Value {
        dispatch_post(
            &test_state.state,
            "addfile".into(),
            None,
            None,
            None,
            Some((file_name.into(), data)),
            None,
            None,
            SabApiRequest::default(),
        )
        .await
        .expect("addfile response")
        .0
    }

    fn queued_names(test_state: &TestState) -> Vec<String> {
        let mut names: Vec<String> = test_state
            .state
            .queue_manager
            .get_jobs()
            .into_iter()
            .map(|job| job.name)
            .collect();
        names.sort();
        names
    }

    /// `addfile` accepts the same compressed uploads as the native add
    /// endpoint: `.nzb.gz`, `.nzb.bz2` and `.zip` (one job per NZB inside).
    #[tokio::test]
    async fn addfile_accepts_compressed_nzbs() {
        let test_state = test_state();
        let gz = addfile_upload(&test_state, "Gz.Show.nzb.gz", gzip(SAMPLE_NZB.as_bytes())).await;
        assert_eq!(gz["status"], serde_json::json!(true), "gz: {gz}");
        let bz = addfile_upload(&test_state, "Bz.Show.nzb.bz2", bzip2(SAMPLE_NZB.as_bytes())).await;
        assert_eq!(bz["status"], serde_json::json!(true), "bz2: {bz}");
        assert_eq!(queued_names(&test_state), vec!["Bz.Show", "Gz.Show"]);

        let test_state = self::test_state();
        let archive = zip_of(&[
            ("First.nzb", SAMPLE_NZB.as_bytes()),
            ("folder/Second.nzb", SAMPLE_NZB.as_bytes()),
            ("readme.txt", b"not an nzb"),
        ]);
        let zip = addfile_upload(&test_state, "pack.zip", archive).await;
        assert_eq!(zip["status"], serde_json::json!(true), "zip: {zip}");
        assert_eq!(zip["nzo_ids"].as_array().unwrap().len(), 2);
        assert_eq!(queued_names(&test_state), vec!["First", "Second"]);

        let broken =
            addfile_upload(&self::test_state(), "broken.nzb.gz", b"not gzip".to_vec()).await;
        assert_eq!(broken["status"], serde_json::json!(false));
        assert!(broken["error"].is_string());
    }

    /// Serves `body` once on a loopback port at `path`.
    async fn spawn_bytes_server(body: Vec<u8>, path: &str) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind ephemeral test server");
        let addr = listener.local_addr().expect("test server local addr");
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept test connection");
            let mut buf = [0u8; 1024];
            let _ = socket.read(&mut buf).await;
            let mut response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .into_bytes();
            response.extend_from_slice(&body);
            let _ = socket.write_all(&response).await;
            let _ = socket.shutdown().await;
        });
        format!("http://{addr}{path}")
    }

    /// `addurl` unpacks a fetched `.nzb.gz` the same way.
    ///
    /// Adapted: loopback is admitted through the test state's
    /// `fetch_allowed_hosts` rather than the upstream cfg(test) task-local
    /// seam, which this tree does not have.
    #[tokio::test]
    async fn addurl_accepts_gzipped_nzb() {
        let test_state = test_state();
        let mut config = (*test_state.state.config()).clone();
        config.general.fetch_allowed_hosts = vec!["127.0.0.1".into()];
        test_state.state.config.store(Arc::new(config));

        let url = spawn_bytes_server(gzip(SAMPLE_NZB.as_bytes()), "/Url.Show.nzb.gz").await;
        let response = dispatch_post(
            &test_state.state,
            "addurl".into(),
            None,
            None,
            None,
            None,
            Some(url),
            None,
            SabApiRequest::default(),
        )
        .await
        .expect("addurl response")
        .0;
        assert_eq!(
            response["status"],
            serde_json::json!(true),
            "resp={response}"
        );
        assert_eq!(queued_names(&test_state), vec!["Url.Show"]);
    }

    /// SABnzbd falls back to the default category when `addfile`/`addurl`
    /// name a category that is not configured, and matches configured
    /// category names case-insensitively.
    #[tokio::test]
    async fn add_with_unknown_category_falls_back_to_default() {
        let test_state = test_state();
        let mut config = (*test_state.state.config()).clone();
        config.categories.push(CategoryConfig {
            name: "tv".into(),
            ..CategoryConfig::default()
        });
        test_state.state.config.store(Arc::new(config));

        assert_eq!(
            addfile_with_category(&test_state, "no-such-cat").await,
            "Default"
        );
        assert_eq!(addfile_with_category(&test_state, "TV").await, "tv");
        assert_eq!(addfile_with_category(&test_state, "*").await, "Default");
    }

    fn config_request(name: &str, value: &str) -> SabApiRequest {
        SabApiRequest {
            mode: Some("config".into()),
            name: Some(name.into()),
            value: Some(value.into()),
            ..SabApiRequest::default()
        }
    }

    /// `mode=config&name=speedlimit` sets the live limit, accepting an
    /// absolute value with an optional K/M/G suffix as SABnzbd does. Values
    /// of 1-100 (or with `%`) are percentages of the maximum bandwidth.
    #[tokio::test]
    async fn config_speedlimit_sets_the_live_limit() {
        let test_state = test_state();
        let qm = &test_state.state.queue_manager;

        for (value, expected) in [
            ("512K", 512 * 1024),
            ("2M", 2 * 1024 * 1024),
            ("1.5M", 1024 * 1024 * 3 / 2),
            ("1048576", 1_048_576),
            ("0", 0),
            ("3M", 3 * 1024 * 1024),
            ("100", 0),
        ] {
            let response = dispatch_mode(
                &test_state.state,
                "config",
                &config_request("speedlimit", value),
            )
            .0;
            assert_eq!(
                response,
                serde_json::json!({ "status": true }),
                "value={value}"
            );
            assert_eq!(qm.get_speed_limit(), expected, "value={value}");
        }

        // RustNZB has no maximum-bandwidth setting, so a partial percentage
        // cannot be applied; report that instead of silently ignoring it.
        qm.set_speed_limit(4096);
        let response = dispatch_mode(
            &test_state.state,
            "config",
            &config_request("speedlimit", "50%"),
        )
        .0;
        assert_eq!(response["status"], serde_json::json!(false));
        assert!(response["error"].is_string());
        assert_eq!(qm.get_speed_limit(), 4096);
    }

    /// `mode=config&name=set_pause&value=<minutes>` is SABnzbd's timed
    /// pause; `value=0` cancels it and resumes.
    #[tokio::test]
    async fn config_set_pause_starts_and_cancels_a_timed_pause() {
        let test_state = test_state();
        let qm = &test_state.state.queue_manager;

        let response = dispatch_mode(
            &test_state.state,
            "config",
            &config_request("set_pause", "5"),
        )
        .0;
        assert_eq!(response, serde_json::json!({ "status": true }));
        assert!(qm.is_paused());
        let remaining = qm.pause_remaining_secs().expect("timed pause");
        assert!((290..=300).contains(&remaining), "remaining={remaining}");

        let response = dispatch_mode(
            &test_state.state,
            "config",
            &config_request("set_pause", "0"),
        )
        .0;
        assert_eq!(response, serde_json::json!({ "status": true }));
        assert!(!qm.is_paused());
        assert_eq!(qm.pause_remaining_secs(), None);
    }

    /// `mode=config` without a setter keeps returning the configuration;
    /// unknown setters are reported as not implemented, not ignored.
    #[tokio::test]
    async fn config_without_setter_still_returns_configuration() {
        let test_state = test_state();
        let config = dispatch_mode(&test_state.state, "config", &SabApiRequest::default()).0;
        assert!(config["config"]["categories"].is_array());

        let unknown = dispatch_mode(
            &test_state.state,
            "config",
            &config_request("no_such_setter", "1"),
        )
        .0;
        assert_eq!(unknown["status"], serde_json::json!(false));
    }

    /// SABnzbd's `mode=pause` pauses the whole queue and cancels any
    /// scheduled resume (`_api_pause` calls `plan_resume(0)`); a `value`
    /// that is not a job id must not turn it into a no-op.
    #[tokio::test]
    async fn pause_with_non_job_value_pauses_all_and_cancels_timed_resume() {
        let test_state = test_state();
        let qm = &test_state.state.queue_manager;
        qm.pause_for(600);
        qm.resume_all();
        assert!(!qm.is_paused());

        let req = SabApiRequest {
            value: Some("30".into()),
            ..SabApiRequest::default()
        };
        let response = dispatch_mode(&test_state.state, "pause", &req).0;
        assert_eq!(response["status"], serde_json::json!(true));
        assert!(qm.is_paused());

        qm.pause_for(600);
        let response = dispatch_mode(&test_state.state, "pause", &SabApiRequest::default()).0;
        assert_eq!(response["status"], serde_json::json!(true));
        assert_eq!(qm.pause_remaining_secs(), None);
        assert!(qm.is_paused());
    }

    fn assert_real_gigabytes(value: &serde_json::Value, field: &str) {
        let gigabytes: f64 = value[field]
            .as_str()
            .unwrap_or_else(|| panic!("{field} is a string"))
            .parse()
            .unwrap_or_else(|_| panic!("{field} is numeric"));
        assert!(gigabytes > 0.0, "{field} should report real disk space");
    }

    /// queue and fullstatus must report real free/total space for the
    /// incomplete (1) and complete (2) directories, not hardcoded zeros.
    #[tokio::test]
    async fn queue_and_fullstatus_report_real_disk_space() {
        let test_state = test_state();
        let queue = dispatch_mode(&test_state.state, "queue", &SabApiRequest::default()).0;
        let fullstatus =
            dispatch_mode(&test_state.state, "fullstatus", &SabApiRequest::default()).0;
        for section in [&queue["queue"], &fullstatus["status"]] {
            for field in [
                "diskspace1",
                "diskspace2",
                "diskspacetotal1",
                "diskspacetotal2",
            ] {
                assert_real_gigabytes(section, field);
            }
            assert_ne!(section["diskspace1_norm"], "0 B");
            assert_ne!(section["diskspace2_norm"], "0 B");
        }
    }

    /// fullstatus reports uptime since start in SABnzbd's `calc_age` form
    /// and lists the configured servers.
    #[tokio::test]
    async fn fullstatus_reports_uptime_and_configured_servers() {
        let mut test_state = test_state();
        test_state.state.started_at -= std::time::Duration::from_secs(2 * 3600 + 120);
        let mut config = (*test_state.state.config()).clone();
        let mut server = crate::nzb_core::config::ServerConfig::new("srv-1", "news.example.com");
        server.name = "Primary".into();
        server.connections = 12;
        config.servers = vec![server];
        test_state.state.config.store(Arc::new(config));

        let status = dispatch_mode(&test_state.state, "fullstatus", &SabApiRequest::default()).0;
        let status = &status["status"];
        assert_eq!(status["uptime"], "2h");
        let servers = status["servers"].as_array().expect("servers array");
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0]["servername"], "Primary");
        assert_eq!(servers[0]["servertotalconn"], 12);
        assert_eq!(servers[0]["serveractive"], true);
    }

    #[test]
    fn sab_age_matches_sabnzbd_calc_age() {
        assert_eq!(format_sab_age(0), "0m");
        assert_eq!(format_sab_age(3599), "59m");
        assert_eq!(format_sab_age(3600), "1h");
        assert_eq!(format_sab_age(2 * 86_400 + 5), "2d");
    }

    /// Run a multipart `mode=addfile` with `query` as the query string and
    /// `fields` as extra text fields, returning the queued job.
    async fn addfile_multipart(query: SabApiRequest, fields: &[(&str, &str)]) -> NzbJob {
        let TestState { state, _tempdir } = test_state();
        let state = Arc::new(state);
        let boundary = "sabboundary";
        let mut body = format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"mode\"\r\n\r\naddfile\r\n"
        );
        for (name, value) in fields {
            body.push_str(&format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n"
            ));
        }
        body.push_str(&format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"name\"; filename=\"upload.nzb\"\r\nContent-Type: application/x-nzb\r\n\r\n{SAMPLE_NZB}\r\n--{boundary}--\r\n"
        ));
        let query = SabApiRequest {
            apikey: Some("contract-api-key".into()),
            ..query
        };
        let request = Request::builder()
            .method("POST")
            .uri("/sabnzbd/api")
            .header(
                CONTENT_TYPE,
                format!("multipart/form-data; boundary={boundary}"),
            )
            .body(axum::body::Body::from(body))
            .expect("build request");
        let response = h_sabnzbd_api_post(State(state.clone()), Query(query), request)
            .await
            .expect("addfile over multipart")
            .into_response();
        let value = json_body(response).await;
        assert_eq!(value["status"], serde_json::json!(true), "resp={value}");
        let mut jobs = state.queue_manager.get_jobs();
        assert_eq!(jobs.len(), 1);
        jobs.remove(0)
    }

    /// SABnzbd's `pp` (0-3) overrides the category's post-processing for
    /// the added job; `script` is accepted and ignored.
    #[tokio::test]
    async fn addfile_applies_pp_override_and_ignores_script() {
        let from_query = addfile_multipart(
            SabApiRequest {
                pp: Some("1".into()),
                ..SabApiRequest::default()
            },
            &[("script", "Notify.py")],
        )
        .await;
        assert_eq!(from_query.pp_override, Some(1));

        let from_field = addfile_multipart(SabApiRequest::default(), &[("pp", "0")]).await;
        assert_eq!(from_field.pp_override, Some(0));

        // SABnzbd's cumulative 2 (+repair/unpack) is RustNZB's 3.
        let repair_unpack = addfile_multipart(SabApiRequest::default(), &[("pp", "2")]).await;
        assert_eq!(repair_unpack.pp_override, Some(3));

        let invalid = addfile_multipart(SabApiRequest::default(), &[("pp", "7")]).await;
        assert_eq!(invalid.pp_override, None);

        let absent = addfile_multipart(SabApiRequest::default(), &[]).await;
        assert_eq!(absent.pp_override, None);
    }

    /// `mode=server_stats` has its own shape in SABnzbd
    /// (`api.py::_api_server_stats`): byte totals plus a per-server map, not
    /// the fullstatus envelope.
    #[tokio::test]
    async fn server_stats_uses_sabnzbd_shape() {
        let test_state = test_state();
        let mut config = (*test_state.state.config()).clone();
        let mut server = crate::nzb_core::config::ServerConfig::new("srv-1", "news.example.com");
        server.name = "Primary".into();
        config.servers = vec![server];
        test_state.state.config.store(Arc::new(config));

        let mut entry = history_entry("stats-job", "Stats Job", "tv", JobStatus::Completed, 60);
        entry.server_stats = vec![ServerArticleStats {
            server_id: "srv-1".into(),
            server_name: "Primary".into(),
            articles_downloaded: 9,
            articles_failed: 1,
            bytes_downloaded: 4096,
        }];
        test_state
            .state
            .queue_manager
            .with_db(|database| database.history_insert(&entry).expect("insert history"));

        let stats = dispatch_mode(&test_state.state, "server_stats", &SabApiRequest::default()).0;
        assert!(
            stats.get("status").is_none(),
            "not the fullstatus envelope: {stats}"
        );
        for field in ["total", "month", "week", "day"] {
            assert_eq!(stats[field], 4096, "{field}");
        }
        let primary = &stats["servers"]["Primary"];
        for field in ["total", "month", "week", "day"] {
            assert_eq!(primary[field], 4096, "Primary.{field}");
        }
        assert!(primary["daily"].is_object());
        assert_eq!(primary["articles_tried"], 10);
        assert_eq!(primary["articles_success"], 9);
    }

    /// `mode=warnings` is a real SABnzbd mode (`api.py::_api_warnings`);
    /// it must answer with a `warnings` list, consistent with fullstatus,
    /// and `name=clear` must succeed.
    #[tokio::test]
    async fn warnings_mode_matches_fullstatus_warnings() {
        let test_state = test_state();
        let warnings = dispatch_mode(&test_state.state, "warnings", &SabApiRequest::default()).0;
        let fullstatus =
            dispatch_mode(&test_state.state, "fullstatus", &SabApiRequest::default()).0;
        assert!(warnings["warnings"].is_array(), "resp={warnings}");
        assert_eq!(warnings["warnings"], fullstatus["status"]["warnings"]);

        let clear = SabApiRequest {
            name: Some("clear".into()),
            ..SabApiRequest::default()
        };
        let cleared = dispatch_mode(&test_state.state, "warnings", &clear).0;
        assert_eq!(cleared, serde_json::json!({ "status": true }));
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
            .join("tv")
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

    /// A failed job keeps its partial download in `incomplete/` for retry.
    /// Deleting its history entry, with or without `del_files`, must free it.
    #[tokio::test]
    async fn history_delete_removes_retained_incomplete_work_dir() {
        let test_state = test_state();
        let incomplete = test_state.state.queue_manager.incomplete_dir();
        for (id, del_files) in [("sab-failed-keep", None), ("sab-failed-del", Some("1"))] {
            insert_history_status(&test_state, id, JobStatus::Failed, 10);
            let work_dir = incomplete.join(id);
            std::fs::create_dir_all(&work_dir).expect("create retained work dir");
            std::fs::write(work_dir.join("partial.rar"), b"partial").expect("write partial");

            let req = SabApiRequest {
                value: Some(id.into()),
                del_files: del_files.map(Into::into),
                ..SabApiRequest::default()
            };
            let response = handle_history_delete(&test_state.state, &req).0;
            assert_eq!(response["status"], serde_json::json!(true));
            assert!(
                !work_dir.exists(),
                "{id}: retained work dir must be removed"
            );
        }
        assert!(incomplete.is_dir(), "the incomplete root itself is kept");
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

    /// A Failed entry and a Completed entry (a retry of the same release) share
    /// one output directory. Clearing failed history with `del_files=1` must
    /// not remove the completed payload.
    #[tokio::test]
    async fn history_delete_failed_keeps_directory_shared_with_completed_entry() {
        let test_state = test_state();
        let shared = test_state
            .state
            .config()
            .general
            .complete_dir
            .join("tv")
            .join("same-release");
        std::fs::create_dir_all(&shared).expect("create shared output dir");
        std::fs::write(shared.join("file.mkv"), b"payload").expect("write payload");

        let mut failed = history_entry("job-failed", "same-release", "tv", JobStatus::Failed, 2);
        failed.output_dir = shared.clone();
        let mut completed =
            history_entry("job-done", "same-release", "tv", JobStatus::Completed, 1);
        completed.output_dir = shared.clone();
        test_state.state.queue_manager.with_db(|database| {
            database.history_insert(&failed).expect("insert failed");
            database
                .history_insert(&completed)
                .expect("insert completed");
        });

        let response = handle_history_delete(
            &test_state.state,
            &SabApiRequest {
                value: Some("failed".into()),
                del_files: Some("1".into()),
                ..SabApiRequest::default()
            },
        )
        .0;
        assert_eq!(response["status"], serde_json::json!(true));
        assert!(
            shared.join("file.mkv").is_file(),
            "completed payload survives"
        );
        assert_eq!(history_ids(&test_state), vec!["job-done".to_string()]);
    }

    /// `mode=config&name=set_pause` with a value past chrono's range must clamp
    /// instead of panicking, and still pause.
    #[tokio::test]
    async fn config_set_pause_clamps_huge_duration() {
        let test_state = test_state();
        let response = dispatch_mode(
            &test_state.state,
            "config",
            &config_request("set_pause", &u64::MAX.to_string()),
        )
        .0;
        assert_eq!(response, serde_json::json!({ "status": true }));
        let qm = &test_state.state.queue_manager;
        assert!(qm.is_paused());
        let remaining = qm.pause_remaining_secs().expect("timed pause");
        assert!(
            (364 * 86_400..365 * 86_400).contains(&remaining),
            "remaining={remaining}"
        );
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
        spawn_nzb_server_with_headers(body, "").await
    }

    /// Like [`spawn_nzb_server`], adding `extra_headers` (each line ending
    /// in `\r\n`) to the response.
    async fn spawn_nzb_server_with_headers(
        body: &'static str,
        extra_headers: &'static str,
    ) -> String {
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
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: application/x-nzb\r\n{extra_headers}Connection: close\r\n\r\n{}",
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

    /// Run a GET `mode=addurl` against a loopback fixture (admitted by the
    /// test state's explicit `fetch_allowed_hosts` entry) and return the
    /// response and the names of the queued jobs.
    async fn addurl_over_get(
        url: String,
        nzbname: Option<&str>,
    ) -> (serde_json::Value, Vec<String>) {
        let TestState { state, _tempdir } = test_state();
        // Loopback is not a private LAN address; admit the fixture only here,
        // through an explicit entry, instead of relaxing the guard globally.
        {
            let mut config = (*state.config()).clone();
            config.general.fetch_allowed_hosts = vec!["127.0.0.1".into()];
            state.config.store(std::sync::Arc::new(config));
        }
        let state = Arc::new(state);
        let req = SabApiRequest {
            mode: Some("addurl".into()),
            name: Some(url),
            nzbname: nzbname.map(str::to_string),
            apikey: Some("contract-api-key".into()),
            ..SabApiRequest::default()
        };
        let response = h_sabnzbd_api_get(State(state.clone()), Query(req))
            .await
            .expect("addurl over GET")
            .into_response();
        let value = json_body(response).await;
        let names = state
            .queue_manager
            .get_jobs()
            .into_iter()
            .map(|job| job.name)
            .collect();
        (value, names)
    }

    /// `mode=addurl&name=<URL>` used the whole URL as the job name, which
    /// `output_dir_for` rejects, so every successful fetch failed to
    /// enqueue. The name comes from the URL's last path segment instead,
    /// without `.nzb` or the query string.
    #[tokio::test]
    async fn addurl_names_job_from_url_path_not_the_url() {
        let url = spawn_nzb_server(SAMPLE_NZB).await;
        let (response, names) = addurl_over_get(format!("{url}?apikey=x&t=get"), None).await;
        assert_eq!(
            response["status"],
            serde_json::json!(true),
            "resp={response}"
        );
        assert_eq!(names, vec!["test".to_string()]);
    }

    #[tokio::test]
    async fn addurl_honours_nzbname() {
        let url = spawn_nzb_server(SAMPLE_NZB).await;
        let (response, names) = addurl_over_get(url, Some("My.Show.S01E01")).await;
        assert_eq!(
            response["status"],
            serde_json::json!(true),
            "resp={response}"
        );
        assert_eq!(names, vec!["My.Show.S01E01".to_string()]);
    }

    #[tokio::test]
    async fn addurl_uses_content_disposition_filename() {
        let url = spawn_nzb_server_with_headers(
            SAMPLE_NZB,
            "Content-Disposition: attachment; filename=\"Some.Release.nzb\"\r\n",
        )
        .await;
        let (response, names) = addurl_over_get(url, None).await;
        assert_eq!(
            response["status"],
            serde_json::json!(true),
            "resp={response}"
        );
        assert_eq!(names, vec!["Some.Release".to_string()]);
    }

    #[test]
    fn addurl_job_name_sources_in_sabnzbd_order() {
        let url = reqwest::Url::parse("https://indexer.example/get/My%20File.nzb?id=1").unwrap();
        assert_eq!(addurl_job_name(None, None, &url), "My File");
        assert_eq!(
            addurl_job_name(
                None,
                Some("attachment; filename*=UTF-8''Caf%C3%A9.nzb"),
                &url
            ),
            "Café"
        );
        assert_eq!(
            addurl_job_name(Some("Chosen.nzb"), Some("attachment; filename=x.nzb"), &url),
            "Chosen"
        );
        let bare = reqwest::Url::parse("https://indexer.example/").unwrap();
        assert!(!addurl_job_name(None, None, &bare).contains('/'));
    }

    /// `nzbname` also overrides the job name for uploads, from the query
    /// string, a urlencoded body or a multipart field.
    #[tokio::test]
    async fn addfile_honours_nzbname_from_query_and_multipart() {
        for (query_name, field_name, expected) in [
            (Some("From.Query"), None, "From.Query"),
            (None, Some("From.Field"), "From.Field"),
        ] {
            let TestState { state, _tempdir } = test_state();
            let state = Arc::new(state);
            let boundary = "sabboundary";
            let mut body = String::new();
            body.push_str(&format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"mode\"\r\n\r\naddfile\r\n"
            ));
            if let Some(field) = field_name {
                body.push_str(&format!(
                    "--{boundary}\r\nContent-Disposition: form-data; name=\"nzbname\"\r\n\r\n{field}\r\n"
                ));
            }
            body.push_str(&format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"name\"; filename=\"upload.nzb\"\r\nContent-Type: application/x-nzb\r\n\r\n{SAMPLE_NZB}\r\n--{boundary}--\r\n"
            ));
            let req = SabApiRequest {
                apikey: Some("contract-api-key".into()),
                nzbname: query_name.map(str::to_string),
                ..SabApiRequest::default()
            };
            let request = Request::builder()
                .method("POST")
                .uri("/sabnzbd/api")
                .header(
                    CONTENT_TYPE,
                    format!("multipart/form-data; boundary={boundary}"),
                )
                .body(axum::body::Body::from(body))
                .expect("build request");
            let response = h_sabnzbd_api_post(State(state.clone()), Query(req), request)
                .await
                .expect("addfile over multipart")
                .into_response();
            let value = json_body(response).await;
            assert_eq!(value["status"], serde_json::json!(true), "resp={value}");
            let names: Vec<String> = state
                .queue_manager
                .get_jobs()
                .into_iter()
                .map(|job| job.name)
                .collect();
            assert_eq!(names, vec![expected.to_string()]);
        }
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
