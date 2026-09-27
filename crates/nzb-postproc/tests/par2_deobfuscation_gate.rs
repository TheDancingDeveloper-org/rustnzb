//! Regression suite for the second half of issue #87: the PAR2-guided
//! deobfuscation that fixes obfuscated names used to be skipped on a healthy
//! download.
//!
//! `rename_to_par2_names` (pipeline.rs) recovers original filenames by matching
//! each file's first-16K MD5 against the PAR2 metadata — exactly what an
//! obfuscated `<hash>.NN` set needs. It used to be called only from inside the
//! verify branch, and verification is skipped outright when
//! `articles_failed == 0` ("files are known-good from CRC checks"), so the one
//! mechanism that can deobfuscate the set only ran when the download was
//! *damaged*.
//!
//! The fix runs the rename whenever the PAR2 index parses, independent of the
//! verify decision. These tests pin that: the pairing mirrors
//! `obfuscated_rar_volumes.rs`:
//!
//! * `neg_*` — the damaged path, where the rename always ran. Passed before
//!   the fix too; proves the rename machinery itself was never the problem.
//! * `pos_*` — the healthy path. Same files, same PAR2, only
//!   `articles_failed` differs. This is the test that failed before the fix.
//! * `guard_*` — the rename must stay targeted now that it runs on every job.
//!
//! Extraction and cleanup are disabled throughout so the assertions are about
//! filenames on disk and nothing else — these tests need no `unrar` binary.

mod support;

use std::fs;
use std::path::Path;

use nzb_postproc::{PostProcConfig, run_pipeline};
use support::par2_fixture::Par2Fixture;

/// The obfuscated set name from the issue report.
const HASH: &str = "cfd4be79c0fb01d429c52a9f8551ee79";

/// Canonical names PAR2 knows the volumes by.
const CANONICAL: [&str; 3] = [
    "Movie.Title.2024.part001.rar",
    "Movie.Title.2024.part002.rar",
    "Movie.Title.2024.part003.rar",
];

/// Volume bodies: real RAR signature plus per-volume filler so each file has a
/// distinct 16K hash and matching is unambiguous.
fn volume_bodies() -> Vec<Vec<u8>> {
    (0..3u8)
        .map(|n| {
            let mut body = b"Rar!\x1a\x07\x00".to_vec();
            body.extend_from_slice(&[n; 512]);
            body
        })
        .collect()
}

/// A job directory holding the three volumes under `names`, plus a PAR2 index
/// that records them under their canonical names.
fn job_dir(names: &[String]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let bodies = volume_bodies();

    let mut fixture = Par2Fixture::new();
    for (canonical, body) in CANONICAL.iter().zip(&bodies) {
        fixture = fixture.add_file(canonical, body);
    }
    fixture.write_index(&dir.path().join("Movie.Title.2024.par2"));

    for (name, body) in names.iter().zip(&bodies) {
        fs::write(dir.path().join(name), body).unwrap();
    }
    dir
}

/// The obfuscated layout: `<hash>.45`, `.46`, `.47`.
fn obfuscated_names() -> Vec<String> {
    (45..=47u32).map(|n| format!("{HASH}.{n}")).collect()
}

/// Pipeline config that isolates the verify/rename stage.
fn config(articles_failed: usize) -> PostProcConfig {
    PostProcConfig {
        articles_failed,
        skip_extract: true,
        cleanup_after_extract: false,
        ..Default::default()
    }
}

/// Sorted non-par2 filenames in `dir`.
fn names_on_disk(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| !n.to_lowercase().ends_with(".par2"))
        .collect();
    names.sort();
    names
}

// ---------------------------------------------------------------------------
// Fixture self-test — if this breaks, every result below is meaningless.
// ---------------------------------------------------------------------------

