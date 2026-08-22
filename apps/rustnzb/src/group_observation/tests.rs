use std::{collections::BTreeSet, sync::Arc};

use arc_swap::ArcSwap;
use axum::{Json, extract::State};
use nzb_web::{
    AppState, QueueManager,
    auth::{CredentialStore, TokenStore},
    nzb_core::{
        config::AppConfig,
        db::Database,
        nzb_nntp::{
            ServerConfig,
            testutil::{MockConfig, MockNntpServer, test_config},
        },
    },
};
use tempfile::TempDir;

use super::{
    contract::{
        ArticleHeadInput, ClearSearchInput, ClearSearchRangeInput, OverviewRangeInput,
        clear_search_predicate_digest, now_unix_ms,
    },
    h_article_head, h_clear_search, h_overview_range, missing_ranges,
};

fn state_without_provider() -> (Arc<AppState>, TempDir) {
    state_with_servers(Vec::new())
}

fn state_with_servers(servers: Vec<ServerConfig>) -> (Arc<AppState>, TempDir) {
    let config = AppConfig {
        servers,
        ..AppConfig::default()
    };
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
async fn unsupported_xpat_yields_complete_lossless_overview_on_the_same_connection() {
    let group = "esp.binarios.series.misc";
    let mut groups = std::collections::HashMap::new();
    groups.insert(group.to_string(), (2, 1, 2));
    let server = MockNntpServer::start(MockConfig {
        groups,
        xpat_unsupported: true,
        xover_entries: vec![
            "1\tTraitors Espana S02E01\tposter\tWed, 07 May 2025 20:50:00 +0000\t<1@test>\t\t100\t1".into(),
            "2\tUnrelated\tposter\tWed, 07 May 2025 20:51:00 +0000\t<2@test>\t\t100\t1".into(),
        ],
        ..MockConfig::default()
    })
    .await;
    let (state, _temporary) = state_with_servers(vec![test_config(server.port())]);
    let Json(response) = h_clear_search(
        State(state),
        Json(ClearSearchInput {
            request_id: "clear-unsupported".to_string(),
            cancellation_id: "cancel-unsupported".to_string(),
            group: group.to_string(),
            ranges: vec![ClearSearchRangeInput {
                start_article: 1,
                end_article: 2,
            }],
            patterns: vec!["*Traitors*".to_string()],
            predicate_sha256: clear_search_predicate_digest(&["*Traitors*".to_string()]),
            max_matches_per_range: 100,
            max_response_bytes: 1024 * 1024,
            deadline_at_unix_ms: now_unix_ms().expect("clock") + 10_000,
        }),
    )
    .await
    .expect("clear search response");
    assert_eq!(response["status"], "complete");
    assert_eq!(response["execution_state"], "complete");
    assert_eq!(response["connection_reused"], true);
    assert_eq!(response["accelerator"]["state"], "unsupported");
    assert_eq!(response["ranges"][0]["valid_row_count"], 2);
    assert_eq!(response["ranges"][0]["xpat_matches"], serde_json::json!([]));
}

#[tokio::test]
async fn bounded_multi_range_search_returns_one_exact_receipt_per_range() {
    let group = "esp.binarios.series.misc";
    let mut groups = std::collections::HashMap::new();
    groups.insert(group.to_string(), (4, 1, 4));
    let mut xover_entries_by_range = std::collections::HashMap::new();
    xover_entries_by_range.insert(
        "1-2".to_string(),
        vec![
            "1\tTraitors Espana S02E01\tposter\tWed, 07 May 2025 20:50:00 +0000\t<1@test>\t\t100\t1".into(),
            "2\tUnrelated\tposter\tWed, 07 May 2025 20:51:00 +0000\t<2@test>\t\t100\t1".into(),
        ],
    );
    xover_entries_by_range.insert(
        "3-4".to_string(),
        vec![
            "3\tTraitors Espana S02E01 repost\tposter\tWed, 07 May 2025 20:52:00 +0000\t<3@test>\t\t100\t1".into(),
            "4\tUnrelated again\tposter\tWed, 07 May 2025 20:53:00 +0000\t<4@test>\t\t100\t1".into(),
        ],
    );
    let mut xpat_entries_by_request = std::collections::HashMap::new();
    xpat_entries_by_request.insert(
        "1-2 *Traitors*".to_string(),
        vec!["1 Traitors Espana S02E01".to_string()],
    );
    xpat_entries_by_request.insert(
        "3-4 *Traitors*".to_string(),
        vec!["3 Traitors Espana S02E01 repost".to_string()],
    );
    let server = MockNntpServer::start(MockConfig {
        groups,
        xover_entries_by_range,
        xpat_entries_by_request,
        ..MockConfig::default()
    })
    .await;
    let (state, _temporary) = state_with_servers(vec![test_config(server.port())]);
    let Json(response) = h_clear_search(
        State(state),
        Json(ClearSearchInput {
            request_id: "clear-multi".to_string(),
            cancellation_id: "cancel-multi".to_string(),
            group: group.to_string(),
            ranges: vec![
                ClearSearchRangeInput {
                    start_article: 1,
                    end_article: 2,
                },
                ClearSearchRangeInput {
                    start_article: 3,
                    end_article: 4,
                },
            ],
            patterns: vec!["*Traitors*".to_string()],
            predicate_sha256: clear_search_predicate_digest(&["*Traitors*".to_string()]),
            max_matches_per_range: 100,
            max_response_bytes: 1024 * 1024,
            deadline_at_unix_ms: now_unix_ms().expect("clock") + 10_000,
        }),
    )
    .await
    .expect("clear search response");
    assert_eq!(response["execution_state"], "complete");
    assert_eq!(response["accelerator"]["state"], "supported");
    assert_eq!(response["ranges"].as_array().map(Vec::len), Some(2));
    assert_eq!(response["ranges"][0]["receipt_state"], "complete");
    assert_eq!(response["ranges"][1]["receipt_state"], "complete");
    assert_eq!(response["ranges"][0]["xpat_match_count"], 1);
    assert_eq!(response["ranges"][1]["xpat_match_count"], 1);
}

#[tokio::test]
async fn deadline_cancellation_receipts_cover_every_admitted_range() {
    let group = "esp.binarios.series.misc";
    let mut groups = std::collections::HashMap::new();
    groups.insert(group.to_string(), (4, 1, 4));
    let server = MockNntpServer::start(MockConfig {
        groups,
        response_delay: Some(std::time::Duration::from_millis(100)),
        ..MockConfig::default()
    })
    .await;
    let (state, _temporary) = state_with_servers(vec![test_config(server.port())]);
    let Json(response) = h_clear_search(
        State(state),
        Json(ClearSearchInput {
            request_id: "clear-deadline".to_string(),
            cancellation_id: "cancel-deadline".to_string(),
            group: group.to_string(),
            ranges: vec![
                ClearSearchRangeInput {
                    start_article: 1,
                    end_article: 2,
                },
                ClearSearchRangeInput {
                    start_article: 3,
                    end_article: 4,
                },
            ],
            patterns: vec!["*Traitors*".to_string()],
            predicate_sha256: clear_search_predicate_digest(&["*Traitors*".to_string()]),
            max_matches_per_range: 100,
            max_response_bytes: 1024 * 1024,
            deadline_at_unix_ms: now_unix_ms().expect("clock") + 20,
        }),
    )
    .await
    .expect("clear search response");
    assert_eq!(response["execution_state"], "incomplete");
    assert_eq!(response["ranges"].as_array().map(Vec::len), Some(2));
    assert!(response["ranges"].as_array().is_some_and(|ranges| {
        ranges.iter().all(|range| {
            range["receipt_state"] == "cancelled"
                && range["failure_code"] == "nntp_operation_timed_out"
        })
    }));
}

#[tokio::test]
async fn missing_provider_is_a_typed_blocker_for_every_observation() {
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

    let Json(clear_search) = h_clear_search(
        State(Arc::clone(&state)),
        Json(ClearSearchInput {
            request_id: "clear-one".to_string(),
            cancellation_id: "cancel-one".to_string(),
            group: "esp.binarios.series.misc".to_string(),
            ranges: vec![ClearSearchRangeInput {
                start_article: 1,
                end_article: 10,
            }],
            patterns: vec!["*Traitors*".to_string()],
            predicate_sha256: clear_search_predicate_digest(&["*Traitors*".to_string()]),
            max_matches_per_range: 100,
            max_response_bytes: 1024 * 1024,
            deadline_at_unix_ms: now_unix_ms().expect("clock") + 10_000,
        }),
    )
    .await
    .expect("clear search response");
    assert_eq!(clear_search["status"], "complete");
    assert_eq!(clear_search["execution_state"], "incomplete");
    assert_eq!(clear_search["ranges"][0]["receipt_state"], "refused");
    assert_eq!(
        clear_search["ranges"][0]["failure_code"],
        "nntp_provider_not_configured"
    );
    assert_eq!(clear_search["request_id"], "clear-one");

    let Json(head) = h_article_head(
        State(state),
        Json(ArticleHeadInput {
            request_id: "head-one".to_string(),
            group: "esp.binarios.series.misc".to_string(),
            article_number: 42,
            max_header_bytes: 64 * 1024,
        }),
    )
    .await
    .expect("head response");
    assert_eq!(head["status"], "blocked");
    assert_eq!(head["failure_code"], "nntp_provider_not_configured");
    assert_eq!(head["request_id"], "head-one");
}

#[test]
fn missing_articles_are_compact_ranges() {
    let present = BTreeSet::from([2, 3, 6, 8]);
    assert_eq!(missing_ranges(1, 8, &present), [(1, 1), (4, 5), (7, 7)]);
}
