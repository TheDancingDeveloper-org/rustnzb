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

/// How a test bends the standard scenario run.
#[derive(Default)]
struct Twist {
    /// Refuse the PAR2 index on every server for this many attempts, then
    /// hold its real response until every data article has finished, so it
    /// lands after the data. The hold is what orders it: refusal rounds cost
    /// a confirm delay each, but a loaded host can take longer than that to
    /// download the data (WI-2286).
    index_refusals: usize,
    /// Rename every data file in the NZB, as an obfuscated post does.
    obfuscate: bool,
    /// Data-file segments (file index, segment number) already written before
    /// a restart.
    resumed: Vec<(usize, u32)>,
    /// The PAR2 index was already downloaded before the restart.
    index_resumed: bool,
    /// Stall timeout for the pool, in seconds (0: none).
    stall_timeout_secs: u64,
    /// `par2.defer_volumes` (WI-1230): hold recovery volumes until the slice
    /// map shows damage.
    defer_volumes: bool,
}

struct Run {
    fixture: ScenarioFixture,
    map: SliceMapSummary,
    success: bool,
    articles_failed: usize,
    /// NZB file ids in the order their last article completed.
    completion_order: Vec<String>,
    job: nzb_core::models::NzbJob,
    /// The repair fold `JobFinished` carried (WI-148).
    fold: Option<nzb_decode::SharedFold>,
    /// The job's working directory, still holding what was downloaded.
    dir: tempfile::TempDir,
}

/// Run `scenario` to `JobFinished` and return its slice map.
async fn run(scenario: DamageScenario) -> (ScenarioFixture, SliceMapSummary) {
    let run = run_with(scenario, Twist::default()).await;
    (run.fixture, run.map)
}

async fn run_with(scenario: DamageScenario, twist: Twist) -> Run {
    let fixture = scenario.fixture();
    let mut server_configs = fixture.server_configs();
    let (index_gate, index_gate_rx) = tokio::sync::watch::channel(false);
    if twist.index_refusals > 0 {
        for config in &mut server_configs {
            for segment in &fixture.release.par2_index.segments {
                config
                    .article_holds
                    .insert(segment.message_id.clone(), index_gate_rx.clone());
            }
            let sequences = config
                .article_response_sequences
                .get_or_insert_with(Default::default);
            for segment in &fixture.release.par2_index.segments {
                let mid = segment.message_id.clone();
                let refusals = (0..twist.index_refusals)
                    .map(|_| (430u16, format!("No article: <{mid}>")))
                    .collect();
                sequences.lock().insert(mid, refusals);
            }
        }
    }
    let mut servers = Vec::new();
    for config in server_configs {
        servers.push(MockNntpServer::start(config).await);
    }
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
        twist.stall_timeout_secs,
    );
    pool.set_defer_par2_volumes(twist.defer_volumes);
    pool.start();

    let mut xml = String::from_utf8(fixture.nzb_xml()).unwrap();
    if twist.obfuscate {
        for (i, file) in fixture.release.data_files.iter().enumerate() {
            xml = xml.replace(
                &format!("\"{}\"", file.filename),
                &format!("\"a7f3c9e{i}0b12\""),
            );
        }
    }
    let root = tempfile::tempdir().unwrap();
    let mut job = nzb_core::nzb_parser::parse_nzb(scenario.slug(), xml.as_bytes()).unwrap();
    job.work_dir = root.path().to_path_buf();
    job.output_dir = root.path().join("complete");

    // Lay down what a download interrupted by a restart left on disk.
    let data_names: Vec<String> = job
        .files
        .iter()
        .filter(|f| !f.filename.to_lowercase().ends_with(".par2"))
        .map(|f| f.filename.clone())
        .collect();
    for (file_index, number) in &twist.resumed {
        let source = &fixture.release.data_files[*file_index];
        let name = if twist.obfuscate {
            format!("a7f3c9e{file_index}0b12")
        } else {
            source.filename.clone()
        };
        assert!(data_names.contains(&name));
        let segment = &source.segments[*number as usize - 1];
        write_at(&root.path().join(&name), segment.offset, &segment.body);
        let file = job.files.iter_mut().find(|f| f.filename == name).unwrap();
        let article = file
            .articles
            .iter_mut()
            .find(|a| a.segment_number == *number)
            .unwrap();
        article.downloaded = true;
        article.data_begin = Some(segment.offset);
        article.data_size = Some(segment.body.len() as u64);
    }
    if twist.index_resumed {
        let index = &fixture.release.par2_index;
        std::fs::write(root.path().join(&index.filename), &index.payload).unwrap();
        let file = job
            .files
            .iter_mut()
            .find(|f| f.filename == index.filename)
            .unwrap();
        for (article, segment) in file.articles.iter_mut().zip(&index.segments) {
            article.downloaded = true;
            article.data_begin = Some(segment.offset);
            article.data_size = Some(segment.body.len() as u64);
        }
    }

    // Data articles still to finish before the held index may be served.
    let data_ids: BTreeSet<String> = job
        .files
        .iter()
        .filter(|f| !f.filename.to_lowercase().ends_with(".par2"))
        .map(|f| f.id.clone())
        .collect();
    let mut data_pending = job
        .files
        .iter()
        .filter(|f| data_ids.contains(&f.id))
        .flat_map(|f| &f.articles)
        .filter(|a| !a.downloaded)
        .count();
    let release_index = |pending: usize| {
        if pending == 0 {
            index_gate.send_replace(true);
        }
    };
    release_index(data_pending);

    let (tx, mut rx) = mpsc::channel(10_000);
    pool.submit_job(&job, tx);
    let deadline = Instant::now() + EVENT_TIMEOUT;
    let mut completion_order = Vec::new();
    loop {
        let event = tokio::time::timeout_at(deadline, rx.recv())
            .await
            .unwrap_or_else(|_| panic!("{scenario:?}: timed out before JobFinished"))
            .expect("progress channel closed before JobFinished");
        match event {
            ProgressUpdate::ArticleComplete {
                file_id,
                file_complete,
                ..
            } => {
                if data_ids.contains(&file_id) {
                    data_pending -= 1;
                    release_index(data_pending);
                }
                if file_complete {
                    completion_order.push(file_id);
                }
            }
            ProgressUpdate::ArticleFailed { file_id, .. } if data_ids.contains(&file_id) => {
                data_pending -= 1;
                release_index(data_pending);
            }
            ProgressUpdate::JobFinished {
                success,
                articles_failed,
                slice_map,
                repair_fold,
                ..
            } => {
                let map = slice_map.unwrap_or_else(|| panic!("{scenario:?}: no slice map"));
                return Run {
                    fixture,
                    map,
                    success,
                    articles_failed,
                    completion_order,
                    job,
                    fold: repair_fold,
                    dir: root,
                };
            }
            _ => {}
        }
    }
}