#[test]
fn neg_par2_fixture_is_parseable_and_names_the_files() {
    let dir = job_dir(&obfuscated_names());
    let index = dir.path().join("Movie.Title.2024.par2");

    let file_set = rust_par2::parse(&index).expect("synthesised PAR2 index must parse");
    let mut names: Vec<String> = file_set
        .files
        .values()
        .map(|f| f.filename.clone())
        .collect();
    names.sort();
    assert_eq!(
        names,
        CANONICAL.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
        "synthesised PAR2 index must report the canonical filenames"
    );

    // The 16K hashes must match what the rename path computes, or matching
    // would silently never fire and the gate tests would prove nothing.
    for (canonical, body) in CANONICAL.iter().zip(volume_bodies()) {
        let probe = dir.path().join("probe.bin");
        fs::write(&probe, &body).unwrap();
        let hash = rust_par2::compute_hash_16k(&probe).unwrap();
        assert!(
            file_set
                .files
                .values()
                .any(|f| f.filename == *canonical && f.hash_16k == hash),
            "16K hash recorded for {canonical} must match compute_hash_16k"
        );
        fs::remove_file(&probe).unwrap();
    }
}

// ---------------------------------------------------------------------------
// Negative control — damaged download. The rename always ran here.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn neg_damaged_download_deobfuscates_via_par2() {
    let dir = job_dir(&obfuscated_names());

    // articles_failed > 0 → verify branch runs → rename_to_par2_names runs.
    let _ = run_pipeline(dir.path(), &config(1)).await;

    assert_eq!(
        names_on_disk(dir.path()),
        CANONICAL.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
        "with articles_failed > 0 the PAR2-guided rename should restore the \
         canonical names"
    );
}

// ---------------------------------------------------------------------------
// Healthy download — this is the case that failed before the fix.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn pos_healthy_download_deobfuscates_via_par2() {
    let dir = job_dir(&obfuscated_names());

    // Identical inputs to the negative control; only articles_failed differs.
    let _ = run_pipeline(dir.path(), &config(0)).await;

    assert_eq!(
        names_on_disk(dir.path()),
        CANONICAL.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
        "a clean download of an obfuscated post kept its <hash>.NN names — the \
         PAR2-guided rename has been re-gated behind the verify branch, which \
         articles_failed == 0 skips, so the set is never deobfuscated and the \
         Extract stage finds no archives (issue #87)"
    );
}

#[tokio::test]
async fn pos_multiple_recovery_sets_each_restore_their_files() {
    let dir = tempfile::tempdir().unwrap();
    let first = b"first recovery set payload";
    let second = b"second recovery set payload";
    Par2Fixture::new()
        .with_recovery_set_id(*b"rustnzbfixture01")
        .add_file("First.Release.rar", first)
        .write_index(&dir.path().join("First.Release.par2"));
    Par2Fixture::new()
        .with_recovery_set_id(*b"rustnzbfixture02")
        .add_file("Second.Release.rar", second)
        .write_index(&dir.path().join("Second.Release.par2"));
    fs::write(dir.path().join("obfuscated.01"), first).unwrap();
    fs::write(dir.path().join("obfuscated.02"), second).unwrap();

    let _ = run_pipeline(dir.path(), &config(0)).await;

    assert_eq!(
        names_on_disk(dir.path()),
        vec![
            "First.Release.rar".to_string(),
            "Second.Release.rar".to_string(),
        ],
        "every distinct PAR2 recovery set must participate in deobfuscation"
    );
}

// ---------------------------------------------------------------------------
// Guards — the rename must stay targeted now that it runs on every job.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn guard_correctly_named_files_are_left_alone() {
    let canonical: Vec<String> = CANONICAL.iter().map(|s| s.to_string()).collect();
    let dir = job_dir(&canonical);

    let _ = run_pipeline(dir.path(), &config(1)).await;

    assert_eq!(
        names_on_disk(dir.path()),
        canonical,
        "files already matching PAR2 must not be renamed"
    );
}

#[tokio::test]
async fn guard_unrelated_files_are_not_renamed() {
    // A file PAR2 knows nothing about must keep its name — lifting the gate
    // must not turn the rename into a free-for-all over the job directory.
    let dir = job_dir(&obfuscated_names());
    fs::write(dir.path().join("readme.nfo"), b"release notes").unwrap();

    let _ = run_pipeline(dir.path(), &config(1)).await;

    assert!(
        dir.path().join("readme.nfo").exists(),
        "a file absent from the PAR2 set must be left untouched"
    );
}
