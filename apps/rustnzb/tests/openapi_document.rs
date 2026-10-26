//! The served OpenAPI document must cover the native API.
//!
//! `/swagger-ui` is only useful if `ApiDoc` actually registers the handlers.
//! These tests make an undocumented handler a visible failure: every native
//! `/api/...` route must appear in the generated document, and every
//! `async fn h_*` in the handler modules must carry a `#[utoipa::path]`
//! annotation.
//!
//! The SABnzbd compatibility layer is deliberately outside the document, so
//! neither check looks at `sabnzbd_compat`.

use std::collections::BTreeSet;

use utoipa::OpenApi;

use rustnzb::server::ApiDoc;

/// Every native route the API router serves, as `(METHOD, path)`.
///
/// Keep this in step with `build_api_router` in `apps/rustnzb/src/server.rs`:
/// a route added there without an entry here is a deliberate decision not to
/// document it, and this list is the place to record that.
const NATIVE_ROUTES: &[(&str, &str)] = &[
    ("GET", "/api/auth/status"),
    ("POST", "/api/auth/login"),
    ("POST", "/api/auth/refresh"),
    ("POST", "/api/auth/logout"),
    ("POST", "/api/auth/setup"),
    ("POST", "/api/auth/change_credentials"),
    ("GET", "/api/status"),
    ("GET", "/api/health"),
    ("GET", "/api/ready"),
    ("GET", "/api/logs"),
    ("GET", "/api/setup/status"),
    ("POST", "/api/setup/first-run"),
    ("POST", "/api/setup/import-sabnzbd"),
    ("POST", "/api/setup/import-sabnzbd-api"),
    ("POST", "/api/setup/apply"),
    ("GET", "/api/queue"),
    ("POST", "/api/queue/add"),
    ("POST", "/api/queue/add-url"),
    ("GET", "/api/queue/admissions/{idempotency_key}"),
    ("POST", "/api/queue/pause"),
    ("POST", "/api/queue/resume"),
    ("POST", "/api/queue/pause-for"),
    ("POST", "/api/queue/{id}/pause"),
    ("POST", "/api/queue/{id}/resume"),
    ("POST", "/api/queue/{id}/move"),
    ("POST", "/api/queue/sort"),
    ("PUT", "/api/queue/{id}/priority"),
    ("PUT", "/api/queue/{id}/category"),
    ("POST", "/api/queue/bulk"),
    ("DELETE", "/api/queue/{id}"),
    ("GET", "/api/history"),
    ("DELETE", "/api/history"),
    ("GET", "/api/history/{id}"),
    ("DELETE", "/api/history/{id}"),
    ("POST", "/api/history/{id}/retry"),
    ("GET", "/api/history/{id}/logs"),
    ("POST", "/api/history/bulk"),
    ("POST", "/api/history/restore"),
    ("GET", "/api/statistics"),
    ("GET", "/api/config"),
    ("PATCH", "/api/config"),
    ("GET", "/api/config/schema"),
    ("GET", "/api/config/effective"),
    ("PUT", "/api/config/general"),
    ("GET", "/api/config/restart-pending"),
    ("GET", "/api/config/backup"),
    ("POST", "/api/config/backup/preview"),
    ("POST", "/api/config/backup/restore"),
    ("GET", "/api/config/schedule"),
    ("PUT", "/api/config/schedule"),
    ("GET", "/api/config/sabnzbd-api-key"),
    ("POST", "/api/config/sabnzbd-api-key/rotate"),
    ("GET", "/api/config/servers/health"),
    ("GET", "/api/config/servers/stats"),
    ("GET", "/api/config/servers"),
    ("POST", "/api/config/servers"),
    ("PUT", "/api/config/servers/{id}"),
    ("DELETE", "/api/config/servers/{id}"),
    ("POST", "/api/config/servers/test-config"),
    ("POST", "/api/config/servers/{id}/test"),
    ("GET", "/api/config/categories"),
    ("POST", "/api/config/categories"),
    ("PUT", "/api/config/categories/{name}"),
    ("DELETE", "/api/config/categories/{name}"),
    ("GET", "/api/config/rss-feeds"),
    ("POST", "/api/config/rss-feeds"),
    ("PUT", "/api/config/rss-feeds/{name}"),
    ("DELETE", "/api/config/rss-feeds/{name}"),
    ("GET", "/api/rss/items"),
    ("POST", "/api/rss/items/{id}/download"),
    ("GET", "/api/rss/rules"),
    ("POST", "/api/rss/rules"),
    ("PUT", "/api/rss/rules/{id}"),
    ("DELETE", "/api/rss/rules/{id}"),
    ("GET", "/api/config/history-retention"),
    ("PUT", "/api/config/history-retention"),
    ("GET", "/api/config/max-active-downloads"),
    ("PUT", "/api/config/max-active-downloads"),
    ("GET", "/api/config/speed-limit"),
    ("PUT", "/api/config/speed-limit"),
    ("GET", "/api/config/disk-guards"),
    ("PUT", "/api/config/disk-guards"),
    ("GET", "/api/config/debug-logging"),
    ("PUT", "/api/config/debug-logging"),
    ("GET", "/api/browse-directory"),
    ("GET", "/api/groups"),
    ("POST", "/api/groups/refresh"),
    ("POST", "/api/groups/overview-range"),
    ("POST", "/api/groups/article-head"),
    ("POST", "/api/groups/article-body-prefix"),
    ("POST", "/api/groups/article-availability"),
    ("POST", "/api/groups/clear-search"),
    ("GET", "/api/groups/{id}"),
    ("GET", "/api/groups/{id}/status"),
    ("POST", "/api/groups/{id}/subscribe"),
    ("POST", "/api/groups/{id}/unsubscribe"),
    ("GET", "/api/groups/{id}/headers"),
    ("DELETE", "/api/groups/{id}/headers"),
    ("POST", "/api/groups/{id}/headers/fetch"),
    ("POST", "/api/groups/{id}/headers/mark-read"),
    ("POST", "/api/groups/{id}/headers/mark-all-read"),
    ("POST", "/api/groups/{id}/headers/download"),
    ("GET", "/api/groups/{id}/threads"),
    ("GET", "/api/groups/{gid}/threads/{root_msg_id}"),
    ("GET", "/api/articles/{message_id}"),
];

