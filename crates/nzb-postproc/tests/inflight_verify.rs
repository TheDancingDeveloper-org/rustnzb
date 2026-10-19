//! WI-147: the PAR2 stage builds its verify result from the in-flight slice
//! map instead of re-reading the data files.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use mock_nntp_server::damage::DamageRelease;
use nzb_core::models::{SliceMapSummary, SliceVerdict, StageStatus};
use nzb_postproc::inflight::{VerifySource, verify_set};
use nzb_postproc::{PostProcConfig, run_repair_pipeline};
use rust_par2::{Par2File, Par2FileSet, SliceChecksum};

fn summary(states: Vec<SliceVerdict>, recovery_block_count: u32) -> SliceMapSummary {
    let damaged = states
        .iter()
        .filter(|s| **s == SliceVerdict::Damaged)
        .count();
    let unknown = states
        .iter()
        .filter(|s| **s == SliceVerdict::Unknown)
        .count();
    SliceMapSummary {
        states,
        recovery_block_count,
        damaged,
        unknown,
    }
}

fn all_verified(release: &DamageRelease) -> Vec<SliceVerdict> {
    vec![SliceVerdict::Verified; release.par2.total_slices() as usize]
}

fn write_release(release: &DamageRelease, dir: &Path) {
    for file in &release.data_files {
        std::fs::write(dir.join(&file.filename), &file.payload).unwrap();
    }
    release.write_par2(dir);
}

fn parse_index(release: &DamageRelease, dir: &Path) -> Par2FileSet {
    rust_par2::parse(&dir.join(&release.par2_index.filename)).unwrap()
}

/// Overwrite slice `local` of `filename` with junk.
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

fn config(map: Option<SliceMapSummary>, trust: bool) -> PostProcConfig {
    PostProcConfig {
        articles_failed: 1,
        content_articles_failed: 1,
        slice_map: map,
        trust_inflight_verify: trust,
        ..Default::default()
    }
}

fn verify_message(result: &nzb_postproc::PostProcResult) -> String {
    result
        .stages
        .iter()
        .find(|s| s.name == "Verify")
        .and_then(|s| s.message.clone())
        .unwrap_or_default()
}

#[test]
fn complete_map_reads_no_data_file() {
    let release = DamageRelease::generate("clean", 1);
    let dir = tempfile::tempdir().unwrap();
    write_release(&release, dir.path());
    // Same size, wrong bytes: a disk pass would call these damaged.
    for file in &release.data_files {
        std::fs::write(
            dir.path().join(&file.filename),
            vec![0u8; file.payload.len()],
        )
        .unwrap();
    }
    let set = parse_index(&release, dir.path());
    let map = summary(all_verified(&release), release.recovery_blocks());

    let verify = verify_set(&set, dir.path(), Some(&map));
    assert_eq!(verify.source, VerifySource::InFlight);
    assert!(verify.result.all_correct());
    assert_eq!(verify.bytes_from_disk, 0);
    let total: u64 = release
        .data_files
        .iter()
        .map(|f| f.payload.len() as u64)
        .sum();
    assert_eq!(verify.bytes_skipped, total);
    assert_eq!(
        verify.result.recovery_blocks_available,
        release.recovery_blocks()
    );
}

/// A 5 GB job whose map is complete: verification finishes from the map
/// alone. The data files are sparse and their checksums made up, so any
/// read of them would mark them damaged (and take far longer). The disk
/// half of this is `complete_map_reads_no_data_file`.
#[test]
fn clean_five_gigabyte_job_reads_zero_bytes() {
    const SLICE: u64 = 1 << 20;
    const FILE: u64 = 2_684_354_560; // 2.5 GiB, two of them
    let dir = tempfile::tempdir().unwrap();
    let mut files = HashMap::new();
    let mut order = Vec::new();
    for i in 0..2u8 {
        let name = format!("big{i}.bin");
        std::fs::File::create(dir.path().join(&name))
            .unwrap()
            .set_len(FILE)
            .unwrap();
        let id = [i + 1; 16];
        order.push(id);
        files.insert(
            id,
            Par2File {
                file_id: id,
                hash: [0xEE; 16],
                hash_16k: [0xEE; 16],
                size: FILE,
                filename: name,
                slices: vec![
                    SliceChecksum {
                        md5: [0xEE; 16],
                        crc32: 1,
                    };
                    FILE.div_ceil(SLICE) as usize
                ],
            },
        );
    }
    let set = Par2FileSet {
        recovery_set_id: [9; 16],
        slice_size: SLICE,
        file_order: order,
        files,
        recovery_block_count: 0,
        creator: None,
    };
    let slices = (2 * FILE.div_ceil(SLICE)) as usize;
    let map = summary(vec![SliceVerdict::Verified; slices], 0);

    let started = std::time::Instant::now();
    let verify = verify_set(&set, dir.path(), Some(&map));
    assert_eq!(verify.source, VerifySource::InFlight);
    assert!(verify.result.all_correct());
    assert_eq!(verify.bytes_from_disk, 0);
    assert_eq!(verify.bytes_skipped, 2 * FILE);
    assert!(started.elapsed().as_secs() < 5, "{:?}", started.elapsed());
}

