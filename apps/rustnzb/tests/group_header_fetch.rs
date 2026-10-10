//! Header fetch (`POST /api/groups/{id}/headers/fetch`) against the in-process
//! mock NNTP server: BUG-105 (duplicate concurrent fetches), BUG-106 (stuck on
//! an expired range) and BUG-107 (server selection and per-server watermark).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use axum::Json;
use axum::extract::State;
use nzb_nntp::testutil::{MockConfig, MockNntpServer, test_config};
use nzb_web::auth::{CredentialStore, TokenStore};
use nzb_web::nzb_core::config::{AppConfig, ServerConfig};
use nzb_web::nzb_core::db::Database;
use nzb_web::{AppState, QueueManager};
use rustnzb::group_handlers::{IdPath, h_header_clear, h_header_fetch};
use tempfile::TempDir;

const GROUP: &str = "alt.binaries.test";

fn build_state(servers: Vec<ServerConfig>) -> (Arc<AppState>, TempDir) {
    let config = AppConfig {
        servers: servers.clone(),
        ..AppConfig::default()
    };
    let db = Database::open_memory().expect("open in-memory database");
    let tempdir = TempDir::new().expect("create tempdir");
    let incomplete_dir = tempdir.path().join("incomplete");
    let complete_dir = tempdir.path().join("complete");
    std::fs::create_dir_all(&incomplete_dir).expect("create incomplete dir");
    std::fs::create_dir_all(&complete_dir).expect("create complete dir");

    let log_buffer = nzb_web::LogBuffer::new();
    let queue_manager = QueueManager::new(
        servers,
        db,
        incomplete_dir,
        complete_dir,
        log_buffer.clone(),
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
        tempdir.path().join("config.toml"),
        queue_manager,
        log_buffer,
        Arc::new(TokenStore::new()),
        Arc::new(CredentialStore::new(tempdir.path().to_path_buf())),
    ));
    (state, tempdir)
}

fn xover_line(article_num: u64) -> String {
    format!(
        "{article_num}\tSubject {article_num}\tposter@example.test\t01 Jan 2026\t<m{article_num}@test>\t\t100\t5"
    )
}

async fn mock_with_group(first: u64, last: u64, extra: MockConfig) -> MockNntpServer {
    MockNntpServer::start(MockConfig {
        groups: HashMap::from([(GROUP.to_string(), (last - first + 1, first, last))]),
        xover_entries: (first..=last).map(xover_line).collect(),
        ..extra
    })
    .await
}

fn seed_group(state: &AppState) -> i64 {
    let qm = &state.queue_manager;
    qm.with_db(|db| db.group_upsert_batch(&[(GROUP.to_string(), 0, 0)]))
        .expect("insert group");
    qm.with_db(|db| db.group_list(false, Some(GROUP), 10, 0))
        .expect("list groups")
        .pop()
        .expect("group exists")
        .id
}

fn header_count(state: &AppState, group_id: i64) -> i64 {
    state
        .queue_manager
        .with_db(|db| db.header_count(group_id, None))
        .expect("count headers")
}

fn last_scanned(state: &AppState, group_id: i64) -> i64 {
    state
        .queue_manager
        .with_db(|db| db.group_get(group_id))
        .expect("get group")
        .expect("group exists")
        .last_scanned
}