/// The WebDAV routes exist only in a build with the `webdav` feature.
#[cfg(feature = "webdav")]
const WEBDAV_ROUTES: &[(&str, &str)] = &[
    ("GET", "/api/dav/status"),
    ("POST", "/api/dav/add"),
    ("GET", "/api/config/dav"),
    ("PUT", "/api/config/dav"),
];

#[cfg(not(feature = "webdav"))]
const WEBDAV_ROUTES: &[(&str, &str)] = &[];

fn operations(document: &utoipa::openapi::OpenApi) -> BTreeSet<(String, String)> {
    let mut found = BTreeSet::new();
    for (path, item) in &document.paths.paths {
        for (method, operation) in [
            ("GET", &item.get),
            ("POST", &item.post),
            ("PUT", &item.put),
            ("DELETE", &item.delete),
            ("PATCH", &item.patch),
            ("HEAD", &item.head),
            ("OPTIONS", &item.options),
            ("TRACE", &item.trace),
        ] {
            if operation.is_some() {
                found.insert((method.to_string(), path.clone()));
            }
        }
    }
    found
}

fn expected() -> BTreeSet<(String, String)> {
    NATIVE_ROUTES
        .iter()
        .chain(WEBDAV_ROUTES.iter())
        .map(|(method, path)| (method.to_string(), path.to_string()))
        .collect()
}

#[test]
fn document_covers_every_native_route() {
    let document = ApiDoc::openapi();
    let documented = operations(&document);
    let expected = expected();

    let missing: BTreeSet<_> = expected.difference(&documented).collect();
    let unexpected: BTreeSet<_> = documented.difference(&expected).collect();

    assert!(
        missing.is_empty(),
        "native routes missing from the OpenAPI document (add a #[utoipa::path] \
         annotation and register it in ApiDoc::paths): {missing:?}"
    );
    assert!(
        unexpected.is_empty(),
        "OpenAPI document lists routes the native router does not serve: {unexpected:?}"
    );
}

