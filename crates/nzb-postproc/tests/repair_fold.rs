//! WI-148: the PAR2 stage repairs from the download's in-flight repair fold,
//! byte-identical to the disk path, and falls back to the disk path whenever
//! the fold cannot do it.

use std::path::Path;
use std::sync::Arc;

use mock_nntp_server::damage::{DamageRelease, DamageScenario, ScenarioFixture};
use nzb_core::models::{SliceMapSummary, SliceVerdict, StageStatus};
use nzb_decode::{RepairFold, SharedFold};
use nzb_postproc::{PostProcConfig, PostProcResult, run_repair_pipeline};
use parking_lot::Mutex;

const DAMAGED_LOCAL: [u64; 3] = [3, 4, 17];

fn write_release(release: &DamageRelease, dir: &Path) {
    for file in &release.data_files {
        std::fs::write(dir.join(&file.filename), &file.payload).unwrap();
    }
    release.write_par2(dir);
}

fn corrupt_slice(release: &DamageRelease, dir: &Path, filename: &str, local: u64) {
    let path = dir.join(filename);
    let mut bytes = std::fs::read(&path).unwrap();
    let size = release.par2.slice_size();
    let start = (local * size) as usize;
    let end = (start + size as usize).min(bytes.len());
    for b in &mut bytes[start..end] {
        *b ^= 0xA5;
    }
    std::fs::write(path, bytes).unwrap();
}

/// The intact bytes of every global slice, in PAR2 order (as the download
/// engine reads them back to verify).
fn slices(release: &DamageRelease) -> Vec<Vec<u8>> {
    let size = release.par2.slice_size() as usize;
    let mut out = vec![Vec::new(); release.par2.total_slices() as usize];
    for file in &release.data_files {
        let Some(first) = release.par2.first_global_slice(&file.filename) else {
            continue;
        };
        for (i, chunk) in file.payload.chunks(size).enumerate() {
            out[first as usize + i] = chunk.to_vec();
        }
    }
    out
}

struct Case {
    release: DamageRelease,
    map: SliceMapSummary,
    damaged: Vec<usize>,
}

fn case(seed: u64) -> Case {
    let release = DamageRelease::generate("fold", seed);
    let mkv = release.data_files[0].filename.clone();
    let base = release.par2.first_global_slice(&mkv).unwrap() as usize;
    let mut states = vec![SliceVerdict::Verified; release.par2.total_slices() as usize];
    let damaged: Vec<usize> = DAMAGED_LOCAL.iter().map(|l| base + *l as usize).collect();
    for gi in &damaged {
        states[*gi] = SliceVerdict::Damaged;
    }
    let map = SliceMapSummary {
        damaged: damaged.len(),
        unknown: 0,
        recovery_block_count: release.recovery_blocks(),
        states,
    };
    Case {
        release,
        map,
        damaged,
    }
}

/// A fold with `exps` rows, fed every intact slice except those in `skip`.
fn fold(case: &Case, exps: &[u32], skip: &[usize]) -> SharedFold {
    let data = slices(&case.release);
    let mut fold = RepairFold::new(data.len(), case.release.par2.slice_size(), exps).unwrap();
    for (gi, bytes) in data.iter().enumerate() {
        if !case.damaged.contains(&gi) && !skip.contains(&gi) {
            fold.feed(gi, bytes).unwrap();
        }
    }
    Arc::new(Mutex::new(fold))
}

fn config(case: &Case, fold: Option<SharedFold>) -> PostProcConfig {
    PostProcConfig {
        articles_failed: 1,
        content_articles_failed: 1,
        slice_map: Some(case.map.clone()),
        trust_inflight_verify: true,
        repair_fold: fold,
        ..Default::default()
    }
}

fn repair_message(result: &PostProcResult) -> String {
    result
        .stages
        .iter()
        .find(|s| s.name == "Repair" && s.status == StageStatus::Success)
        .and_then(|s| s.message.clone())
        .unwrap_or_else(|| panic!("no successful Repair stage: {result:?}"))
}