/// The fetch runs in the background; poll the database for its effect.
async fn wait_for(mut done: impl FnMut() -> bool) -> bool {
    for _ in 0..200 {
        if done() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    done()
}

async fn fetch(state: &Arc<AppState>, group_id: i64) -> Result<(), u16> {
    h_header_fetch(State(state.clone()), IdPath(group_id))
        .await
        .map(|Json(_)| ())
        .map_err(|e| e.status().as_u16())
}

/// BUG-106: the watermark points at articles the server has since expired.
/// XOVER of that range answers 423; the fetch must jump to the group's
/// current first article instead of failing forever.
#[tokio::test]
async fn fetch_skips_an_expired_range_instead_of_getting_stuck() {
    let server = mock_with_group(
        50_000_000,
        50_000_002,
        MockConfig {
            xover_423_outside_group: true,
            ..Default::default()
        },
    )
    .await;
    let config = test_config(server.port());
    let (state, _tempdir) = build_state(vec![config.clone()]);
    let group_id = seed_group(&state);
    state
        .queue_manager
        .with_db(|db| db.group_update_watermark(group_id, 100, Some(&config.id)))
        .unwrap();

    fetch(&state, group_id).await.expect("fetch starts");

    assert!(
        wait_for(|| last_scanned(&state, group_id) == 50_000_002).await,
        "watermark must advance past the expired range (stuck at {})",
        last_scanned(&state, group_id)
    );
    assert_eq!(header_count(&state, group_id), 3);
}

/// BUG-107: a disabled server listed first must not be used; the
/// highest-priority enabled server is.
#[tokio::test]
async fn fetch_uses_the_highest_priority_enabled_server() {
    let server = mock_with_group(1, 3, MockConfig::default()).await;
    let unreachable = {
        // Bind and drop a listener to get a port nothing listens on.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().port()
    };
    let server_config = |id: &str, port: u16, priority: u8, enabled: bool| {
        let mut config = test_config(port);
        config.id = id.into();
        config.priority = priority;
        config.enabled = enabled;
        config
    };
    let disabled = server_config("disabled", unreachable, 0, false);
    let low_priority = server_config("low-priority", unreachable, 5, true);
    let primary = server_config("primary", server.port(), 1, true);
    let (state, _tempdir) = build_state(vec![disabled, low_priority, primary]);
    let group_id = seed_group(&state);

    fetch(&state, group_id).await.expect("fetch starts");

    assert!(
        wait_for(|| header_count(&state, group_id) == 3).await,
        "headers must be fetched from the enabled primary server"
    );
    assert!(
        wait_for(|| {
            state
                .queue_manager
                .with_db(|db| db.group_scan_server(group_id))
                .unwrap()
                .as_deref()
                == Some("primary")
        })
        .await
    );
}

/// BUG-107: a watermark measured on another server says nothing about this
/// server's article numbers and must not suppress the fetch.
#[tokio::test]
async fn watermark_from_another_server_is_not_reused() {
    let server = mock_with_group(1, 3, MockConfig::default()).await;
    let (state, _tempdir) = build_state(vec![test_config(server.port())]);
    let group_id = seed_group(&state);
    state
        .queue_manager
        .with_db(|db| db.group_update_watermark(group_id, 900_000, Some("previous-provider")))
        .unwrap();

    fetch(&state, group_id).await.expect("fetch starts");

    assert!(
        wait_for(|| header_count(&state, group_id) == 3 && last_scanned(&state, group_id) == 3)
            .await,
        "articles 1-3 of the new server must be fetched"
    );
}

/// BUG-105: a second fetch of a group while one is running is refused with
/// 409 instead of inserting every header a second time.
#[tokio::test]
async fn concurrent_fetch_of_the_same_group_is_rejected() {
    let server = mock_with_group(
        1,
        3,
        MockConfig {
            // The first fetch stalls after GROUP, so it stays in flight.
            hang_after_command: Some("GROUP".into()),
            ..Default::default()
        },
    )
    .await;
    let (state, _tempdir) = build_state(vec![test_config(server.port())]);
    let group_id = seed_group(&state);

    fetch(&state, group_id).await.expect("first fetch starts");
    assert_eq!(
        fetch(&state, group_id).await,
        Err(409),
        "second fetch must be rejected while the first is running"
    );
}

/// BUG-105: headers can be deleted per group, which also resets the watermark.
#[tokio::test]
async fn clearing_headers_deletes_them_and_resets_the_watermark() {
    let server = mock_with_group(1, 3, MockConfig::default()).await;
    let (state, _tempdir) = build_state(vec![test_config(server.port())]);
    let group_id = seed_group(&state);
    fetch(&state, group_id).await.expect("fetch starts");
    assert!(
        wait_for(|| header_count(&state, group_id) == 3 && last_scanned(&state, group_id) == 3)
            .await
    );

    let Json(body) = h_header_clear(State(state.clone()), IdPath(group_id))
        .await
        .expect("clear headers");
    assert_eq!(body["deleted"], 3);
    assert_eq!(header_count(&state, group_id), 0);
    assert_eq!(last_scanned(&state, group_id), 0);
}
