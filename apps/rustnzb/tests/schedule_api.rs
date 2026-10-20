//! The download schedule: validation, persistence, and how it applies to the
//! live queue.
//!
//! Windows are written to cover every day so the tests do not depend on when
//! they run; the evaluator's edge behaviour is tested in `nzb_core::schedule`.

use std::sync::Arc;

use arc_swap::ArcSwap;
use axum::extract::State;
use nzb_web::auth::{CredentialStore, TokenStore};
use nzb_web::log_buffer::LogBuffer;
use nzb_web::nzb_core::config::{AppConfig, ServerConfig};
use nzb_web::nzb_core::db::Database;
use nzb_web::nzb_core::schedule::{ScheduleAction, ScheduleRule};
use nzb_web::queue_manager::PauseReason;
use nzb_web::state::AppState;
use rustnzb::handlers::{
    ScheduleUpdate, SetSpeedLimitBody, h_queue_pause_all, h_queue_resume_all, h_schedule_get,
    h_schedule_update, h_set_speed_limit,
};

const ALL_DAYS: [&str; 7] = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"];

fn test_state() -> (Arc<AppState>, tempfile::TempDir) {
    test_state_with_servers(Vec::new())
}

fn test_state_with_servers(servers: Vec<ServerConfig>) -> (Arc<AppState>, tempfile::TempDir) {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let log_buffer = LogBuffer::default();
    let config = AppConfig {
        servers,
        ..Default::default()
    };
    let manager = nzb_web::queue_manager::QueueManager::new(
        config.servers.clone(),
        Database::open_memory().expect("database"),
        tempdir.path().join("incomplete"),
        tempdir.path().join("complete"),
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
    let state = AppState::new(
        Arc::new(ArcSwap::from_pointee(config)),
        tempdir.path().join("config.toml"),
        manager,
        log_buffer,
        Arc::new(TokenStore::new()),
        Arc::new(CredentialStore::new(tempdir.path().to_path_buf())),
    );
    (Arc::new(state), tempdir)
}

fn rule(action: ScheduleAction) -> ScheduleRule {
    ScheduleRule {
        id: String::new(),
        name: "Always".into(),
        days: ALL_DAYS.iter().map(|day| (*day).to_string()).collect(),
        start: "00:00".into(),
        end: "24:00".into(),
        action,
        ..ScheduleRule::default()
    }
}

async fn update(
    state: &Arc<AppState>,
    enabled: bool,
    rules: Vec<ScheduleRule>,
) -> serde_json::Value {
    let response = h_schedule_update(
        State(Arc::clone(state)),
        Ok(axum::Json(ScheduleUpdate { enabled, rules })),
    )
    .await
    .expect("schedule update");
    serde_json::to_value(response.0).unwrap()
}

async fn rejection(state: &Arc<AppState>, rules: Vec<ScheduleRule>) -> String {
    match h_schedule_update(
        State(Arc::clone(state)),
        Ok(axum::Json(ScheduleUpdate {
            enabled: true,
            rules,
        })),
    )
    .await
    {
        Ok(_) => panic!("update unexpectedly succeeded"),
        Err(error) => {
            assert_eq!(error.status(), http::StatusCode::BAD_REQUEST);
            error.to_string()
        }
    }
}

#[tokio::test]
async fn a_fresh_install_has_a_disabled_empty_schedule() {
    let (state, _temp) = test_state();
    let body = h_schedule_get(State(state)).await.unwrap().0;
    assert!(!body.enabled);
    assert!(body.rules.is_empty());
    assert!(body.active_rule_ids.is_empty());
    assert!(!body.paused);
    assert_eq!(body.speed_limit_bps, None);
    assert!(body.disabled_servers.is_empty());
}

#[tokio::test]
async fn saving_assigns_ids_and_persists_the_rules() {
    let (state, temp) = test_state();
    let body = update(&state, true, vec![rule(ScheduleAction::Pause)]).await;

    assert_eq!(body["enabled"], true);
    let id = body["rules"][0]["id"].as_str().unwrap();
    assert!(!id.is_empty(), "an id is assigned when none is given");
    assert_eq!(body["rules"][0]["action"], "pause");

    let saved = AppConfig::load(&temp.path().join("config.toml")).unwrap();
    assert!(saved.schedule.enabled);
    assert_eq!(saved.schedule.rules.len(), 1);
    assert_eq!(saved.schedule.rules[0].id, id);
    assert_eq!(saved.schedule.rules[0].start, "00:00");
}

#[tokio::test]
async fn saving_keeps_an_existing_id_so_an_active_window_survives_an_edit() {
    let (state, _temp) = test_state();
    let mut first = rule(ScheduleAction::Pause);
    first.id = "night".into();
    let body = update(&state, true, vec![first]).await;
    assert_eq!(body["rules"][0]["id"], "night");
}

#[tokio::test]
async fn invalid_rules_are_rejected_before_anything_is_saved() {
    let (state, temp) = test_state();

    let mut bad_day = rule(ScheduleAction::Pause);
    bad_day.days = vec!["funday".into()];
    assert!(rejection(&state, vec![bad_day]).await.contains("funday"));

    let mut bad_clock = rule(ScheduleAction::Pause);
    bad_clock.start = "9:00".into();
    assert!(rejection(&state, vec![bad_clock]).await.contains("HH:MM"));

    let mut empty_days = rule(ScheduleAction::Pause);
    empty_days.days.clear();
    assert!(
        rejection(&state, vec![empty_days])
            .await
            .contains("at least one day")
    );

    let mut duplicate = rule(ScheduleAction::Pause);
    duplicate.id = "same".into();
    let mut other = duplicate.clone();
    other.name = "Other".into();
    assert!(
        rejection(&state, vec![duplicate, other])
            .await
            .contains("duplicate id")
    );

    let mut unknown = rule(ScheduleAction::DisableServers);
    unknown.servers = vec!["missing".into()];
    assert!(
        rejection(&state, vec![unknown])
            .await
            .contains("unknown server")
    );

    // Nothing was written.
    assert!(!temp.path().join("config.toml").exists());
    assert!(state.config().schedule.rules.is_empty());
}

#[tokio::test]
async fn a_pause_rule_pauses_the_queue_and_reports_why() {
    let (state, _temp) = test_state();
    update(&state, true, vec![rule(ScheduleAction::Pause)]).await;

    assert!(state.queue_manager.is_paused());
    assert!(state.queue_manager.schedule_pause_active());
    assert_eq!(
        state.queue_manager.global_pause_reason(),
        Some(PauseReason::Schedule)
    );

    let body = h_schedule_get(State(Arc::clone(&state))).await.unwrap().0;
    assert!(body.paused);
    assert_eq!(body.active_rule_ids.len(), 1);
}

#[tokio::test]
async fn disabling_the_schedule_lifts_only_the_pause_it_owns() {
    let (state, _temp) = test_state();
    update(&state, true, vec![rule(ScheduleAction::Pause)]).await;

    update(&state, false, vec![rule(ScheduleAction::Pause)]).await;
    assert!(!state.queue_manager.is_paused());
    assert!(!state.queue_manager.schedule_pause_active());

    // A user pause is not the schedule's to lift.
    let _ = h_queue_pause_all(State(Arc::clone(&state))).await.unwrap();
    update(&state, true, vec![rule(ScheduleAction::Pause)]).await;
    update(&state, false, vec![]).await;
    assert!(
        state.queue_manager.is_paused(),
        "the schedule resumed a pause the user started"
    );
    assert_eq!(
        state.queue_manager.global_pause_reason(),
        Some(PauseReason::Global)
    );
}

#[tokio::test]
async fn a_speed_rule_caps_the_live_limit_without_rewriting_the_configured_one() {
    let (state, temp) = test_state();
    let _ = h_set_speed_limit(
        State(Arc::clone(&state)),
        axum::Json(SetSpeedLimitBody {
            speed_limit_bps: 2_000_000,
        }),
    )
    .await
    .unwrap();

    let mut cap = rule(ScheduleAction::SpeedLimit);
    cap.speed_limit_bps = 500_000;
    let body = update(&state, true, vec![cap]).await;
    assert_eq!(body["speed_limit_bps"], 500_000);
    assert_eq!(state.queue_manager.get_speed_limit(), 500_000);

    // The user's own value is untouched on disk and comes back when the cap ends.
    assert_eq!(state.queue_manager.configured_speed_limit(), 2_000_000);
    assert_eq!(
        AppConfig::load(&temp.path().join("config.toml"))
            .unwrap()
            .general
            .speed_limit_bps,
        2_000_000
    );

    update(&state, false, vec![]).await;
    assert_eq!(state.queue_manager.get_speed_limit(), 2_000_000);
}

#[tokio::test]
async fn a_manual_resume_inside_a_window_is_not_undone_by_the_next_tick() {
    let (state, _temp) = test_state();
    // The id is part of a window's occurrence, so a rule the API adopted under
    // a generated id must be saved back with that id: a fresh one is a
    // different rule, and its window is legitimately new.
    let mut pause = rule(ScheduleAction::Pause);
    pause.id = "quiet".into();
    let rules = vec![pause];
    update(&state, true, rules.clone()).await;
    assert!(state.queue_manager.is_paused());

    // The user resumes from the queue view. Saving the same schedule again is
    // what the monitor does on its next minute tick.
    let _ = h_queue_resume_all(State(Arc::clone(&state))).await.unwrap();
    let body = update(&state, true, rules).await;

    assert!(
        !state.queue_manager.is_paused(),
        "the schedule re-paused a queue the user resumed"
    );
    assert!(
        !body["paused"].as_bool().unwrap(),
        "the API still reports a scheduled pause after the user resumed"
    );
    // The rule is still active; the user simply overrode it for this window.
    assert_eq!(body["active_rule_ids"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn an_unlimited_rule_lifts_the_configured_limit_for_its_window() {
    let (state, _temp) = test_state();
    let _ = h_set_speed_limit(
        State(Arc::clone(&state)),
        axum::Json(SetSpeedLimitBody {
            speed_limit_bps: 5_000_000,
        }),
    )
    .await
    .unwrap();
    assert_eq!(state.queue_manager.get_speed_limit(), 5_000_000);

    // `speed_limit_bps = 0` on a speed_limit rule means unlimited, and has to
    // beat the user's configured 5 MB/s rather than fall back to it.
    let mut unlimited = rule(ScheduleAction::SpeedLimit);
    unlimited.speed_limit_bps = 0;
    let body = update(&state, true, vec![unlimited]).await;

    assert_eq!(body["speed_limit_bps"], 0);
    assert_eq!(
        state.queue_manager.get_speed_limit(),
        0,
        "an unlimited window still ran at the configured limit"
    );
    assert_eq!(
        state.queue_manager.configured_speed_limit(),
        5_000_000,
        "the user's configured limit must be kept for after the window"
    );

    update(&state, false, vec![]).await;
    assert_eq!(state.queue_manager.get_speed_limit(), 5_000_000);
}

#[tokio::test]
async fn a_server_rule_disables_servers_in_the_live_list_only() {
    let mut primary = ServerConfig::new("primary", "news.primary.test");
    primary.name = "Primary".into();
    let mut backup = ServerConfig::new("backup", "news.backup.test");
    backup.name = "Backup".into();
    let (state, temp) = test_state_with_servers(vec![primary, backup]);

    let mut disable = rule(ScheduleAction::DisableServers);
    disable.servers = vec!["backup".into()];
    let body = update(&state, true, vec![disable]).await;
    assert_eq!(body["disabled_servers"][0], "backup");

    let live = state.queue_manager.live_servers();
    let backup = live.iter().find(|s| s.id == "backup").unwrap();
    let primary = live.iter().find(|s| s.id == "primary").unwrap();
    assert!(
        !backup.enabled,
        "the schedule should take backup out of rotation"
    );
    assert!(primary.enabled);

    // The configured list, and the file, still say the user's choice.
    assert!(state.config().servers.iter().all(|s| s.enabled));
    let saved = AppConfig::load(&temp.path().join("config.toml")).unwrap();
    assert!(saved.servers.iter().all(|s| s.enabled));

    update(&state, false, vec![]).await;
    assert!(state.queue_manager.live_servers().iter().all(|s| s.enabled));
}

#[tokio::test]
async fn a_window_opening_during_a_disk_hold_takes_effect_when_the_hold_lifts() {
    let (state, _temp) = test_state();
    let manager = &state.queue_manager;

    // The disk guard is holding the queue before the window opens.
    manager.set_min_free_space(u64::MAX);
    manager.enforce_disk_guard();
    assert_eq!(manager.global_pause_reason(), Some(PauseReason::DiskSpace));

    let body = update(&state, true, vec![rule(ScheduleAction::Pause)]).await;
    assert!(
        body["paused"].as_bool().unwrap(),
        "an open window must report paused even while a disk hold overlaps it"
    );

    // Space frees: the guard drops its own hold, but the window is still
    // open and downloads must not run through it.
    manager.set_min_free_space(1);
    manager.enforce_disk_guard();
    assert!(
        manager.is_paused(),
        "downloads ran through a window that opened during a disk hold"
    );
    assert_eq!(manager.global_pause_reason(), Some(PauseReason::Schedule));
    let body = h_schedule_get(State(Arc::clone(&state))).await.unwrap().0;
    assert!(body.paused);
}

#[tokio::test]
async fn a_manual_resume_survives_one_of_two_overlapping_windows_closing() {
    let (state, _temp) = test_state();
    let manager = &state.queue_manager;

    // Two pause rules overlap. Both cover every day and a wide span, so the
    // exact clock does not matter; they are distinguished by id.
    let mut outer = rule(ScheduleAction::Pause);
    outer.id = "outer".into();
    let mut inner = rule(ScheduleAction::Pause);
    inner.id = "inner".into();
    let body = update(&state, true, vec![outer.clone(), inner]).await;
    assert!(body["paused"].as_bool().unwrap());
    let both = body["active_rule_ids"].as_array().unwrap().len();
    assert_eq!(both, 2, "both windows should be open: {body}");

    manager.resume_all();
    assert!(!manager.is_paused());
    let body = h_schedule_get(State(Arc::clone(&state))).await.unwrap().0;
    assert!(!body.paused);

    // One rule goes away while the other is still open. That is not a new
    // window, so the manual resume must hold.
    let body = update(&state, true, vec![outer]).await;
    assert_eq!(body["active_rule_ids"].as_array().unwrap().len(), 1);
    assert_eq!(
        body["paused"], false,
        "a window closing out of an overlap re-paused a manual resume: {body}"
    );
    assert!(!manager.is_paused());
}