async fn run(case: &Case, fold: Option<SharedFold>) -> (tempfile::TempDir, PostProcResult) {
    let dir = tempfile::tempdir().unwrap();
    write_release(&case.release, dir.path());
    let mkv = case.release.data_files[0].filename.clone();
    for local in DAMAGED_LOCAL {
        corrupt_slice(&case.release, dir.path(), &mkv, local);
    }
    let result = run_repair_pipeline(dir.path(), &config(case, fold), None).await;
    assert!(result.success, "{result:?}");
    (dir, result)
}

fn assert_identical(case: &Case, a: &Path, b: &Path) {
    for file in &case.release.data_files {
        let x = std::fs::read(a.join(&file.filename)).unwrap();
        let y = std::fs::read(b.join(&file.filename)).unwrap();
        assert!(x == y, "{} differs between paths", file.filename);
        assert!(y == file.payload, "{} not repaired", file.filename);
    }
}

#[tokio::test]
async fn fold_repair_is_byte_identical_to_disk_repair() {
    let case = case(11);
    let (disk_dir, disk) = run(&case, None).await;
    let rows: Vec<u32> = (0..8).collect();
    let (fold_dir, folded) = run(&case, Some(fold(&case, &rows, &[]))).await;
    assert!(repair_message(&disk).contains("[repair: disk,"), "{disk:?}");
    let msg = repair_message(&folded);
    // Every intact slice was fed during the download: nothing is read here.
    assert!(msg.contains("[repair: fold, 0 slices read,"), "{msg}");
    assert_identical(&case, disk_dir.path(), fold_dir.path());
}

#[tokio::test]
async fn slices_the_fold_missed_are_read_once_at_solve() {
    let case = case(12);
    let (disk_dir, _) = run(&case, None).await;
    let rows: Vec<u32> = (0..4).collect();
    let (fold_dir, folded) = run(&case, Some(fold(&case, &rows, &[0, 1, 2]))).await;
    let msg = repair_message(&folded);
    assert!(msg.contains("[repair: fold, 3 slices read,"), "{msg}");
    assert_identical(&case, disk_dir.path(), fold_dir.path());
}

#[tokio::test]
async fn too_few_rows_are_extended_from_disk() {
    let case = case(13);
    let (disk_dir, _) = run(&case, None).await;
    // One row for three damaged slices: the solve adds two rows.
    let shared = fold(&case, &[0], &[]);
    let (fold_dir, folded) = run(&case, Some(shared.clone())).await;
    assert!(repair_message(&folded).contains("[repair: fold,"));
    assert_eq!(shared.lock().rows(), 3);
    assert_identical(&case, disk_dir.path(), fold_dir.path());
}

#[tokio::test]
async fn poisoned_fold_falls_back_to_disk() {
    let case = case(14);
    let (disk_dir, _) = run(&case, None).await;
    let rows: Vec<u32> = (0..8).collect();
    let shared = fold(&case, &rows, &[]);
    // A slice fed earlier fails verification afterwards.
    shared.lock().note_corrupt(0);
    let (fold_dir, folded) = run(&case, Some(shared)).await;
    let msg = repair_message(&folded);
    assert!(msg.contains("[repair: disk,"), "{msg}");
    assert!(msg.contains("fold refused"), "{msg}");
    assert_identical(&case, disk_dir.path(), fold_dir.path());
}

#[tokio::test]
async fn fold_of_another_set_falls_back_to_disk() {
    let case = case(15);
    let (disk_dir, _) = run(&case, None).await;
    let other = RepairFold::new(3, case.release.par2.slice_size(), &[0]).unwrap();
    let (fold_dir, folded) = run(&case, Some(Arc::new(Mutex::new(other)))).await;
    assert!(repair_message(&folded).contains("[repair: disk,"));
    assert_identical(&case, disk_dir.path(), fold_dir.path());
}

// ---------------------------------------------------------------------------
// WI-1237: the fold path over the deterministic damage fixtures
// ---------------------------------------------------------------------------

