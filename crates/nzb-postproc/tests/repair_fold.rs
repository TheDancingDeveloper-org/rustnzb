//! WI-148: the PAR2 stage repairs from the download's in-flight repair fold,
//! byte-identical to the disk path, and falls back to the disk path whenever
//! the fold cannot do it.

use std::path::Path;
use std::sync::Arc;

use mock_nntp_server::damage::DamageRelease;
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