#[tokio::test]
async fn damaged_job_repairs_byte_identical_to_disk_path() {
    let release = DamageRelease::generate("damaged", 2);
    let mkv = release.data_files[0].filename.clone();
    let damaged_local = [3u64, 4, 17];
    let base = release.par2.first_global_slice(&mkv).unwrap() as usize;
    let mut states = all_verified(&release);
    for local in damaged_local {
        states[base + local as usize] = SliceVerdict::Damaged;
    }
    let map = summary(states, release.recovery_blocks());

    let disk_dir = tempfile::tempdir().unwrap();
    let flight_dir = tempfile::tempdir().unwrap();
    for dir in [disk_dir.path(), flight_dir.path()] {
        write_release(&release, dir);
        for local in damaged_local {
            corrupt_slice(&release, dir, &mkv, local);
        }
    }

    let disk = run_repair_pipeline(disk_dir.path(), &config(Some(map.clone()), false), None).await;
    let flight = run_repair_pipeline(flight_dir.path(), &config(Some(map), true), None).await;
    assert!(disk.success, "{disk:?}");
    assert!(flight.success, "{flight:?}");
    assert!(verify_message(&disk).contains("[verify: disk,"), "{disk:?}");
    assert!(
        verify_message(&flight).contains("[verify: in-flight,"),
        "{flight:?}"
    );
    assert!(
        flight
            .stages
            .iter()
            .any(|s| s.name == "Repair" && s.status == StageStatus::Success)
    );
    for file in &release.data_files {
        let a = std::fs::read(disk_dir.path().join(&file.filename)).unwrap();
        let b = std::fs::read(flight_dir.path().join(&file.filename)).unwrap();
        assert!(a == b, "{} differs between paths", file.filename);
        assert!(b == file.payload, "{} not repaired", file.filename);
    }
}

#[test]
fn unknown_slices_verify_only_that_file_from_disk() {
    let release = DamageRelease::generate("partial", 3);
    let dir = tempfile::tempdir().unwrap();
    write_release(&release, dir.path());
    let nfo = release.data_files[1].filename.clone();
    corrupt_slice(&release, dir.path(), &nfo, 0);
    let mut states = all_verified(&release);
    let nfo_slice = release.par2.first_global_slice(&nfo).unwrap() as usize;
    states[nfo_slice] = SliceVerdict::Unknown;
    let map = summary(states, release.recovery_blocks());
    let set = parse_index(&release, dir.path());

    let verify = verify_set(&set, dir.path(), Some(&map));
    assert_eq!(verify.source, VerifySource::Partial);
    assert_eq!(
        verify.bytes_from_disk,
        release.data_files[1].payload.len() as u64
    );
    assert_eq!(verify.result.intact.len(), 1);
    assert_eq!(verify.result.damaged.len(), 1);
    assert_eq!(verify.result.damaged[0].filename, nfo);
    assert_eq!(verify.result.damaged[0].damaged_block_indices, vec![0]);
}

#[test]
fn file_missing_or_resized_on_disk_falls_back_to_disk() {
    let release = DamageRelease::generate("gone", 4);
    let dir = tempfile::tempdir().unwrap();
    write_release(&release, dir.path());
    let mkv = &release.data_files[0];
    let nfo = &release.data_files[1];
    std::fs::remove_file(dir.path().join(&nfo.filename)).unwrap();
    std::fs::write(dir.path().join(&mkv.filename), &mkv.payload[..1000]).unwrap();
    let map = summary(all_verified(&release), release.recovery_blocks());
    let set = parse_index(&release, dir.path());

    let verify = verify_set(&set, dir.path(), Some(&map));
    assert_eq!(verify.source, VerifySource::Partial);
    assert_eq!(verify.bytes_skipped, 0);
    let missing: HashSet<_> = verify.result.missing.iter().map(|m| &m.filename).collect();
    assert!(missing.contains(&nfo.filename));
    assert_eq!(verify.result.damaged[0].filename, mkv.filename);
}

#[test]
fn map_of_another_shape_is_ignored() {
    let release = DamageRelease::generate("shape", 5);
    let dir = tempfile::tempdir().unwrap();
    write_release(&release, dir.path());
    let mut states = all_verified(&release);
    states.push(SliceVerdict::Verified);
    let map = summary(states, release.recovery_blocks());
    let set = parse_index(&release, dir.path());

    let verify = verify_set(&set, dir.path(), Some(&map));
    assert_eq!(verify.source, VerifySource::Disk);
    assert!(verify.result.all_correct());
}

#[tokio::test]
async fn escape_hatch_forces_the_disk_pass() {
    let release = DamageRelease::generate("hatch", 6);
    let dir = tempfile::tempdir().unwrap();
    write_release(&release, dir.path());
    let map = summary(all_verified(&release), release.recovery_blocks());

    let trusted = run_repair_pipeline(dir.path(), &config(Some(map.clone()), true), None).await;
    assert!(
        verify_message(&trusted).contains("[verify: in-flight,"),
        "{trusted:?}"
    );
    let forced = run_repair_pipeline(dir.path(), &config(Some(map), false), None).await;
    assert!(
        verify_message(&forced).contains("[verify: disk, 0 bytes"),
        "{forced:?}"
    );
}
