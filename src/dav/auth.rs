//! Auth middleware for the `/dav` mount.
//!
//! Reads `DavConfig.username/password/api_key` live from `AppState` on each
//! request. Accepts:
//!
//! - `X-Api-Key: <key>` header matching `dav.api_key`, OR
//! - HTTP Basic auth matching `dav.username` + `dav.password`.
//!
//! When **all three** fields are unset, requests pass through unauthenticated
//! (a warning is logged at startup so the operator notices).

use std::sync::Arc;

use axum::{
    body::Body,
    http::{HeaderMap, Response, StatusCode, header},
    middleware::Next,
};
use base64::Engine;
use nzb_web::AppState;
use nzb_web::auth::constant_time_eq;

pub async fn dav_auth(
    state: Arc<AppState>,
    headers: HeaderMap,
    request: axum::extract::Request,
    next: Next,
) -> Response<Body> {
    let cfg = state.config();
    let dav = &cfg.dav;

    // Open access when no credentials are set anywhere.
    let configured = dav.api_key.is_some() || dav.username.is_some() || dav.password.is_some();
    if !configured {
        return next.run(request).await;
    }

    // X-Api-Key
    if let Some(expected) = dav.api_key.as_deref()
        && let Some(provided) = headers.get("X-Api-Key").and_then(|h| h.to_str().ok())
        && constant_time_eq(provided.as_bytes(), expected.as_bytes())
    {
        return next.run(request).await;
    }

    // Basic auth
    if let (Some(user), Some(pass)) = (dav.username.as_deref(), dav.password.as_deref())
        && let Some(provided) = headers
            .get(header::AUTHORIZATION)
            .and_then(|h| h.to_str().ok())
            .and_then(|h| h.strip_prefix("Basic "))
            .and_then(|v| base64::engine::general_purpose::STANDARD.decode(v).ok())
            .and_then(|v| String::from_utf8(v).ok())
        && let Some((u, p)) = provided.split_once(':')
        && constant_time_eq(u.as_bytes(), user.as_bytes())
        && constant_time_eq(p.as_bytes(), pass.as_bytes())
    {
        return next.run(request).await;
    }

    // WebDAV clients (Plex, Infuse, davfs2, …) need WWW-Authenticate to know to
    // prompt for credentials. Unlike the SPA on /api, /dav has no JS login flow,
    // so the browser-style popup is exactly what we want here.
    Response::builder()
        .status(StatusCode::UNAUTHORIZED)
        .header(header::WWW_AUTHENTICATE, "Basic realm=\"rustnzb DAV\"")
        .body(Body::empty())
        .unwrap()
}
