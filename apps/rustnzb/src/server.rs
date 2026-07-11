use std::sync::Arc;

use axum::Router;
use axum::extract::DefaultBodyLimit;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use base64::Engine;
use http::{HeaderMap, StatusCode, header};
use rust_embed::Embed;
use tokio::net::TcpListener;
use tower_http::cors::{AllowHeaders, AllowOrigin, CorsLayer};
use tower_http::trace::TraceLayer;
use tracing::info;

use utoipa::OpenApi;
use utoipa_swagger_ui::SwaggerUi;

use crate::group_handlers;
use crate::handlers;
use nzb_web::auth;
use nzb_web::error::ApiError;
use nzb_web::sabnzbd_compat;
use nzb_web::state::AppState;

#[derive(OpenApi)]
#[openapi(info(title = "rustnzb API", version = env!("RUSTNZB_BUILD_VERSION")))]
struct ApiDoc;

/// Embed the Angular SPA build at compile time.
#[derive(Embed)]
#[folder = "$CARGO_MANIFEST_DIR/frontend/dist/frontend/browser"]
struct StaticAssets;

/// Serve the root page (index.html) from embedded static assets.
async fn h_root() -> Response {
    serve_embedded_file("index.html")
}

/// SPA fallback: serve static file if it exists, otherwise index.html.
#[allow(clippy::collapsible_if)]
async fn h_spa_fallback(uri: axum::http::Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    // Try exact file first
    if !path.is_empty() {
        if let Some(content) = StaticAssets::get(path) {
            let mime = mime_guess::from_path(path).first_or_octet_stream();
            return (
                StatusCode::OK,
                [
                    (header::CONTENT_TYPE, mime.as_ref().to_string()),
                    (header::CACHE_CONTROL, cache_header_for(path).to_string()),
                ],
                content.data.into_owned(),
            )
                .into_response();
        }
    }
    // SPA fallback to index.html
    serve_embedded_file("index.html")
}

