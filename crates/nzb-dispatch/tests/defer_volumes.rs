//! WI-1230: with `par2.defer_volumes` the download engine holds PAR2 recovery
//! volumes until the job's slice map shows damage, then fetches the smallest
//! prefix of volumes whose blocks cover the damage plus the repair margin.
//! Each test runs a deterministic damage scenario (WI-151) through a live
//! dispatcher and checks which volumes were downloaded into the job
//! directory (the assembler may create a file before any article lands, so a
//! volume counts as fetched only when its bytes are on disk).

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use mock_nntp_server::damage::{DamageScenario, RECOVERY_VOLUME_BLOCKS, ScenarioFixture};
use nzb_dispatch::bandwidth::{BandwidthConfig, BandwidthLimiter};
use nzb_dispatch::{DispatchEngine, DispatchHandle, ProgressUpdate};
use nzb_nntp::testutil::{MockNntpServer, test_config};
use parking_lot::Mutex;
use tokio::sync::mpsc;
use tokio::time::Instant;

const EVENT_TIMEOUT: Duration = Duration::from_secs(60);
const REPAIR_MARGIN: usize = 8;

/// Run `scenario` to `JobFinished` and return the fixture and, in volume
/// order, whether each volume was downloaded.
async fn run(scenario: DamageScenario, defer: bool) -> (ScenarioFixture, Vec<bool>) {
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
    pool.set_defer_par2_volumes(defer);
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
        if let ProgressUpdate::JobFinished { .. } = event {
            break;
        }
    }
    pool.release_completed_job(&job.id);
    let present = volumes_present(&fixture, root.path());
    (fixture, present)
}

fn volumes_present(fixture: &ScenarioFixture, dir: &Path) -> Vec<bool> {
    fixture
        .release
        .volumes
        .iter()
        .map(|v| {
            std::fs::read(dir.join(&v.file.filename))
                .is_ok_and(|bytes| bytes.iter().any(|b| *b != 0))
        })
        .collect()
}

/// How many volumes, lowest first, cover `need` recovery blocks (all of them
/// when none do).
fn covering_prefix(need: usize) -> usize {
    let mut blocks = 0;
    for (i, b) in RECOVERY_VOLUME_BLOCKS.iter().enumerate() {
        blocks += *b as usize;
        if blocks >= need {
            return i + 1;
        }
    }
    RECOVERY_VOLUME_BLOCKS.len()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn clean_job_fetches_no_volumes() {
    let (fixture, present) = run(DamageScenario::MissingOnPrimaryOnly, true).await;
    assert!(fixture.expect.damaged_slices.is_empty());
    assert_eq!(present, vec![false; RECOVERY_VOLUME_BLOCKS.len()]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn damaged_job_fetches_the_minimal_covering_prefix() {
    let (fixture, present) = run(DamageScenario::MissingEverywhere, true).await;
    let damaged = fixture.expect.damaged_slices.len();
    assert!(damaged > 0);
    let prefix = covering_prefix(damaged + REPAIR_MARGIN);
    let expected: Vec<bool> = (0..RECOVERY_VOLUME_BLOCKS.len())
        .map(|i| i < prefix)
        .collect();
    assert_eq!(present, expected, "damaged slices: {damaged}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn defer_off_fetches_every_volume() {
    let (_, present) = run(DamageScenario::MissingOnPrimaryOnly, false).await;
    assert_eq!(present, vec![true; RECOVERY_VOLUME_BLOCKS.len()]);
}
