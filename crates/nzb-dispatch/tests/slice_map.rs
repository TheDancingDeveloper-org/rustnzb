//! WI-1228: the download engine verifies PAR2 slices as articles land and
//! reports the per-job slice map with `JobFinished`. Each test runs one
//! deterministic damage scenario from the mock-NNTP fixtures (WI-151) through a
//! live dispatcher and checks the map against the fixture's expectation and
//! against `rust_par2::verify` on the same damage.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use mock_nntp_server::damage::{DamageScenario, ScenarioFixture};
use nzb_core::models::{SliceMapSummary, SliceVerdict};
use nzb_dispatch::bandwidth::{BandwidthConfig, BandwidthLimiter};
use nzb_dispatch::{DispatchEngine, DispatchHandle, ProgressUpdate};
use nzb_nntp::testutil::{MockNntpServer, test_config};
use parking_lot::Mutex;
use tokio::sync::mpsc;
use tokio::time::Instant;

const EVENT_TIMEOUT: Duration = Duration::from_secs(60);

/// Run `scenario` to `JobFinished` and return its slice map.
async fn run(scenario: DamageScenario) -> (ScenarioFixture, SliceMapSummary) {
    let fixture = scenario.fixture();
    let servers: Vec<MockNntpServer> = fixture.start_servers().await;
    let configs = servers
        .iter()
        .enumerate()
        .map(|(priority, server)| {
            let mut config = test_config(server.port());
            config.id = format!("srv{priority}");
            config.name = config.id.clone();
            config.connections = 2;
            config.priority = priority as u8;
            config
        })
        .collect();
    let pool = DispatchHandle::new(
        Arc::new(Mutex::new(configs)),
        Arc::new(BandwidthLimiter::new(BandwidthConfig::default())),
        0,
    );
    pool.start();

    let root = tempfile::tempdir().unwrap();
    let mut job = nzb_core::nzb_parser::parse_nzb(scenario.slug(), &fixture.nzb_xml()).unwrap();
    job.work_dir = root.path().to_path_buf();
    job.output_dir = root.path().join("complete");

    let (tx, mut rx) = mpsc::channel(10_000);
    pool.submit_job(&job, tx);
    let deadline = Instant::now() + EVENT_TIMEOUT;
    loop {
        let event = tokio::time::timeout_at(deadline, rx.recv())
            .await
            .unwrap_or_else(|_| panic!("{scenario:?}: timed out before JobFinished"))
            .expect("progress channel closed before JobFinished");
        if let ProgressUpdate::JobFinished { slice_map, .. } = event {
            let map = slice_map.unwrap_or_else(|| panic!("{scenario:?}: no slice map"));
            return (fixture, map);
        }
    }
}

fn slices_with(map: &SliceMapSummary, verdict: SliceVerdict) -> BTreeSet<u32> {
    map.states
        .iter()
        .enumerate()
        .filter(|(_, s)| **s == verdict)
        .map(|(i, _)| i as u32)
        .collect()
}

/// The damaged global slices `rust_par2::verify` finds in what the downloader
/// should have left on disk for this scenario.
fn par2_verify_damage(fixture: &ScenarioFixture) -> BTreeSet<u32> {
    let dir = tempfile::tempdir().unwrap();
    fixture.write_received(dir.path());
    let index = dir.path().join(&fixture.release.par2_index.filename);
    let set = rust_par2::parse(&index).unwrap();
    let result = rust_par2::verify(&set, dir.path());
    let mut damaged = BTreeSet::new();
    for file in &result.damaged {
        let base = fixture
            .release
            .par2
            .first_global_slice(&file.filename)
            .unwrap();
        damaged.extend(file.damaged_block_indices.iter().map(|i| base + i));
    }
    for file in &result.missing {
        damaged.extend(fixture.release.file_slices(&file.filename));
    }
    damaged
}

fn assert_map(scenario: DamageScenario, fixture: &ScenarioFixture, map: &SliceMapSummary) {
    assert_eq!(
        map.states.len() as u32,
        fixture.release.par2.total_slices(),
        "{scenario:?}: slice count"
    );
    let damaged = slices_with(map, SliceVerdict::Damaged);
    assert_eq!(
        damaged, fixture.expect.damaged_slices,
        "{scenario:?}: damaged slices"
    );
    assert_eq!(
        damaged,
        par2_verify_damage(fixture),
        "{scenario:?}: disagrees with rust_par2::verify"
    );
    assert_eq!(map.damaged, damaged.len());
    assert_eq!(map.unknown, 0, "{scenario:?}: {map:?}");
    let verified = slices_with(map, SliceVerdict::Verified);
    assert_eq!(
        verified.len() + damaged.len(),
        map.states.len(),
        "{scenario:?}: every other slice is verified"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn missing_everywhere_damages_exactly_the_expected_slices() {
    let scenario = DamageScenario::MissingEverywhere;
    let (fixture, map) = run(scenario).await;
    assert!(!fixture.expect.damaged_slices.is_empty());
    assert_map(scenario, &fixture, &map);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn corrupt_body_damages_exactly_the_expected_slice() {
    let scenario = DamageScenario::CorruptBody;
    let (fixture, map) = run(scenario).await;
    assert_eq!(fixture.expect.damaged_slices.len(), 1);
    assert_map(scenario, &fixture, &map);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn missing_on_primary_only_verifies_every_slice() {
    let scenario = DamageScenario::MissingOnPrimaryOnly;
    let (fixture, map) = run(scenario).await;
    assert!(fixture.expect.damaged_slices.is_empty());
    assert_map(scenario, &fixture, &map);
}
