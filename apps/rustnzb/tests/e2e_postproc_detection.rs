//! End-to-end tests: download output → post-processing file detection.
//!
//! These tests validate that files written by the download/assembly phase
//! are correctly discovered by the post-processing pipeline. This is a
//! recurring integration point failure — the assembler writes files using
//! names derived from NZB subject lines, while the post-processor searches
//! by file extension (.par2, .rar, .7z, .zip).
//!
//! When NZB subjects are obfuscated (common for DMCA avoidance), the
//! subject-derived filenames lack proper extensions, causing the
//! post-processor to report "No par2 files found" and "No archives found."

#![allow(clippy::uninlined_format_args)]

use std::fs;
use std::path::{Path, PathBuf};

use nzb_web::nzb_core::models::StageStatus;
use nzb_web::nzb_postproc::detect::{
    ArchiveType, find_archives, find_cleanup_files, find_par2_files,
};
use nzb_web::nzb_postproc::pipeline::{PostProcConfig, run_pipeline};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Create a temporary work directory populated with the given filenames.
fn make_work_dir(files: &[&str]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    for name in files {
        let path = dir.path().join(name);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        // Write a small marker so the file isn't empty
        fs::write(&path, format!("test-content-for-{name}")).unwrap();
    }
    dir
}

/// Simulate how the download engine constructs output paths.
/// This mirrors `download_engine.rs:170`:
///   let output_path = job.work_dir.join(&file.filename);
fn assembler_output_path(work_dir: &Path, nzb_filename: &str) -> PathBuf {
    work_dir.join(nzb_filename)
}

/// Simulate `extract_filename` from nzb_parser.rs — extracts filename from
/// an NZB subject line. Duplicated here to test the integration without
/// importing private functions.
fn extract_filename_from_subject(subject: &str) -> String {
    // Try to find quoted filename first
    if let Some(start) = subject.find('"')
        && let Some(end) = subject[start + 1..].find('"')
    {
        return subject[start + 1..start + 1 + end].to_string();
    }

    // Try to find filename before (xx/yy) pattern
    if let Some(paren_pos) = subject.rfind('(') {
        let before_paren = subject[..paren_pos].trim();
        // Take the last space-separated token as filename
        if let Some(last_space) = before_paren.rfind(' ') {
            let candidate = &before_paren[last_space + 1..];
            if candidate.contains('.') {
                return candidate.to_string();
            }
        }
        if before_paren.contains('.') {
            return before_paren.to_string();
        }
    }

    subject.to_string()
}

// ===========================================================================
// Test Group 1: Normal (non-obfuscated) NZB subjects
// ===========================================================================

#[test]
fn normal_subjects_par2_detected() {
    // Standard NZB subjects with real filenames
    let subjects = [
        r#"Movie (2024) "Movie.2024.par2" yEnc (01/50)"#,
        r#"Movie (2024) "Movie.2024.vol00+01.par2" yEnc (02/50)"#,
        r#"Movie (2024) "Movie.2024.vol01+02.par2" yEnc (03/50)"#,
    ];

    let filenames: Vec<String> = subjects
        .iter()
        .map(|s| extract_filename_from_subject(s))
        .collect();
    let dir = make_work_dir(&filenames.iter().map(|s| s.as_str()).collect::<Vec<_>>());

    // Verify filenames extracted correctly
    assert_eq!(filenames[0], "Movie.2024.par2");
    assert_eq!(filenames[1], "Movie.2024.vol00+01.par2");
    assert_eq!(filenames[2], "Movie.2024.vol01+02.par2");

    // Post-processor should find all 3 par2 files
    let par2_files = find_par2_files(dir.path());
    assert_eq!(
        par2_files.len(),
        3,
        "Expected 3 par2 files, found {}: {:?}",
        par2_files.len(),
        par2_files
    );

    // Index file should be first
    let first_name = par2_files[0].file_name().unwrap().to_str().unwrap();
    assert!(
        !first_name.contains(".vol"),
        "Index par2 should be first, got: {first_name}"
    );
}

