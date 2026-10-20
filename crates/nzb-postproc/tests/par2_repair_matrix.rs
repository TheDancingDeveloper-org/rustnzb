//! WI-1757: PAR2 repair and unpack through `run_pipeline`, over the
//! deterministic damage fixtures (WI-151).
//!
//! - Every `DamageScenario`: a repairable job completes with byte-identical
//!   files, and an unrepairable one fails with `RepairFailed` and leaves the
//!   job directory alone so a retry can use it.
//! - A multi-volume RAR set with a damaged volume is repaired, then
//!   extracted, then cleaned up.
//! - A password-protected RAR extracts with the right password and fails
//!   with `ArchivePasswordRequired` without one or with the wrong one.
//!
//! RAR extraction runs against `mock_nntp_server::fake_unrar`, which fails
//! unless every volume matches its original bytes, so a completed extraction
//! also shows that repair ran first.
#![cfg(unix)]

use std::path::Path;

use mock_nntp_server::damage::{DamageRelease, DamageScenario, seeded_bytes};
use mock_nntp_server::fake_unrar::{self, FakeUnrar};
use nzb_postproc::nzb_core::models::JobFailureCode;
use nzb_postproc::{PostProcConfig, run_pipeline};

/// RAR 5 signature, so detection sniffs the volumes as RAR.
const RAR5_MAGIC: &[u8] = b"Rar!\x1a\x07\x01\x00";

/// A config that verifies with PAR2, the way the queue manager does after
/// `failed` articles, and extracts into `out`.
fn config(failed: usize, out: &Path) -> PostProcConfig {
    PostProcConfig {
        articles_failed: failed,
        content_articles_failed: failed,
        output_dir: Some(out.to_path_buf()),
        ..PostProcConfig::default()
    }
}

fn has_par2(dir: &Path) -> bool {
    std::fs::read_dir(dir)
        .unwrap()
        .any(|e| e.unwrap().path().extension().is_some_and(|x| x == "par2"))
}

#[tokio::test]
async fn every_damage_scenario_repairs_exactly_when_parity_covers_it() {
    for scenario in DamageScenario::ALL {
        let fixture = scenario.fixture();
        let job = tempfile::tempdir().unwrap();
        let out = tempfile::tempdir().unwrap();
        fixture.write_received(job.path());

        // Corrupt bodies decode cleanly, but the slice map flags them, so
        // they count as failures that force a PAR2 pass, like missing and
        // unestablished articles and files left out of the NZB.
        let expect = &fixture.expect;
        let failed = expect.missing_articles.len()
            + expect.corrupt_articles.len()
            + expect.unestablished_articles.len()
            + expect.files_absent_from_nzb.len();
        let result = run_pipeline(job.path(), &config(failed, out.path())).await;
        let slug = scenario.slug();

        if expect.repairable {
            assert!(
                result.success,
                "{slug}: {:?} {:?}",
                result.error, result.stages
            );
            for file in &fixture.release.data_files {
                let on_disk = std::fs::read(job.path().join(&file.filename))
                    .or_else(|_| std::fs::read(out.path().join(&file.filename)))
                    .unwrap_or_else(|e| panic!("{slug}: {} missing: {e}", file.filename));
                assert!(
                    on_disk == file.payload,
                    "{slug}: {} not byte-identical after repair",
                    file.filename
                );
            }
        } else {
            assert!(!result.success, "{slug}: unrepairable damage must fail");
            assert_eq!(
                result.failure_code,
                Some(JobFailureCode::RepairFailed),
                "{slug}: {:?}",
                result.error
            );
            assert!(has_par2(job.path()), "{slug}: PAR2 set kept for a retry");
            for file in &fixture.release.data_files {
                if !expect.files_absent_from_nzb.contains(&file.filename) {
                    assert!(
                        job.path().join(&file.filename).exists(),
                        "{slug}: {} kept for a retry",
                        file.filename
                    );
                }
            }
        }
    }
}

/// A three-volume RAR set protected by a PAR2 set with `volume_blocks`.
struct RarRelease {
    release: DamageRelease,
    volumes: Vec<(String, Vec<u8>)>,
}

impl RarRelease {
    fn new(stem: &str, seed: u64, volume_blocks: &[u32]) -> Self {
        let volumes: Vec<(String, Vec<u8>)> = (1..=3)
            .map(|n| {
                let mut bytes = RAR5_MAGIC.to_vec();
                bytes.extend(seeded_bytes(seed + n, 24_000));
                (format!("{stem}.part{n:02}.rar"), bytes)
            })
            .collect();
        let release = DamageRelease::from_files(stem, volumes.clone(), volume_blocks);
        Self { release, volumes }
    }

    fn volume_refs(&self) -> Vec<(&str, &[u8])> {
        self.volumes
            .iter()
            .map(|(n, b)| (n.as_str(), b.as_slice()))
            .collect()
    }

    /// Write the set into `dir` with `damaged_articles` of the second
    /// volume zeroed (articles are 1-based), plus the PAR2 set.
    fn write_damaged(&self, dir: &Path, damaged_articles: &[u32]) {
        for (index, file) in self.release.data_files.iter().enumerate() {
            let mut bytes = file.payload.clone();
            if index == 1 {
                for segment in &file.segments {
                    if damaged_articles.contains(&segment.segment_number) {
                        let start = segment.offset as usize;
                        bytes[start..start + segment.body.len()].fill(0);
                    }
                }
            }
            std::fs::write(dir.join(&file.filename), bytes).unwrap();
        }
        self.release.write_par2(dir);
    }
}

