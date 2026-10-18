//! WI-1225: the download path records every article outcome in the damage
//! ledger. Each test runs one deterministic damage scenario from the
//! mock-NNTP fixtures (WI-151) through a real `QueueManager` and checks the
//! ledger verdicts the job leaves behind.

mod harness;

use std::time::Duration;

use harness::{HarnessBuilder, ServerProfile};
use mock_nntp_server::damage::{DamageScenario, ScenarioFixture};
use nzb_web::nzb_core::models::JobStatus;
use nzb_web::nzb_core::{LedgerRecord, LedgerState};

const TERMINAL: &[JobStatus] = &[JobStatus::Completed, JobStatus::Failed];

/// Index of the `.mkv` in the submitted NZB: data files come first.
const MKV_FILE_INDEX: u32 = 0;

async fn run(scenario: DamageScenario) -> (harness::TestEngine, String, ScenarioFixture) {
    let fixture = scenario.fixture();
    let mut builder = HarnessBuilder::new().article_timeout(2);
    for (priority, (id, config)) in ["A", "B"]
        .into_iter()
        .zip(fixture.server_configs())
        .enumerate()
    {
        builder = builder.with_server(
            ServerProfile::start(id, config, 2)
                .await
                .with_priority(priority as u8),
        );
    }
    let engine = builder.build();
    let job_id = engine
        .submit_nzb_xml(scenario.slug(), fixture.nzb_xml())
        .expect("submit");
    assert!(
        engine
            .wait_for_status(&job_id, Duration::from_secs(90), TERMINAL)
            .await,
        "{scenario:?}: job never finished"
    );
    (engine, job_id, fixture)
}

fn mkv_records(engine: &harness::TestEngine, job_id: &str) -> Vec<LedgerRecord> {
    engine
        .queue_manager
        .with_db(|db| db.ledger_load_file(job_id, MKV_FILE_INDEX))
        .expect("load ledger")
}

fn state_of(records: &[LedgerRecord], segment: u32) -> Option<LedgerState> {
    records
        .iter()
        .find(|r| r.key.segment_number == segment)
        .map(|r| r.state)
}

/// Poll until every listed segment reaches `want`, then return the records.
async fn wait_for_states(
    engine: &harness::TestEngine,
    job_id: &str,
    segments: &[u32],
    want: LedgerState,
) -> Vec<LedgerRecord> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        let records = mkv_records(engine, job_id);
        if segments
            .iter()
            .all(|s| state_of(&records, *s) == Some(want))
        {
            return records;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("segments {segments:?} never reached {want:?}: {records:#?}");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Every article of the mkv has a record; those not named are present.
fn assert_rest_present(fixture: &ScenarioFixture, records: &[LedgerRecord], damaged: &[u32]) {
    let total = fixture.release.data_files[0].segments.len() as u32;
    for segment in 1..=total {
        let state = state_of(records, segment);
        assert!(state.is_some(), "segment {segment} has no ledger record");
        if !damaged.contains(&segment) {
            assert_eq!(state, Some(LedgerState::Present), "segment {segment}");
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn missing_everywhere_is_confirmed_missing() {
    let (engine, job_id, fixture) = run(DamageScenario::MissingEverywhere).await;
    let records = wait_for_states(&engine, &job_id, &[3, 8], LedgerState::ConfirmedMissing).await;
    assert_rest_present(&fixture, &records, &[3, 8]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn missing_on_primary_only_is_present() {
    let (engine, job_id, fixture) = run(DamageScenario::MissingOnPrimaryOnly).await;
    let records = wait_for_states(&engine, &job_id, &[2, 9], LedgerState::Present).await;
    assert_rest_present(&fixture, &records, &[]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn busy_provider_is_present() {
    let (engine, job_id, fixture) = run(DamageScenario::BusyProvider).await;
    let records = wait_for_states(&engine, &job_id, &[6], LedgerState::Present).await;
    assert_rest_present(&fixture, &records, &[]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn transport_timeout_on_last_server_is_held_provisionally() {
    let (engine, job_id, fixture) = run(DamageScenario::TransportTimeoutOnLastServer).await;
    let records = wait_for_states(&engine, &job_id, &[4], LedgerState::ProvisionalMissing).await;
    // Give the confirm re-ask time to run; a timeout must never confirm.
    tokio::time::sleep(Duration::from_secs(3)).await;
    let records_after = mkv_records(&engine, &job_id);
    assert_eq!(
        state_of(&records_after, 4),
        Some(LedgerState::ProvisionalMissing)
    );
    let key = nzb_web::nzb_core::ArticleKey::new(job_id.as_str(), MKV_FILE_INDEX, 4);
    assert!(
        engine
            .queue_manager
            .with_db(|db| db.ledger_held_provisionally(&key))
            .unwrap()
    );
    assert_rest_present(&fixture, &records, &[4]);
}