#[test]
fn normal_subjects_archives_detected() {
    let subjects = [
        r#"Movie (2024) "Movie.2024.part001.rar" yEnc (04/50)"#,
        r#"Movie (2024) "Movie.2024.part002.rar" yEnc (05/50)"#,
        r#"Movie (2024) "Movie.2024.part003.rar" yEnc (06/50)"#,
    ];

    let filenames: Vec<String> = subjects
        .iter()
        .map(|s| extract_filename_from_subject(s))
        .collect();
    let dir = make_work_dir(&filenames.iter().map(|s| s.as_str()).collect::<Vec<_>>());

    let archives = find_archives(dir.path());
    assert_eq!(
        archives.len(),
        1,
        "Expected 1 RAR first-volume, found {}: {:?}",
        archives.len(),
        archives
    );
    assert_eq!(archives[0].0, ArchiveType::Rar);
}

#[tokio::test]
async fn normal_subjects_pipeline_finds_files() {
    // Full set: par2 + rar (no actual par2/unrar binary needed — we're testing detection)
    let files = [
        "Movie.2024.par2",
        "Movie.2024.vol00+01.par2",
        "Movie.2024.vol01+02.par2",
        "Movie.2024.part001.rar",
        "Movie.2024.part002.rar",
    ];
    let dir = make_work_dir(&files);

    // With zero failures, par2 should be skipped (files known-good)
    let config_zero = PostProcConfig {
        cleanup_after_extract: false,
        output_dir: None,
        articles_failed: 0,
        content_articles_failed: 0,
        skip_extract: false,
        password: None,
    };
    let result = run_pipeline(dir.path(), &config_zero).await;
    let verify = result.stages.iter().find(|s| s.name == "Verify").unwrap();
    assert_eq!(
        verify.status,
        StageStatus::Skipped,
        "Verify should be skipped when articles_failed == 0 (files known-good)"
    );
    assert!(
        verify
            .message
            .as_deref()
            .unwrap_or("")
            .contains("zero article failures"),
        "Skip message should indicate zero failures"
    );

    // With failures, par2 repair should be attempted (will fail on dummy files,
    // but the stage should NOT be skipped — confirming detection works)
    let config_fail = PostProcConfig {
        cleanup_after_extract: false,
        output_dir: None,
        articles_failed: 1,
        content_articles_failed: 1,
        skip_extract: false,
        password: None,
    };
    let result = run_pipeline(dir.path(), &config_fail).await;
    let repair = result.stages.iter().find(|s| s.name == "Repair").unwrap();
    assert_ne!(
        repair.status,
        StageStatus::Skipped,
        "Repair should not be skipped when articles_failed > 0 and par2 files exist"
    );
}

// ===========================================================================
// Test Group 2: Obfuscated NZB subjects (THE BUG)
// ===========================================================================

#[test]
fn obfuscated_subject_hash_only() {
    // Common obfuscation: subject is just a random hash
    let subjects = [
        "a8f3c72d1e4b5689 (1/50)",
        "a8f3c72d1e4b5689 (2/50)",
        "a8f3c72d1e4b5689 (3/50)",
    ];

    let filenames: Vec<String> = subjects
        .iter()
        .map(|s| extract_filename_from_subject(s))
        .collect();

    // These filenames have no extension — they'll be written as-is
    for f in &filenames {
        assert!(
            !f.contains('.'),
            "Obfuscated filename should have no extension: {f}"
        );
    }

    let dir = make_work_dir(&filenames.iter().map(|s| s.as_str()).collect::<Vec<_>>());

    // BUG: Post-processor can't find par2 files because names lack .par2 extension
    let par2_files = find_par2_files(dir.path());
    // This SHOULD find files (the actual files are par2), but currently returns 0
    // because detection is purely extension-based and the filenames are obfuscated.
    assert_eq!(
        par2_files.len(),
        0,
        "BUG DEMONSTRATION: Obfuscated filenames are invisible to par2 detection. \
         When this test starts failing (par2_files.len() > 0), the fix is working!"
    );
}

