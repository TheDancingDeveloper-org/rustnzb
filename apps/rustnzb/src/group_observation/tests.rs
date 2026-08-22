use std::{collections::BTreeSet, sync::Arc};

use arc_swap::ArcSwap;
use axum::{Json, extract::State};
use nzb_web::{
    AppState, QueueManager,
    auth::{CredentialStore, TokenStore},
    nzb_core::{config::AppConfig, db::Database},
};
use tempfile::TempDir;

use super::{
    contract::{HeaderPatternInput, OverviewRangeInput},
    h_header_pattern, h_overview_range, missing_ranges,
};

fn state_without_provider() -> (Arc<AppState>, TempDir) {
    let config = AppConfig::default();
    let database = Database::open_memory().expect("open in-memory database");
    let temporary = TempDir::new().expect("create temporary directory");
    let incomplete = temporary.path().join("incomplete");
    let complete = temporary.path().join("complete");
    std::fs::create_dir_all(&incomplete).expect("create incomplete directory");
    std::fs::create_dir_all(&complete).expect("create complete directory");
    let log = nzb_web::LogBuffer::new();
    let queue = QueueManager::new(
        config.servers.clone(),
        database,
        incomplete,
        complete,
        log.clone(),
        config.general.max_active_downloads,
        config.categories.clone(),
        config.general.min_free_space_bytes,
        config.general.speed_limit_bps,
        false,
        config.general.max_nested_archive_depth,
        config.general.abort_hopeless,
        config.general.early_failure_check,
        config.general.required_completion_pct,
        config.general.article_timeout_secs,
    );
    let state = Arc::new(AppState::new(
        Arc::new(ArcSwap::from_pointee(config)),
        temporary.path().join("config.toml"),
        queue,
        log,
        Arc::new(TokenStore::new()),
        Arc::new(CredentialStore::new(temporary.path().to_path_buf())),
    ));
    (state, temporary)
}

#[tokio::test]
async fn missing_provider_is_a_typed_blocker_for_both_operations() {
    let (state, _temporary) = state_without_provider();
    let Json(overview) = h_overview_range(
        State(Arc::clone(&state)),
        Json(OverviewRangeInput {
            request_id: "overview-one".to_string(),
            group: "esp.binarios.series.misc".to_string(),
            start_article: 1,
            end_article: 10,
            max_headers: 10,
        }),
    )
    .await
    .expect("overview response");
    assert_eq!(overview["status"], "blocked");
    assert_eq!(overview["failure_code"], "nntp_provider_not_configured");
    assert_eq!(overview["request_id"], "overview-one");

    let Json(pattern) = h_header_pattern(
        State(state),
        Json(HeaderPatternInput {
            request_id: "pattern-one".to_string(),
            group: "esp.binarios.series.misc".to_string(),
            start_article: 1,
            end_article: 100_000,
            patterns: vec!["*Traitors*".to_string()],
            max_matches: 100,
        }),
    )
    .await
    .expect("pattern response");
    assert_eq!(pattern["status"], "blocked");
    assert_eq!(pattern["failure_code"], "nntp_provider_not_configured");
    assert_eq!(pattern["request_id"], "pattern-one");
}

#[test]
fn missing_articles_are_compact_ranges() {
    let present = BTreeSet::from([2, 3, 6, 8]);
    assert_eq!(missing_ranges(1, 8, &present), [(1, 1), (4, 5), (7, 7)]);
}