/// Look up an embedded file and return it with the correct Content-Type.
fn serve_embedded_file(path: &str) -> Response {
    match StaticAssets::get(path) {
        Some(content) => {
            let mime = mime_guess::from_path(path).first_or_octet_stream();
            (
                StatusCode::OK,
                [
                    (header::CONTENT_TYPE, mime.as_ref().to_string()),
                    (header::CACHE_CONTROL, cache_header_for(path).to_string()),
                ],
                content.data.into_owned(),
            )
                .into_response()
        }
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

/// Returns the appropriate Cache-Control header value for a static asset path.
///
/// - index.html: no-cache so browsers always revalidate; stale index.html after
///   an upgrade would reference old hashed chunk filenames that no longer exist.
/// - Angular-hashed assets (e.g. main-7QCIQPRR.js): immutable — the hash in the
///   filename guarantees content identity, so these are safe to cache forever.
/// - Everything else (favicons, logos): 1-hour TTL as a reasonable middle ground.
fn cache_header_for(path: &str) -> &'static str {
    if path == "index.html" {
        "no-cache, must-revalidate"
    } else if is_hashed_asset(path) {
        "max-age=31536000, immutable"
    } else {
        "max-age=3600"
    }
}

/// Detects Angular content-hashed filenames (e.g. `main-7QCIQPRR.js`).
/// Angular's outputHashing=all appends an 8-character uppercase alphanumeric
/// hash before the extension.
fn is_hashed_asset(path: &str) -> bool {
    let Some(dot_pos) = path.rfind('.') else {
        return false;
    };
    let stem = &path[..dot_pos];
    let Some(dash_pos) = stem.rfind('-') else {
        return false;
    };
    let hash = &stem[dash_pos + 1..];
    hash.len() == 8
        && hash
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
}

/// Build the axum Router with all API routes.
pub fn build_router(state: Arc<AppState>) -> Router {
    let cors = CorsLayer::default()
        .allow_origin(AllowOrigin::any())
        .allow_headers(AllowHeaders::any());

    // Auth endpoints (no auth middleware on these)
    let auth_routes = Router::new()
        .route("/auth/status", get(auth::h_auth_status))
        .route("/auth/login", post(auth::h_auth_login))
        .route("/auth/refresh", post(auth::h_auth_refresh))
        .route("/auth/logout", post(auth::h_auth_logout))
        .route("/auth/setup", post(auth::h_auth_setup))
        .route(
            "/auth/change_credentials",
            post(auth::h_auth_change_credentials),
        );

    // Native REST API (protected by auth middleware)
    let api_routes = Router::new()
        // Health check (Docker HEALTHCHECK) — no auth needed, added separately below
        // Status
        .route("/status", get(handlers::h_status))
        // Setup / Import
        .route("/setup/status", get(handlers::h_setup_status))
        .route(
            "/setup/import-sabnzbd",
            post(handlers::h_import_sabnzbd_ini),
        )
        .route(
            "/setup/import-sabnzbd-api",
            post(handlers::h_import_sabnzbd_api),
        )
        .route("/setup/apply", post(handlers::h_setup_apply))
        // Logs
        .route("/logs", get(handlers::h_logs))
        // Queue
        .route("/queue", get(handlers::h_queue_list))
        .route("/queue/add", post(handlers::h_queue_add))
        .route("/queue/add-url", post(handlers::h_queue_add_url))
        .route("/queue/pause", post(handlers::h_queue_pause_all))
        .route("/queue/resume", post(handlers::h_queue_resume_all))
        .route("/queue/pause-for", post(handlers::h_queue_pause_for))
        .route("/queue/{id}/pause", post(handlers::h_queue_pause))
        .route("/queue/{id}/resume", post(handlers::h_queue_resume))
        .route("/queue/{id}/move", post(handlers::h_queue_move))
        .route("/queue/{id}/priority", put(handlers::h_queue_set_priority))
        .route(
            "/queue/{id}/category",
            put(handlers::h_queue_change_category),
        )
        .route("/queue/bulk", post(handlers::h_queue_bulk_action))
        .route("/queue/{id}", delete(handlers::h_queue_delete))
        // History
        .route("/history", get(handlers::h_history_list))
        .route(
            "/history/{id}",
            get(handlers::h_history_get).delete(handlers::h_history_delete),
        )
        .route("/history/{id}/retry", post(handlers::h_history_retry))
        .route("/history/{id}/logs", get(handlers::h_history_logs))
        .route("/history", delete(handlers::h_history_clear))
        .route("/statistics", get(handlers::h_global_statistics))
        // Config
        .route("/config", get(handlers::h_config_get))
        .route("/config/general", put(handlers::h_general_update))
        .route("/config/servers/health", get(handlers::h_servers_health))
        .route("/config/servers/stats", get(handlers::h_server_stats))
        .route("/config/servers", get(handlers::h_servers_list))
        .route("/config/servers", post(handlers::h_server_add))
        .route("/config/servers/{id}", put(handlers::h_server_update))
        .route("/config/servers/{id}", delete(handlers::h_server_delete))
        .route(
            "/config/servers/test-config",
            post(handlers::h_server_test_inline),
        )
        .route("/config/servers/{id}/test", post(handlers::h_server_test))
        .route("/config/categories", get(handlers::h_categories_list))
        .route("/config/categories", post(handlers::h_category_add))
        .route(
            "/config/categories/{name}",
            put(handlers::h_category_update),
        )
        .route(
            "/config/categories/{name}",
            delete(handlers::h_category_delete),
        )
        .route("/config/rss-feeds", get(handlers::h_rss_feeds_list))
        .route("/config/rss-feeds", post(handlers::h_rss_feed_add))
        .route("/config/rss-feeds/{name}", put(handlers::h_rss_feed_update))
        .route(
            "/config/rss-feeds/{name}",
            delete(handlers::h_rss_feed_delete),
        )
        // RSS items and rules
        .route("/rss/items", get(handlers::h_rss_items_list))
        .route(
            "/rss/items/{id}/download",
            post(handlers::h_rss_item_download),
        )
        .route("/rss/rules", get(handlers::h_rss_rules_list))
        .route("/rss/rules", post(handlers::h_rss_rule_add))
        .route("/rss/rules/{id}", put(handlers::h_rss_rule_update))
        .route("/rss/rules/{id}", delete(handlers::h_rss_rule_delete))
        .route(
            "/config/history-retention",
            get(handlers::h_history_retention_get),
        )
        .route(
            "/config/history-retention",
            put(handlers::h_history_retention_set),
        )
        .route(
            "/config/max-active-downloads",
            get(handlers::h_max_active_downloads_get),
        )
        .route(
            "/config/max-active-downloads",
            put(handlers::h_max_active_downloads_set),
        )
        .route("/config/speed-limit", get(handlers::h_get_speed_limit))
        .route("/config/speed-limit", put(handlers::h_set_speed_limit))
        .route("/config/disk-guards", get(handlers::h_disk_guards_get))
        .route("/config/disk-guards", put(handlers::h_disk_guards_set))
        .route("/browse-directory", get(handlers::h_browse_directory))
        // Newsgroup browsing
        .route("/groups", get(group_handlers::h_group_list))
        .route("/groups/refresh", post(group_handlers::h_group_refresh))
        .route("/groups/{id}", get(group_handlers::h_group_get))
        .route("/groups/{id}/status", get(group_handlers::h_group_status))
        .route(
            "/groups/{id}/subscribe",
            post(group_handlers::h_group_subscribe),
        )
        .route(
            "/groups/{id}/unsubscribe",
            post(group_handlers::h_group_unsubscribe),
        )
        .route("/groups/{id}/headers", get(group_handlers::h_header_list))
        .route(
            "/groups/{id}/headers/fetch",
            post(group_handlers::h_header_fetch),
        )
        .route(
            "/groups/{id}/headers/mark-read",
            post(group_handlers::h_header_mark_read),
        )
        .route(
            "/groups/{id}/headers/mark-all-read",
            post(group_handlers::h_header_mark_all_read),
        )
        .route(
            "/groups/{id}/headers/download",
            post(group_handlers::h_header_download),
        )
        .route("/groups/{id}/threads", get(group_handlers::h_thread_list))
        .route(
            "/groups/{gid}/threads/{root_msg_id}",
            get(group_handlers::h_thread_get),
        )
        .route("/articles/{message_id}", get(group_handlers::h_article_get));

    // WebDAV media library management endpoints (only compiled when feature is on)
    #[cfg(feature = "webdav")]
    let api_routes = api_routes
        .route("/dav/add", post(handlers::h_dav_add))
        .route("/dav/status", get(handlers::h_dav_status))
        .route(
            "/config/dav",
            get(handlers::h_dav_config_get).put(handlers::h_dav_config_set),
        );

    // Arr-compatible API (Sonarr/Radarr) — uses its own API key auth
    // Sonarr/Radarr hit /api (the standard SABnzbd path), so register both.
    let sabnzbd_route = Router::new()
        .route("/sabnzbd/api", get(sabnzbd_compat::h_sabnzbd_api_get))
        .route("/sabnzbd/api", post(sabnzbd_compat::h_sabnzbd_api_post))
        .route("/api", get(sabnzbd_compat::h_sabnzbd_api_get))
        .route("/api", post(sabnzbd_compat::h_sabnzbd_api_post));

    // Build the auth middleware closure
    let token_store = state.token_store.clone();
    let credential_store = state.credential_store.clone();
    let api_key = state.config().general.api_key.clone();

    let auth_middleware = axum::middleware::from_fn(
        move |headers: HeaderMap, request: axum::extract::Request, next: Next| {
            let token_store = token_store.clone();
            let credential_store = credential_store.clone();
            let api_key = api_key.clone();
            async move {
                // API key (X-Api-Key header) — config-based, bypasses session/credential auth
                if let Some(ref expected) = api_key
                    && let Some(provided) = headers.get("X-Api-Key").and_then(|h| h.to_str().ok())
                {
                    if auth::constant_time_eq(provided.as_bytes(), expected.as_bytes()) {
                        return Ok(next.run(request).await);
                    }
                    return Err(ApiError::unauthorized());
                }

                // If no credentials configured, allow all requests (setup_required state)
                if !credential_store.has_credentials() {
                    return Ok(next.run(request).await);
                }

                // Try Bearer token first
                if let Some(token) = headers
                    .get("Authorization")
                    .and_then(|h| h.to_str().ok())
                    .and_then(|h| h.strip_prefix("Bearer "))
                {
                    if token_store.validate_access_token(token) {
                        return Ok(next.run(request).await);
                    }
                    return Err(ApiError::unauthorized());
                }

                // Try Basic auth
                let user_pass = headers
                    .get("Authorization")
                    .and_then(|h| h.to_str().ok())
                    .and_then(|h| h.strip_prefix("Basic "))
                    .and_then(|v| base64::engine::general_purpose::STANDARD.decode(v).ok())
                    .and_then(|v| String::from_utf8(v).ok());

                let user_pass = match user_pass {
                    Some(up) => up,
                    None => {
                        // Return plain 401 without WWW-Authenticate header.
                        // Sending WWW-Authenticate triggers the browser's native
                        // auth popup, which conflicts with the SPA's own login flow.
                        return Err(ApiError::unauthorized());
                    }
                };

                if let Some((u, p)) = user_pass.split_once(':')
                    && credential_store.validate(u, p)
                {
                    return Ok(next.run(request).await);
                }

                Err(ApiError::unauthorized())
            }
        },
    );

    Router::new()
        // Root serves index.html
        .route("/", get(h_root))
        // Health check — no auth (Docker HEALTHCHECK)
        .route("/api/health", get(handlers::h_health))
        // Auth endpoints — no auth middleware
        .nest("/api", auth_routes)
        // Protected API routes
        .nest("/api", api_routes.layer(auth_middleware))
        // SABnzbd compat — uses its own API key auth
        .merge(sabnzbd_route)
        // SPA fallback — serve Angular for all unmatched routes
        .fallback(h_spa_fallback)
        .layer(DefaultBodyLimit::max(200 * 1024 * 1024)) // 200 MB for multi-file NZB uploads
        .layer(TraceLayer::new_for_http())
        .layer(cors)
        .with_state(state)
        .merge(SwaggerUi::new("/swagger-ui").url("/api-docs/openapi.json", ApiDoc::openapi()))
}

/// Start the HTTP server with a default router.
pub async fn run(state: Arc<AppState>) -> anyhow::Result<()> {
    let router = build_router(state.clone());
    serve(state, router).await
}

/// Start the HTTP server with a pre-built (possibly augmented) router.
pub async fn serve(state: Arc<AppState>, router: Router) -> anyhow::Result<()> {
    let config = state.config();
    let addr = format!("{}:{}", config.general.listen_addr, config.general.port);
    let listener = TcpListener::bind(&addr).await?;

    info!("HTTP server listening on http://{addr}");
    info!("Web GUI: http://{addr}/");
    info!("Arr API: http://{addr}/sabnzbd/api?mode=version");

    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    info!("HTTP server stopped, shutting down queue manager...");
    state.queue_manager.shutdown().await;
    info!("Graceful shutdown complete");

    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("Failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("Failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => { info!("Received SIGINT, shutting down..."); },
        _ = terminate => { info!("Received SIGTERM, shutting down..."); },
    }
}

#[cfg(test)]
mod tests {
    use super::{cache_header_for, is_hashed_asset};

    #[test]
    fn hashed_asset_detection() {
        assert!(is_hashed_asset("main-7QCIQPRR.js"));
        assert!(is_hashed_asset("styles-C2LX33AG.css"));
        assert!(is_hashed_asset("chunk-2B3NUF5K.js"));
        assert!(!is_hashed_asset("index.html"));
        assert!(!is_hashed_asset("favicon.ico"));
        assert!(!is_hashed_asset("logo.png"));
        // lowercase hash chars should not match (Angular uses uppercase)
        assert!(!is_hashed_asset("main-7qciqprr.js"));
        // hash too short
        assert!(!is_hashed_asset("main-7QCIQPR.js"));
    }

    #[test]
    fn cache_headers() {
        assert_eq!(cache_header_for("index.html"), "no-cache, must-revalidate");
        assert_eq!(
            cache_header_for("main-7QCIQPRR.js"),
            "max-age=31536000, immutable"
        );
        assert_eq!(cache_header_for("favicon.ico"), "max-age=3600");
        assert_eq!(cache_header_for("logo.png"), "max-age=3600");
    }
}