/// A fixture written the way the download leaves it: the damaged bytes on
/// disk, plus the slice map the download would have produced. Slices the
/// fixture calls unestablished stay `Unknown`, like a resumed or late-index
/// job, so the verify pass reads that file from disk.
fn damage_case(scenario: DamageScenario) -> (ScenarioFixture, SliceMapSummary, Vec<usize>) {
    let fixture = scenario.fixture();
    let release = &fixture.release;
    let total = release.par2.total_slices() as usize;
    let mut states = vec![SliceVerdict::Verified; total];
    for gi in &fixture.expect.damaged_slices {
        states[*gi as usize] = SliceVerdict::Damaged;
    }
    for gi in &fixture.expect.unknown_slices {
        states[*gi as usize] = SliceVerdict::Unknown;
    }
    let mut damaged: Vec<usize> = fixture
        .expect
        .damaged_slices
        .iter()
        .chain(&fixture.expect.unknown_slices)
        .map(|g| *g as usize)
        .collect();
    damaged.sort_unstable();
    let map = SliceMapSummary {
        damaged: fixture.expect.damaged_slices.len(),
        unknown: fixture.expect.unknown_slices.len(),
        recovery_block_count: fixture.expect.recovery_blocks,
        states,
    };
    (fixture, map, damaged)
}

fn scenario_config(
    fixture: &ScenarioFixture,
    map: SliceMapSummary,
    fold: Option<SharedFold>,
) -> PostProcConfig {
    let expect = &fixture.expect;
    let failed = expect.missing_articles.len()
        + expect.corrupt_articles.len()
        + expect.unestablished_articles.len()
        + expect.files_absent_from_nzb.len();
    PostProcConfig {
        articles_failed: failed,
        content_articles_failed: failed,
        slice_map: Some(map),
        trust_inflight_verify: true,
        repair_fold: fold,
        ..Default::default()
    }
}

/// How a scenario's fold is opened and fed.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Sizing {
    /// One row per recovery block: the widest fold the set allows.
    EveryExponent,
    /// Exactly the D rows the engine opens for the damage it knows about.
    DamageRows,
}

impl Sizing {
    fn exponents(self, recovery_blocks: u32, damaged: usize) -> Vec<u32> {
        let rows = match self {
            Sizing::EveryExponent => recovery_blocks as usize,
            Sizing::DamageRows => damaged,
        };
        (0..rows as u32).collect()
    }
}

/// Run one scenario's job twice — repair from the fold, repair from disk —
/// and assert both write the seeded payload, byte for byte.
///
/// `feed_from_disk`: feed the fold from the bytes the fixture wrote to disk
/// instead of from the pristine payload. A `Verified` slice's bytes on disk are
/// meant to be the payload, so this also proves the on-disk fixture agrees with
/// the slice map the verifier trusts.
async fn assert_fold_matches_disk(scenario: DamageScenario, sizing: Sizing, feed_from_disk: bool) {
    let (fixture, map, damaged) = damage_case(scenario);
    let slug = scenario.slug();
    let release = &fixture.release;

    let all = slices(release);
    assert!(all.len() == map.states.len(), "{slug}: slice count");

    let fold_dir = tempfile::tempdir().unwrap();
    fixture.write_received(fold_dir.path());
    let on_disk: Option<Vec<Vec<u8>>> = feed_from_disk.then(|| {
        let slice = release.par2.slice_size() as usize;
        let mut out = vec![Vec::new(); all.len()];
        for file in &release.data_files {
            let Some(first) = release.par2.first_global_slice(&file.filename) else {
                continue;
            };
            let path = fold_dir.path().join(&file.filename);
            let Ok(bytes) = std::fs::read(&path) else {
                // A file absent from the NZB was never written; its slices stay
                // unfed, exactly as they would for a late index or a resume.
                continue;
            };
            for (i, chunk) in bytes.chunks(slice).enumerate() {
                out[first as usize + i] = chunk.to_vec();
            }
        }
        out
    });
    let source = on_disk.as_deref().unwrap_or(&all);

    let mut fold = RepairFold::new(
        all.len(),
        release.par2.slice_size(),
        &sizing.exponents(fixture.expect.recovery_blocks, damaged.len()),
    )
    .unwrap();
    for (gi, bytes) in source.iter().enumerate() {
        if !damaged.contains(&gi) {
            fold.feed(gi, bytes).unwrap();
        }
    }
    let shared: SharedFold = Arc::new(Mutex::new(fold));
    let folded = run_repair_pipeline(
        fold_dir.path(),
        &scenario_config(&fixture, map.clone(), Some(shared)),
        None,
    )
    .await;
    assert!(
        folded.success,
        "{slug}: fold path failed: {:?} {:?}",
        folded.error, folded.stages
    );
    let msg = repair_message(&folded);
    assert!(
        msg.contains("[repair: fold, 0 slices read,"),
        "{slug}: {msg}"
    );

    let disk_dir = tempfile::tempdir().unwrap();
    fixture.write_received(disk_dir.path());
    let disk =
        run_repair_pipeline(disk_dir.path(), &scenario_config(&fixture, map, None), None).await;
    assert!(
        disk.success,
        "{slug}: disk path failed: {:?} {:?}",
        disk.error, disk.stages
    );
    assert!(
        repair_message(&disk).contains("[repair: disk,"),
        "{slug}: {}",
        repair_message(&disk)
    );

    for file in &release.data_files {
        let from_fold = std::fs::read(fold_dir.path().join(&file.filename)).unwrap();
        let from_disk = std::fs::read(disk_dir.path().join(&file.filename)).unwrap();
        assert!(
            from_fold == from_disk,
            "{slug}: {} differs between the fold and disk repairs",
            file.filename
        );
        assert!(
            from_fold == file.payload,
            "{slug}: {} not byte-identical to the seeded payload",
            file.filename
        );
    }
}