fn write_at(path: &std::path::Path, offset: u64, bytes: &[u8]) {
    use std::io::{Seek, SeekFrom, Write};
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
        .unwrap();
    file.seek(SeekFrom::Start(offset)).unwrap();
    file.write_all(bytes).unwrap();
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

/// The index's file id, and assert it completed after every data file.
fn assert_index_last(run: &Run) {
    let index_name = &run.fixture.release.par2_index.filename;
    let id_of = |name: &str| {
        run.job
            .files
            .iter()
            .find(|f| f.filename == name)
            .map(|f| f.id.clone())
            .unwrap()
    };
    let index_at = run
        .completion_order
        .iter()
        .position(|id| *id == id_of(index_name))
        .expect("index completed");
    for file in run
        .job
        .files
        .iter()
        .filter(|f| !f.filename.to_lowercase().ends_with(".par2"))
    {
        if let Some(at) = run.completion_order.iter().position(|id| *id == file.id) {
            assert!(at < index_at, "{} completed after the index", file.filename);
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn index_delivered_last_still_completes_the_map() {
    let scenario = DamageScenario::CorruptBody;
    let run = run_with(
        scenario,
        Twist {
            index_refusals: 2,
            ..Twist::default()
        },
    )
    .await;
    assert_index_last(&run);
    assert_map(scenario, &run.fixture, &run.map);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn obfuscated_names_are_mapped_by_16k_hash() {
    let scenario = DamageScenario::MissingEverywhere;
    let run = run_with(
        scenario,
        Twist {
            obfuscate: true,
            ..Twist::default()
        },
    )
    .await;
    assert_map(scenario, &run.fixture, &run.map);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn obfuscated_names_with_index_last_are_mapped() {
    let scenario = DamageScenario::CorruptBody;
    let run = run_with(
        scenario,
        Twist {
            obfuscate: true,
            index_refusals: 2,
            ..Twist::default()
        },
    )
    .await;
    assert_index_last(&run);
    assert_map(scenario, &run.fixture, &run.map);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restart_verifies_segments_written_before_it() {
    let scenario = DamageScenario::CorruptBody;
    let run = run_with(
        scenario,
        Twist {
            resumed: vec![(0, 1), (0, 2), (0, 3)],
            ..Twist::default()
        },
    )
    .await;
    assert_map(scenario, &run.fixture, &run.map);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restart_with_index_already_on_disk_installs_on_first_write() {
    let scenario = DamageScenario::MissingEverywhere;
    let run = run_with(
        scenario,
        Twist {
            resumed: vec![(0, 1), (0, 2)],
            index_resumed: true,
            ..Twist::default()
        },
    )
    .await;
    assert_map(scenario, &run.fixture, &run.map);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn transport_timeout_on_last_server_leaves_slices_unknown() {
    let scenario = DamageScenario::TransportTimeoutOnLastServer;
    let Run {
        fixture,
        map,
        success,
        articles_failed,
        ..
    } = run_with(
        scenario,
        Twist {
            index_resumed: true,
            stall_timeout_secs: 1,
            ..Twist::default()
        },
    )
    .await;
    assert!(!success);
    assert_eq!(articles_failed, 1);
    assert!(!fixture.expect.unknown_slices.is_empty());
    assert_eq!(slices_with(&map, SliceVerdict::Damaged), BTreeSet::new());
    assert_eq!(
        slices_with(&map, SliceVerdict::Unknown),
        fixture.expect.unknown_slices
    );
    assert_eq!(map.unknown, fixture.expect.unknown_slices.len());
}

/// WI-148: a damaged job's fold is built during the download, holds every
/// intact slice once, and post-processing repairs from it without reading the
/// intact slices again; the bytes match the payload.
async fn repairs_from_the_fold(scenario: DamageScenario) {
    let run = run_with(scenario, Twist::default()).await;
    assert_map(scenario, &run.fixture, &run.map);
    let fold = run
        .fold
        .clone()
        .unwrap_or_else(|| panic!("{scenario:?}: damage seen but no fold: {:?}", run.map));
    let (fed, disk_fed, total) = {
        let f = fold.lock();
        assert!(!f.is_poisoned());
        assert!(f.rows() >= run.map.damaged, "{f:?}");
        (f.fed_count(), f.disk_fed(), f.total_slices())
    };
    let verified = slices_with(&run.map, SliceVerdict::Verified).len();
    // Every fed slice is a verified one, fed once; the pre-damage prefix is
    // the only part read back from disk for the fold.
    assert!(fed <= verified, "fed {fed} of {verified} verified");
    assert!(disk_fed <= fed);
    assert_eq!(total, run.map.states.len());
    // The download wrote everything except the damaged articles; drop in the
    // recovery volumes it would have fetched.
    let dir = run.dir.path();
    for volume in &run.fixture.release.volumes {
        std::fs::write(dir.join(&volume.file.filename), &volume.file.payload).unwrap();
    }
    let config = nzb_postproc::PostProcConfig {
        articles_failed: 1,
        content_articles_failed: 1,
        slice_map: Some(run.map.clone()),
        trust_inflight_verify: true,
        repair_fold: Some(fold.clone()),
        ..Default::default()
    };
    let fold_after = fold;
    let result = nzb_postproc::run_repair_pipeline(dir, &config, None).await;
    assert!(result.success, "{scenario:?}: {result:?}");
    let repair = result
        .stages
        .iter()
        .find(|s| s.name == "Repair")
        .and_then(|s| s.message.clone())
        .unwrap_or_default();
    assert!(repair.contains("[repair: fold,"), "{scenario:?}: {repair}");
    // Every intact slice went into the rows exactly once, and the fold read
    // from disk only what the network path had not handed it.
    let f = fold_after.lock();
    assert_eq!(f.fed_count(), verified, "{scenario:?}: {f:?}");
    assert!(
        f.disk_fed() <= verified - fed + disk_fed,
        "{scenario:?}: {f:?}"
    );
    drop(f);
    for file in &run.fixture.release.data_files {
        let bytes = std::fs::read(dir.join(&file.filename)).unwrap();
        assert!(
            bytes == file.payload,
            "{scenario:?}: {} not repaired",
            file.filename
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn missing_everywhere_repairs_from_the_fold() {
    repairs_from_the_fold(DamageScenario::MissingEverywhere).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn corrupt_body_repairs_from_the_fold() {
    repairs_from_the_fold(DamageScenario::CorruptBody).await;
}

/// WI-1236: with `par2.defer_volumes` on, a `MissingEverywhere` job's fold is
/// driven by the download engine alone. At job end it holds every recovery row
/// the set needs, and every slice that verified before damage was known is
/// read back from disk at most once — the prefix feed is the only disk read
/// the fold does, and it is done in bounded batches.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn missing_everywhere_with_defer_builds_the_fold_from_the_prefix() {
    let scenario = DamageScenario::MissingEverywhere;
    let run = run_with(
        scenario,
        Twist {
            defer_volumes: true,
            ..Twist::default()
        },
    )
    .await;
    assert_map(scenario, &run.fixture, &run.map);
    assert_deferred_fold(
        scenario,
        &run,
        "missing_everywhere_with_defer_builds_the_fold_from_the_prefix",
    );
}

/// WI-1236: the same assertions with the PAR2 index arriving after the data.
/// Nothing can be folded while the slices verify — there is no fold to fold
/// into — so every intact slice reaches the fold through the prefix feed, and
/// each is still read from disk exactly once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn late_index_with_defer_reads_the_prefix_once() {
    let scenario = DamageScenario::MissingEverywhere;
    let run = run_with(
        scenario,
        Twist {
            defer_volumes: true,
            index_refusals: 2,
            ..Twist::default()
        },
    )
    .await;
    assert_map(scenario, &run.fixture, &run.map);
    assert_deferred_fold(
        scenario,
        &run,
        "late_index_with_defer_reads_the_prefix_once",
    );
    let fold = run.fold.clone().expect("damage seen, so a fold exists");
    let f = fold.lock();
    // The index is held on every server until all the data has finished, so
    // data lands and verifies before the set installs and a fold can open.
    // At least one slice is therefore folded from disk, not from the worker
    // that verified it. How many more the rest of the download folds from its
    // own bytes is a race between the workers and the prefix feed, so only the
    // floor is asserted; `assert_deferred_fold` pins the invariant that
    // matters, no re-reads.
    assert!(
        f.disk_reads() > 0,
        "a late index must send the prefix through the disk feed: {f:?}"
    );
}

/// The WI-1236 fold assertions, shared by both deferral shapes: the fold holds
/// a row for every damaged slice, every verified slice is fed, and no slice is
/// ever read back from disk twice for the prefix.
fn assert_deferred_fold(scenario: DamageScenario, run: &Run, name: &str) {
    let fold = run
        .fold
        .clone()
        .unwrap_or_else(|| panic!("{scenario:?}: damage seen but no fold: {:?}", run.map));
    let verified = run
        .map
        .states
        .iter()
        .filter(|s| **s == SliceVerdict::Verified)
        .count();
    let damaged = run.map.damaged;
    let f = fold.lock();
    assert!(!f.is_poisoned(), "{name}: poisoned");
    assert!(
        f.rows() >= damaged,
        "{name}: {} rows for {damaged} damage",
        f.rows()
    );
    assert_eq!(f.fed_count(), verified, "{name}: {f:?}");
    assert_eq!(
        f.disk_rereads(),
        0,
        "{name}: a prefix slice read twice: {f:?}"
    );
    // Every slice read off disk is folded: the feed never reads one and drops
    // it, so the read count and the disk-fed count are the same.
    assert_eq!(
        f.disk_reads(),
        f.disk_fed(),
        "{name}: read but not folded: {f:?}"
    );
    // The prefix is only part of the verified slices once damage opens the
    // fold mid-download; it can never be more than all of them.
    assert!(
        f.disk_reads() <= verified,
        "{name}: read {} of {verified} verified slices: {f:?}",
        f.disk_reads()
    );
    assert!(
        f.disk_read_bytes() <= verified as u64 * run.fixture.release.par2.slice_size(),
        "{name}: {f:?}"
    );
    drop(f);
    // Every recovery row the solve consumes is one the download's volumes
    // carry: the fold did not need post-processing to add any.
    let volume_exponents: BTreeSet<u32> = run
        .fixture
        .release
        .volumes
        .iter()
        .flat_map(|v| v.exponents.iter().copied())
        .collect();
    let fold = run.fold.clone().unwrap();
    let f = fold.lock();
    for (row, exponent) in f.exponents().iter().enumerate() {
        assert!(
            volume_exponents.contains(exponent),
            "{name}: row {row} on exponent {exponent}, not a volume's"
        );
    }
}

/// No damage, no fold: a clean job pays nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn clean_job_opens_no_fold() {
    let run = run_with(DamageScenario::MissingOnPrimaryOnly, Twist::default()).await;
    assert!(run.fold.is_none());
}