#[test]
fn obfuscated_subject_quoted_hash() {
    // Another common pattern: quoted hash in subject
    let subjects = [
        r#"[alt.binaries.movies] "0123456789abcdef0123456789abcdef" yEnc (1/50)"#,
        r#"[alt.binaries.movies] "0123456789abcdef0123456789abcdef" yEnc (2/50)"#,
    ];

    let filenames: Vec<String> = subjects
        .iter()
        .map(|s| extract_filename_from_subject(s))
        .collect();

    // Quoted text is extracted but has no extension
    for f in &filenames {
        assert!(
            !f.ends_with(".par2") && !f.ends_with(".rar"),
            "Obfuscated filename should have no archive extension: {f}"
        );
    }

    let dir = make_work_dir(&filenames.iter().map(|s| s.as_str()).collect::<Vec<_>>());

    let par2_files = find_par2_files(dir.path());
    let archives = find_archives(dir.path());

    assert_eq!(
        par2_files.len(),
        0,
        "BUG: par2 detection fails for obfuscated names"
    );
    assert_eq!(
        archives.len(),
        0,
        "BUG: archive detection fails for obfuscated names"
    );
}

#[test]
fn obfuscated_subject_uuid_style() {
    // UUID-style obfuscation
    let subjects = [
        "3a7c9d2e-1f40-4b8a-bc5e-2d3f4g5h6i7j (1/10)",
        "3a7c9d2e-1f40-4b8a-bc5e-2d3f4g5h6i7j (2/10)",
    ];

    let filenames: Vec<String> = subjects
        .iter()
        .map(|s| extract_filename_from_subject(s))
        .collect();
    let dir = make_work_dir(&filenames.iter().map(|s| s.as_str()).collect::<Vec<_>>());

    let par2_files = find_par2_files(dir.path());
    let archives = find_archives(dir.path());

    assert_eq!(
        par2_files.len(),
        0,
        "BUG: par2 detection fails for UUID-obfuscated names"
    );
    assert_eq!(
        archives.len(),
        0,
        "BUG: archive detection fails for UUID-obfuscated names"
    );
}

#[tokio::test]
async fn obfuscated_pipeline_skips_everything() {
    // Simulate a fully obfuscated download: files exist but have no extensions
    let obfuscated_files = [
        "a8f3c72d1e4b5689", // actually a par2 index
        "b9e4d83f2c5a6790", // actually a par2 volume
        "c0f5e94g3d6b7801", // actually part001.rar
        "d1g6f05h4e7c8912", // actually part002.rar
    ];
    let dir = make_work_dir(&obfuscated_files);
    let config = PostProcConfig {
        cleanup_after_extract: false,
        output_dir: None,
        articles_failed: 0,
        content_articles_failed: 0,
        skip_extract: false,
        password: None,
    };

    let result = run_pipeline(dir.path(), &config).await;

    // Pipeline "succeeds" but skips everything — this is the bug.
    // It reports success because skipped stages aren't failures.
    assert!(
        result.success,
        "Pipeline reports success even though nothing was processed"
    );

    let verify = result.stages.iter().find(|s| s.name == "Verify").unwrap();
    assert_eq!(
        verify.status,
        StageStatus::Skipped,
        "BUG: Verify is skipped because obfuscated files aren't detected"
    );
    assert_eq!(
        verify.message.as_deref(),
        Some("No par2 files found"),
        "BUG: Pipeline can't find par2 files with obfuscated names"
    );

    let extract = result.stages.iter().find(|s| s.name == "Extract").unwrap();
    assert_eq!(
        extract.status,
        StageStatus::Skipped,
        "BUG: Extract is skipped because obfuscated files aren't detected"
    );
    assert_eq!(
        extract.message.as_deref(),
        Some("No archives found"),
        "BUG: Pipeline can't find archives with obfuscated names"
    );
}

// ===========================================================================
// Test Group 3: yEnc filename vs NZB subject filename mismatch
// ===========================================================================