#[tokio::test]
async fn damaged_multi_volume_rar_is_repaired_then_extracted_and_cleaned() {
    let fake = fake_unrar::install();
    let set = RarRelease::new("repairset", 0x1757_0001, &[1, 2, 4, 8]);
    fake.register("repairset", &set.volume_refs(), None);
    let job = tempfile::tempdir().unwrap();
    let out = tempfile::tempdir().unwrap();
    set.write_damaged(job.path(), &[2]);

    let result = run_pipeline(job.path(), &config(1, out.path())).await;

    assert!(result.success, "{:?} {:?}", result.error, result.stages);
    let extracted = std::fs::read(out.path().join("repairset.mkv")).expect("extracted output");
    assert!(extracted == FakeUnrar::expected_output(&set.volume_refs()));
    for (name, _) in &set.volumes {
        assert!(!job.path().join(name).exists(), "{name} cleaned up");
    }
    assert!(!has_par2(job.path()), "PAR2 files cleaned up");
}

#[tokio::test]
async fn rar_set_damaged_beyond_recovery_fails_without_extracting() {
    let fake = fake_unrar::install();
    // One recovery slice; three zeroed articles touch far more.
    let set = RarRelease::new("hopelessset", 0x1757_0002, &[1]);
    fake.register("hopelessset", &set.volume_refs(), None);
    let job = tempfile::tempdir().unwrap();
    let out = tempfile::tempdir().unwrap();
    set.write_damaged(job.path(), &[1, 2, 3]);

    let result = run_pipeline(job.path(), &config(3, out.path())).await;

    assert!(!result.success);
    assert_eq!(result.failure_code, Some(JobFailureCode::RepairFailed));
    assert!(
        !out.path().join("hopelessset.mkv").exists(),
        "nothing extracted"
    );
    for (name, _) in &set.volumes {
        assert!(job.path().join(name).exists(), "{name} kept for a retry");
    }
    assert!(has_par2(job.path()));
}

/// Write an undamaged single-volume RAR `stem.rar` into a fresh job dir and
/// register it with `password`.
fn passworded_rar(stem: &str, password: &str) -> (tempfile::TempDir, Vec<u8>) {
    let fake = fake_unrar::install();
    let mut bytes = RAR5_MAGIC.to_vec();
    bytes.extend(seeded_bytes(0x1757_0100, 8_000));
    let name = format!("{stem}.rar");
    fake.register(stem, &[(&name, &bytes)], Some(password));
    let job = tempfile::tempdir().unwrap();
    std::fs::write(job.path().join(&name), &bytes).unwrap();
    (job, bytes)
}

#[tokio::test]
async fn password_protected_rar_extracts_with_the_right_password() {
    let (job, bytes) = passworded_rar("lockedok", "s3cr3t pw");
    let out = tempfile::tempdir().unwrap();
    let config = PostProcConfig {
        password: Some("s3cr3t pw".into()),
        ..config(0, out.path())
    };

    let result = run_pipeline(job.path(), &config).await;

    assert!(result.success, "{:?} {:?}", result.error, result.stages);
    assert_eq!(
        std::fs::read(out.path().join("lockedok.mkv")).unwrap(),
        bytes
    );
}

#[tokio::test]
async fn password_protected_rar_without_the_password_fails_typed() {
    for (stem, password) in [("lockednone", None), ("lockedwrong", Some("guess"))] {
        let (job, _) = passworded_rar(stem, "s3cr3t pw");
        let out = tempfile::tempdir().unwrap();
        let config = PostProcConfig {
            password: password.map(str::to_string),
            ..config(0, out.path())
        };

        let result = run_pipeline(job.path(), &config).await;

        assert!(!result.success, "{stem}: must not complete");
        assert_eq!(
            result.failure_code,
            Some(JobFailureCode::ArchivePasswordRequired),
            "{stem}: {:?}",
            result.error
        );
        assert!(!out.path().join(format!("{stem}.mkv")).exists());
        assert!(
            job.path().join(format!("{stem}.rar")).exists(),
            "{stem}: archive kept"
        );
    }
}

/// Control for the test above: with zero failed articles the PAR2 pass is
/// skipped, the damage stays, and the fake extractor rejects the volume. So
/// a successful extraction elsewhere really does depend on the repair.
#[tokio::test]
async fn unrepaired_volume_is_rejected_by_the_extractor() {
    let fake = fake_unrar::install();
    let set = RarRelease::new("controlset", 0x1757_0003, &[1, 2, 4, 8]);
    fake.register("controlset", &set.volume_refs(), None);
    let job = tempfile::tempdir().unwrap();
    let out = tempfile::tempdir().unwrap();
    set.write_damaged(job.path(), &[2]);

    let result = run_pipeline(job.path(), &config(0, out.path())).await;

    assert!(!result.success, "a damaged volume must not extract");
    assert!(!out.path().join("controlset.mkv").exists());
}