#[tokio::test]
async fn every_repairable_damage_scenario_is_byte_identical_from_the_fold() {
    let mut repairable = 0;
    for scenario in DamageScenario::ALL {
        let expect = scenario.fixture().expect;
        // `BusyProvider` is repairable because nothing is damaged: the retry
        // fetches it, so there is no repair to compare.
        let at_risk = expect.damaged_slices.len() + expect.unknown_slices.len();
        if !expect.repairable || at_risk == 0 {
            continue;
        }
        repairable += 1;
        assert_fold_matches_disk(scenario, Sizing::EveryExponent, false).await;
    }
    // The matrix must actually exercise the repair path.
    assert!(repairable >= 4, "only {repairable} repairable scenarios");
}

/// The same identity, with the fold opened at the row count production uses
/// (D, not every exponent) and fed from the bytes on disk rather than from the
/// payload. This covers the engine's sizing and shows the written fixture
/// agrees with the slice map the verifier trusts.
#[tokio::test]
async fn damage_sized_fold_fed_from_disk_is_byte_identical_to_disk_repair() {
    let mut covered = 0;
    for scenario in DamageScenario::ALL {
        let expect = scenario.fixture().expect;
        let at_risk = expect.damaged_slices.len() + expect.unknown_slices.len();
        if !expect.repairable || at_risk == 0 {
            continue;
        }
        covered += 1;
        assert_fold_matches_disk(scenario, Sizing::DamageRows, true).await;
    }
    assert!(covered >= 4, "only {covered} repairable scenarios");
}

/// A fold whose rows stop short of the damage, and whose budget refuses the
/// extra rows, must not fail the job: the disk path repairs it.
#[tokio::test]
async fn budget_exhaustion_falls_back_to_disk_and_still_repairs() {
    let case = case(16);
    let data = slices(&case.release);
    // One row, and no room to grow: three damaged slices need three.
    let mut fold = RepairFold::new(data.len(), case.release.par2.slice_size(), &[0])
        .unwrap()
        .with_row_limit(1);
    for (gi, bytes) in data.iter().enumerate() {
        if !case.damaged.contains(&gi) {
            fold.feed(gi, bytes).unwrap();
        }
    }
    let shared: SharedFold = Arc::new(Mutex::new(fold));
    let (dir, result) = run(&case, Some(shared)).await;
    let msg = repair_message(&result);
    assert!(msg.contains("[repair: disk,"), "{msg}");
    assert!(
        msg.contains("fold refused: no recovery rows fit the memory budget"),
        "{msg}"
    );
    for file in &case.release.data_files {
        let bytes = std::fs::read(dir.path().join(&file.filename)).unwrap();
        assert!(bytes == file.payload, "{} not repaired", file.filename);
    }
}