#[test]
fn yenc_filename_is_real_but_unused() {
    // The yEnc =ybegin header contains the real filename.
    // Currently, decode_and_assemble() discards decoded.filename.
    //
    // This test documents the mismatch: the NZB subject gives one name,
    // yEnc gives the real name, but files on disk use the NZB name.

    // NZB subject → obfuscated
    let nzb_subject = r#"[alt.binaries.movies] "0123456789abcdef" yEnc (1/50)"#;
    let nzb_filename = extract_filename_from_subject(nzb_subject);
    assert_eq!(nzb_filename, "0123456789abcdef");

    // yEnc header → real filename (this is what we'd get from decoded.filename)
    let yenc_filename = "Movie.2024.par2";

    // The assembler writes to work_dir/<nzb_filename>
    let dir = tempfile::tempdir().unwrap();
    let disk_path = assembler_output_path(dir.path(), &nzb_filename);
    fs::write(&disk_path, b"fake-par2-content").unwrap();

    // Post-processor searches by extension — finds nothing
    let par2_files = find_par2_files(dir.path());
    assert!(
        par2_files.is_empty(),
        "BUG: File on disk is '{nzb_filename}' but par2 detector looks for '*.par2'"
    );

    // If the file were renamed to the yEnc name, detection would work
    let correct_path = dir.path().join(yenc_filename);
    fs::rename(&disk_path, &correct_path).unwrap();

    let par2_files = find_par2_files(dir.path());
    assert_eq!(
        par2_files.len(),
        1,
        "After renaming to yEnc filename, par2 detection works"
    );
}

// ===========================================================================
// Test Group 4: Deobfuscation rename simulation
// ===========================================================================

/// Simulate the deobfuscation rename logic from download_engine.rs.
/// This mirrors the rename loop that runs after all workers finish.
fn simulate_deobfuscation(
    work_dir: &Path,
    nzb_filenames: &std::collections::HashMap<String, String>,
    yenc_names: &std::collections::HashMap<String, String>,
) -> Vec<(String, String)> {
    let mut renames = Vec::new();
    for (file_id, yenc_name) in yenc_names {
        if let Some(nzb_name) = nzb_filenames.get(file_id) {
            if nzb_name == yenc_name {
                continue;
            }
            let clean_name = Path::new(yenc_name.as_str())
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(yenc_name);
            if clean_name.is_empty() || nzb_name == clean_name {
                continue;
            }
            let old_path = work_dir.join(nzb_name);
            let new_path = work_dir.join(clean_name);
            if old_path.exists() && !new_path.exists() {
                fs::rename(&old_path, &new_path).unwrap();
                renames.push((nzb_name.clone(), clean_name.to_string()));
            }
        }
    }
    renames
}

#[test]
fn deobfuscation_renames_files_correctly() {
    use std::collections::HashMap;

    // Simulate: NZB subjects were obfuscated, yEnc headers have real names
    let dir = make_work_dir(&[
        "a8f3c72d1e4b5689", // will become Movie.2024.par2
        "b9e4d83f2c5a6790", // will become Movie.2024.vol00+01.par2
        "c0f5e94g3d6b7801", // will become Movie.2024.part001.rar
    ]);

    let nzb_filenames: HashMap<String, String> = HashMap::from([
        ("f1".into(), "a8f3c72d1e4b5689".into()),
        ("f2".into(), "b9e4d83f2c5a6790".into()),
        ("f3".into(), "c0f5e94g3d6b7801".into()),
    ]);

    let yenc_names: HashMap<String, String> = HashMap::from([
        ("f1".into(), "Movie.2024.par2".into()),
        ("f2".into(), "Movie.2024.vol00+01.par2".into()),
        ("f3".into(), "Movie.2024.part001.rar".into()),
    ]);

    // Before deobfuscation: pipeline can't find anything
    assert_eq!(find_par2_files(dir.path()).len(), 0);
    assert_eq!(find_archives(dir.path()).len(), 0);

    // Run deobfuscation
    let renames = simulate_deobfuscation(dir.path(), &nzb_filenames, &yenc_names);
    assert_eq!(renames.len(), 3, "All 3 files should be renamed");

    // After deobfuscation: pipeline finds everything
    let par2_files = find_par2_files(dir.path());
    assert_eq!(
        par2_files.len(),
        2,
        "Should find 2 par2 files after deobfuscation"
    );

    let archives = find_archives(dir.path());
    assert_eq!(archives.len(), 1, "Should find 1 RAR after deobfuscation");
    assert_eq!(archives[0].0, ArchiveType::Rar);
}