#[test]
fn every_operation_is_described() {
    let document = ApiDoc::openapi();

    for (path, item) in &document.paths.paths {
        for (method, operation) in [
            ("GET", &item.get),
            ("POST", &item.post),
            ("PUT", &item.put),
            ("DELETE", &item.delete),
            ("PATCH", &item.patch),
        ] {
            let Some(operation) = operation else {
                continue;
            };
            assert!(
                operation.tags.as_ref().is_some_and(|tags| !tags.is_empty()),
                "{method} {path} has no tag, so Swagger UI files it under \"default\""
            );
            assert!(
                operation
                    .summary
                    .as_deref()
                    .is_some_and(|summary| !summary.is_empty()),
                "{method} {path} has no summary"
            );
            assert!(
                !operation.responses.responses.is_empty(),
                "{method} {path} declares no responses"
            );
        }
    }
}

#[test]
fn schemas_are_registered_for_the_shared_models() {
    let document = ApiDoc::openapi();
    let schemas = document
        .components
        .as_ref()
        .map(|components| {
            components
                .schemas
                .keys()
                .cloned()
                .collect::<BTreeSet<String>>()
        })
        .unwrap_or_default();

    for name in [
        "ApiError",
        "NzbJob",
        "HistoryResponse",
        "QueueResponse",
        "StatusResponse",
        "ServerConfig",
        "TokenResponse",
        "LogResponse",
    ] {
        assert!(
            schemas.contains(name),
            "schema {name} is missing from the OpenAPI components: {schemas:?}"
        );
    }
}

/// Every `async fn h_*` in a handler module must be documented.
///
/// This catches a new handler that is routed but never annotated, which the
/// route list above would only catch once someone remembered to add it there
/// too. `h_root` is not an API handler and lives in `server.rs`, which is not
/// scanned.
#[test]
fn every_handler_is_annotated() {
    let sources: &[(&str, &str)] = &[
        ("handlers.rs", include_str!("../src/handlers.rs")),
        (
            "group_handlers.rs",
            include_str!("../src/group_handlers.rs"),
        ),
        ("admissions.rs", include_str!("../src/admissions.rs")),
        (
            "group_observation.rs",
            include_str!("../src/group_observation.rs"),
        ),
        (
            "group_observation/availability.rs",
            include_str!("../src/group_observation/availability.rs"),
        ),
        (
            "group_observation/body_prefix.rs",
            include_str!("../src/group_observation/body_prefix.rs"),
        ),
        (
            "group_observation/clear_search.rs",
            include_str!("../src/group_observation/clear_search.rs"),
        ),
    ];

    let mut undocumented = Vec::new();
    for (name, source) in sources {
        let lines: Vec<&str> = source.lines().collect();
        for (index, line) in lines.iter().enumerate() {
            let trimmed = line.trim_start();
            let is_handler = trimmed.starts_with("pub async fn h_")
                || trimmed.starts_with("pub(crate) async fn h_");
            if !is_handler {
                continue;
            }

            // The annotation and doc block sit immediately above the
            // handler, separated from whatever precedes them by a blank line,
            // so the contiguous block above the `fn` is what to inspect.
            let mut cursor = index;
            let mut annotated = false;
            while cursor > 0 {
                cursor -= 1;
                let previous = lines[cursor].trim();
                if previous.is_empty() {
                    break;
                }
                if previous.starts_with("#[utoipa::path") {
                    annotated = true;
                    break;
                }
            }

            if !annotated {
                undocumented.push(format!("{name}:{}: {}", index + 1, trimmed));
            }
        }
    }

    assert!(
        undocumented.is_empty(),
        "handlers without a #[utoipa::path] annotation: {undocumented:#?}"
    );
}