#[tokio::test]
async fn deobfuscation_enables_full_pipeline() {
    use std::collections::HashMap;

    let dir = make_work_dir(&[
        "a8f3c72d1e4b5689",
        "b9e4d83f2c5a6790",
        "c0f5e94g3d6b7801",
        "d1g6f05h4e7c8912",
    ]);

    let nzb_filenames: HashMap<String, String> = HashMap::from([
        ("f1".into(), "a8f3c72d1e4b5689".into()),
        ("f2".into(), "b9e4d83f2c5a6790".into()),
        ("f3".into(), "c0f5e94g3d6b7801".into()),
        ("f4".into(), "d1g6f05h4e7c8912".into()),
    ]);

    let yenc_names: HashMap<String, String> = HashMap::from([
        ("f1".into(), "Movie.2024.par2".into()),
        ("f2".into(), "Movie.2024.vol00+01.par2".into()),
        ("f3".into(), "Movie.2024.part001.rar".into()),
        ("f4".into(), "Movie.2024.part002.rar".into()),
    ]);

    // Before deobfuscation: pipeline skips everything
    let config = PostProcConfig {
        cleanup_after_extract: false,
        output_dir: None,
        articles_failed: 0,
        content_articles_failed: 0,
        skip_extract: false,
        password: None,
    };
    let result = run_pipeline(dir.path(), &config).await;
    let verify = result.stages.iter().find(|s| s.name == "Verify").unwrap();
    assert_eq!(
        verify.status,
        StageStatus::Skipped,
        "Pre-deobfuscation: verify skipped"
    );

    // Deobfuscate
    simulate_deobfuscation(dir.path(), &nzb_filenames, &yenc_names);

    // After deobfuscation: pipeline finds par2 files. With articles_failed == 0,
    // verify is still skipped (files known-good). To prove detection works, we
    // run with articles_failed > 0 so repair is attempted.
    let config_fail = PostProcConfig {
        cleanup_after_extract: false,
        output_dir: None,
        articles_failed: 1,
        content_articles_failed: 1,
        skip_extract: false,
        password: None,
    };
    let result = run_pipeline(dir.path(), &config_fail).await;
    let repair = result.stages.iter().find(|s| s.name == "Repair").unwrap();
    assert_ne!(
        repair.status,
        StageStatus::Skipped,
        "Post-deobfuscation: repair should find par2 files and attempt processing"
    );

    // Also verify that archives ARE detectable now (even if pipeline didn't reach extract)
    let archives = find_archives(dir.path());
    assert_eq!(
        archives.len(),
        1,
        "Post-deobfuscation: archives should be detectable"
    );
}

#[test]
fn deobfuscation_skips_when_names_match() {
    use std::collections::HashMap;

    // When NZB subject already has the correct filename, no rename needed
    let dir = make_work_dir(&["Movie.2024.par2", "Movie.2024.part001.rar"]);

    let nzb_filenames: HashMap<String, String> = HashMap::from([
        ("f1".into(), "Movie.2024.par2".into()),
        ("f2".into(), "Movie.2024.part001.rar".into()),
    ]);

    let yenc_names: HashMap<String, String> = HashMap::from([
        ("f1".into(), "Movie.2024.par2".into()),
        ("f2".into(), "Movie.2024.part001.rar".into()),
    ]);

    let renames = simulate_deobfuscation(dir.path(), &nzb_filenames, &yenc_names);
    assert_eq!(
        renames.len(),
        0,
        "No renames needed when names already match"
    );

    // Files should still be there with original names
    assert!(dir.path().join("Movie.2024.par2").exists());
    assert!(dir.path().join("Movie.2024.part001.rar").exists());
}

#[test]
fn deobfuscation_handles_path_components_in_yenc_name() {
    use std::collections::HashMap;

    // Some yEnc headers include path components — we should strip them
    let dir = make_work_dir(&["a8f3c72d1e4b5689"]);

    let nzb_filenames: HashMap<String, String> =
        HashMap::from([("f1".into(), "a8f3c72d1e4b5689".into())]);

    let yenc_names: HashMap<String, String> =
        HashMap::from([("f1".into(), "some/path/Movie.2024.par2".into())]);

    let renames = simulate_deobfuscation(dir.path(), &nzb_filenames, &yenc_names);
    assert_eq!(renames.len(), 1);

    // File should be renamed to just the filename, not the full path
    assert!(dir.path().join("Movie.2024.par2").exists());
    assert!(!dir.path().join("a8f3c72d1e4b5689").exists());
}

// ===========================================================================
// Test Group 5: Regression — subdirectory detection (max_depth fix)
// (renumbered from original group 4)
// ===========================================================================

#[test]
fn files_in_subdirectories_are_found() {
    // Regression test for the max_depth(1) fix.
    // Some NZBs have files in subdirectories (e.g. "Subs/movie.srt").
    let files = [
        "Movie.2024.par2",
        "Movie.2024.vol00+01.par2",
        "subdir/Movie.2024.part001.rar",
        "subdir/Movie.2024.part002.rar",
        "Subs/subs.zip",
    ];
    let dir = make_work_dir(&files);

    let par2_files = find_par2_files(dir.path());
    assert_eq!(par2_files.len(), 2, "Should find par2 files at root level");

    let archives = find_archives(dir.path());
    assert_eq!(
        archives.len(),
        2,
        "Should find archives in subdirectories: {:?}",
        archives
    );

    let has_rar = archives.iter().any(|(t, _)| *t == ArchiveType::Rar);
    let has_zip = archives.iter().any(|(t, _)| *t == ArchiveType::Zip);
    assert!(has_rar, "Should find RAR in subdir");
    assert!(has_zip, "Should find ZIP in subdir");
}

#[test]
fn cleanup_finds_files_in_subdirs() {
    let files = [
        "Movie.par2",
        "Movie.vol00+01.par2",
        "subdir/Movie.part001.rar",
        "subdir/Movie.part002.rar",
        "Movie.mkv", // should NOT be cleaned
    ];
    let dir = make_work_dir(&files);

    let cleanup = find_cleanup_files(dir.path());
    assert_eq!(
        cleanup.len(),
        4,
        "Should find 4 cleanup files (2 par2 + 2 rar), got: {:?}",
        cleanup
    );

    // Verify .mkv is not in cleanup list
    for path in &cleanup {
        let name = path.file_name().unwrap().to_str().unwrap();
        assert!(!name.ends_with(".mkv"), "mkv should not be in cleanup list");
    }
}

// ===========================================================================
// Test Group 6: Mixed content scenarios
// ===========================================================================

#[test]
fn mixed_normal_and_obfuscated_partial_detection() {
    // Realistic scenario: some files have proper names (e.g. .nfo, .nzb)
    // but the main content (par2 + rar) is obfuscated
    let files = [
        "Movie.2024.nfo",   // normal name, not an archive
        "a1b2c3d4e5f6g7h8", // obfuscated par2
        "b2c3d4e5f6g7h8i9", // obfuscated rar part 1
        "c3d4e5f6g7h8i9j0", // obfuscated rar part 2
    ];
    let dir = make_work_dir(&files);

    let par2_files = find_par2_files(dir.path());
    let archives = find_archives(dir.path());

    assert_eq!(par2_files.len(), 0, "Obfuscated par2 files not detected");
    assert_eq!(archives.len(), 0, "Obfuscated archives not detected");
}

#[tokio::test]
async fn pipeline_with_only_archives_no_par2() {
    // Non-obfuscated archives but no par2 (some NZBs don't include par2)
    let files = ["Movie.2024.part001.rar", "Movie.2024.part002.rar"];
    let dir = make_work_dir(&files);
    let output_dir = tempfile::tempdir().unwrap();
    let config = PostProcConfig {
        cleanup_after_extract: false,
        output_dir: Some(output_dir.path().to_path_buf()),
        articles_failed: 0,
        content_articles_failed: 0,
        skip_extract: false,
        password: None,
    };

    let result = run_pipeline(dir.path(), &config).await;

    // Verify should be skipped (no par2), but extract should be attempted
    let verify = result.stages.iter().find(|s| s.name == "Verify").unwrap();
    assert_eq!(verify.status, StageStatus::Skipped);

    let extract = result.stages.iter().find(|s| s.name == "Extract").unwrap();
    assert_ne!(
        extract.status,
        StageStatus::Skipped,
        "Extract should not be skipped when rar files exist"
    );
}

// ===========================================================================
// Test Group 7: extract_filename edge cases
// ===========================================================================

#[test]
fn extract_filename_various_subject_formats() {
    // Standard: quoted filename
    assert_eq!(
        extract_filename_from_subject(r#"Some Poster "movie.par2" yEnc (1/50)"#),
        "movie.par2"
    );

    // Standard: filename before (xx/yy)
    assert_eq!(
        extract_filename_from_subject("Some Poster movie.rar (1/50)"),
        "movie.rar"
    );

    // Obfuscated: hash only
    let name = extract_filename_from_subject("a8f3c72d1e4b5689 (1/50)");
    assert!(
        !name.contains('.'),
        "Hash-only subject produces extensionless filename: {name}"
    );

    // Obfuscated: quoted hash
    let name = extract_filename_from_subject(r#"[group] "0123456789abcdef" yEnc (1/50)"#);
    assert_eq!(name, "0123456789abcdef");
    assert!(!name.contains('.'), "Quoted hash has no extension: {name}");

    // Obfuscated: UUID
    let name = extract_filename_from_subject("3a7c9d2e-1f40-4b8a-bc5e-000000000000 (1/10)");
    assert!(
        !name.ends_with(".par2") && !name.ends_with(".rar"),
        "UUID subject should not produce archive extension: {name}"
    );

    // Edge: no parenthesized part counter
    let name = extract_filename_from_subject("just a random subject");
    assert_eq!(name, "just a random subject");
}

// ===========================================================================
// Test Group 8: File move to output (post-pipeline)
// ===========================================================================

#[test]
fn move_to_output_only_copies_top_level_files() {
    // This tests the move_to_history behavior in queue_manager.rs:592-610
    // which only copies top-level files (not recursive).
    // Files in subdirectories are silently left behind.
    let work_dir = make_work_dir(&["movie.mkv", "subdir/subs.srt"]);
    let output_dir = tempfile::tempdir().unwrap();

    // Simulate queue_manager move logic (lines 592-610)
    if let Ok(entries) = fs::read_dir(work_dir.path()) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() {
                let dest = output_dir.path().join(entry.file_name());
                fs::rename(&path, &dest).unwrap();
            }
        }
    }

    // Top-level file should be moved
    assert!(output_dir.path().join("movie.mkv").exists());
    // Subdirectory file is NOT moved (potential secondary bug)
    assert!(
        !output_dir.path().join("subs.srt").exists(),
        "Subdirectory files are not moved to output — this may be a secondary issue"
    );
    // Original subdir file still in work_dir
    assert!(work_dir.path().join("subdir/subs.srt").exists());
}

// ===========================================================================
// Test Group 9: Archive type detection completeness
// ===========================================================================

#[test]
fn all_archive_types_detected() {
    let files = ["movie.part001.rar", "backup.7z", "docs.zip"];
    let dir = make_work_dir(&files);

    let archives = find_archives(dir.path());
    let types: Vec<ArchiveType> = archives.iter().map(|(t, _)| *t).collect();

    assert!(types.contains(&ArchiveType::Rar), "RAR not detected");
    assert!(types.contains(&ArchiveType::SevenZip), "7z not detected");
    assert!(types.contains(&ArchiveType::Zip), "ZIP not detected");
    assert_eq!(archives.len(), 3);
}

#[test]
fn old_style_rar_detection() {
    let files = ["archive.rar", "archive.r00", "archive.r01", "archive.r02"];
    let dir = make_work_dir(&files);

    let archives = find_archives(dir.path());
    assert_eq!(
        archives.len(),
        1,
        "Only first volume (.rar) should be returned"
    );
    assert_eq!(archives[0].0, ArchiveType::Rar);
    assert!(
        archives[0]
            .1
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .ends_with(".rar"),
        "First volume should be the .rar file"
    );
}
