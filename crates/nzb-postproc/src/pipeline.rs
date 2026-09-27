//! Post-processing pipeline orchestrator.
//!
//! Stages:
//! - **Verify** — native PAR2 verification (skipped when articles_failed == 0)
//! - **Repair** — native PAR2 repair when files are damaged
//! - **Extract** — unpack RAR, 7z, ZIP archives
//! - **Cleanup** — remove archive/par2 files

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use nzb_core::models::{JobFailureCode, StageResult, StageStatus};
use tracing::{debug, error, info, warn};

use crate::detect::{ArchiveType, find_archives, find_cleanup_files, find_par2_files};
use crate::par2::par2_repair;
use crate::resources::PostProcResourcePool;
use crate::unpack::{ArchivePasswordRequired, extract_7z, extract_rar, extract_tar, extract_zip};

fn increment_counter(name: &'static str) {
    opentelemetry::global::meter_provider()
        .meter("rustnzb")
        .u64_counter(name)
        .build()
        .add(1, &[]);
}

/// Outcome of the combined verify+repair spawn_blocking task.
/// Keeps VerifyResult (which is !Send) on the blocking thread, then returns
/// only Send-safe data back to the async context.
enum VerifyRepairOutcome {
    AllCorrect {
        intact_count: usize,
    },
    Damaged {
        intact: usize,
        damaged: usize,
        missing: usize,
        blocks_needed: u32,
        blocks_available: u32,
        repair_result: Result<rust_par2::RepairResult, rust_par2::RepairError>,
    },
}

/// Final result of the complete post-processing pipeline.
#[derive(Debug)]
pub struct PostProcResult {
    /// Whether all stages completed successfully.
    pub success: bool,
    /// Results from each stage that was attempted.
    pub stages: Vec<StageResult>,
    /// Error message if the pipeline failed.
    pub error: Option<String>,
    /// Typed terminal failure code when the pipeline failed.
    pub failure_code: Option<JobFailureCode>,
}

/// Configuration for the post-processing pipeline.
#[derive(Debug, Clone)]
pub struct PostProcConfig {
    /// Remove par2 and archive files after successful extraction.
    pub cleanup_after_extract: bool,
    /// Directory where extracted files should be placed.
    /// If None, extracts into the job directory itself.
    pub output_dir: Option<PathBuf>,
    /// Number of articles that failed during download.
    /// When 0, par2 verification is skipped (files are known-good).
    /// When > 0, `par2 repair` is run directly (which verifies + repairs
    /// in a single pass), avoiding the redundant verify-then-repair double-scan.
    pub articles_failed: usize,
    /// Failed articles belonging to source/content files. PAR2 volume
    /// failures do not make an otherwise intact archive unextractable.
    pub content_articles_failed: usize,
    /// When true, the extract stage is skipped because direct unpack already
    /// handled RAR extraction during the download phase.
    pub skip_extract: bool,
    /// Optional archive password (from NZB metadata or indexer API).
    pub password: Option<String>,
    /// Maximum archive nesting depth processed automatically. Zero permits
    /// the outer archive only; the default handles five nested layers.
    pub max_nested_archive_depth: u8,
}

impl Default for PostProcConfig {
    fn default() -> Self {
        Self {
            cleanup_after_extract: true,
            output_dir: None,
            articles_failed: 0,
            content_articles_failed: 0,
            skip_extract: false,
            password: None,
            max_nested_archive_depth: 5,
        }
    }
}

/// Run the full post-processing pipeline on a completed job directory.
///
/// Stages executed in order:
/// 1. **Verify** — par2 verification
/// 2. **Repair** — par2 repair (only if verify found issues)
/// 3. **Extract** — unpack RAR, 7z, ZIP archives
/// 4. **Cleanup** — remove archive/par2 files (if configured)
pub async fn run_pipeline(job_dir: &Path, config: &PostProcConfig) -> PostProcResult {
    run_pipeline_with_resources(job_dir, config, None).await
}

/// Run the pipeline under optional shared stage-specific resource gates.
///
/// This additive entry point keeps [`PostProcConfig`] source-compatible for
/// library consumers while allowing applications to coordinate independent
/// jobs through one [`PostProcResourcePool`].
pub async fn run_pipeline_with_resources(
    job_dir: &Path,
    config: &PostProcConfig,
    resources: Option<&Arc<PostProcResourcePool>>,
) -> PostProcResult {
    run_pipeline_with_cleanup(job_dir, config, resources, &[], &[]).await
}

/// Run the pipeline with optional category cleanup rules.
pub async fn run_pipeline_with_cleanup(
    job_dir: &Path,
    config: &PostProcConfig,
    resources: Option<&Arc<PostProcResourcePool>>,
    cleanup_patterns: &[String],
    unwanted_extensions: &[String],
) -> PostProcResult {
    let mut stages: Vec<StageResult> = Vec::new();
    let mut pipeline_ok = true;
    let mut failure_code = None;

    info!(dir = %job_dir.display(), "Starting post-processing pipeline");

    // ------------------------------------------------------------------
    // Stage 1: Native PAR2 verification
    // ------------------------------------------------------------------
    // Parse the PAR2 index file and verify all files via MD5 hashing.
    // This is pure Rust — no process spawn, no stdout parsing.
    //
    // If all files pass → done (no par2cmdline needed).
    // If files are damaged → attempt native repair.
    //
    // When articles_failed == 0 the files are known-good from CRC checks
    // during yEnc decode, so we skip the expensive MD5 verification pass.
    let par2_files = find_par2_files(job_dir);

    info!(
        par2_files = par2_files.len(),
        "PAR2 files discovered for post-processing"
    );

    if par2_files.is_empty() {
        if config.content_articles_failed > 0 {
            pipeline_ok = false;
            failure_code = Some(JobFailureCode::ArticlesUnavailable);
            stages.push(StageResult {
                name: "Verify".to_string(),
                status: StageStatus::Failed,
                message: Some(format!(
                    "{} content article(s) missing and no PAR2 recovery set is available",
                    config.content_articles_failed
                )),
                duration_secs: 0.0,
            });
        } else {
            stages.push(StageResult {
                name: "Verify".to_string(),
                status: StageStatus::Skipped,
                message: Some("No par2 files found".to_string()),
                duration_secs: 0.0,
            });
        }
    } else if config.articles_failed == 0 {
        // Files are known-good from CRC checks during yEnc decode, so the
        // expensive MD5 verification pass is skipped.
        //
        // PAR2-guided deobfuscation still has to run. Obfuscated posts arrive
        // with meaningless filenames whether or not an article failed, and the
        // PAR2 metadata is the only record of the real names. While this
        // rename lived inside the verify branch below, a *clean* download of
        // an obfuscated post was never deobfuscated: no archive was found, the
        // Extract stage reported "No archives found", and the job completed
        // with raw volumes on disk. A damaged download self-healed; a healthy
        // one did not (issue #87).
        info!("Skipping PAR2 verification — zero article failures (CRC-verified)");
        let start = Instant::now();
        let message = match rust_par2::parse(&par2_files[0]) {
            Ok(file_set) => {
                rename_to_par2_names(&file_set, job_dir);
                "Skipped — zero article failures (PAR2-guided rename applied)".to_string()
            }
            Err(e) => {
                debug!(error = %e, "PAR2 parse failed; skipping PAR2-guided deobfuscation");
                format!("Skipped — zero article failures (PAR2 parse failed: {e})")
            }
        };
        stages.push(StageResult {
            name: "Verify".to_string(),
            status: StageStatus::Skipped,
            message: Some(message),
            duration_secs: start.elapsed().as_secs_f64(),
        });
    } else {
        let verify_start = Instant::now();
        let _repair_permit = if let Some(resources) = resources {
            Some(resources.acquire_repair().await)
        } else {
            None
        };
        let index_par2 = par2_files[0].clone();

        match rust_par2::parse(&index_par2) {
            Ok(file_set) => {
                // PAR2-guided deobfuscation: if files on disk don't match
                // PAR2 expected names (common with obfuscated posts where
                // NZB subjects have readable names but PAR2 references
                // the original obfuscated filenames), rename them using
                // MD5-16k hash matching before verification runs. The
                // zero-failure branch above runs this too — verification is
                // skipped there, but deobfuscation must not be.
                rename_to_par2_names(&file_set, job_dir);

                // Run verify (and repair if needed) in a single spawn_blocking call.
                // This avoids two problems:
                //   1. CPU-intensive verify/repair doesn't block the async runtime
                //   2. VerifyResult (not Send) stays on one thread, so repair_from_verify
                //      can reuse it — no redundant second verification pass
                let dir = job_dir.to_path_buf();
                let verify_repair_result = tokio::task::spawn_blocking(move || {
                    let verify_result = rust_par2::verify(&file_set, &dir);

                    if verify_result.all_correct() {
                        VerifyRepairOutcome::AllCorrect {
                            intact_count: verify_result.intact.len(),
                        }
                    } else {
                        let intact = verify_result.intact.len();
                        let damaged = verify_result.damaged.len();
                        let missing = verify_result.missing.len();
                        let blocks_needed = verify_result.blocks_needed();
                        let blocks_available = verify_result.recovery_blocks_available;

                        info!(
                            intact,
                            damaged,
                            missing,
                            blocks_needed,
                            "Native PAR2 verify: damage detected, attempting native repair"
                        );

                        // Repair using the pre-computed verify result — no second verify pass
                        info!("Running native PAR2 repair (with pre-computed verify)");
                        let repair_result =
                            rust_par2::repair_from_verify(&file_set, &dir, &verify_result);

                        VerifyRepairOutcome::Damaged {
                            intact,
                            damaged,
                            missing,
                            blocks_needed,
                            blocks_available,
                            repair_result,
                        }
                    }
                })
                .await;

                let verify_duration = verify_start.elapsed().as_secs_f64();

                match verify_repair_result {
                    Ok(VerifyRepairOutcome::AllCorrect { intact_count }) => {
                        increment_counter("par2.verify_success");
                        info!(
                            files = intact_count,
                            duration_secs = verify_duration,
                            "Native PAR2 verify: all files correct"
                        );
                        stages.push(StageResult {
                            name: "Verify".to_string(),
                            status: StageStatus::Success,
                            message: Some(format!(
                                "All {intact_count} files correct (native verify, {verify_duration:.3}s)",
                            )),
                            duration_secs: verify_duration,
                        });
                    }
                    Ok(VerifyRepairOutcome::Damaged {
                        intact,
                        damaged,
                        missing,
                        blocks_needed,
                        blocks_available,
                        repair_result,
                    }) => {
                        // Push the verify stage result
                        stages.push(StageResult {
                            name: "Verify".to_string(),
                            status: StageStatus::Success,
                            message: Some(format!(
                                "{intact} intact, {damaged} damaged, {missing} missing — {blocks_needed} blocks needed (native verify)",
                            )),
                            duration_secs: verify_duration,
                        });

                        // Push the repair stage result
                        match repair_result {
                            Ok(result) => {
                                increment_counter(if result.success {
                                    "par2.repair_success"
                                } else {
                                    "par2.repair_failure"
                                });
                                info!(
                                    blocks_repaired = result.blocks_repaired,
                                    files_repaired = result.files_repaired,
                                    "Native PAR2 repair complete"
                                );
                                if !result.success {
                                    pipeline_ok = false;
                                    failure_code = Some(JobFailureCode::RepairFailed);
                                }
                                stages.push(StageResult {
                                    name: "Repair".to_string(),
                                    status: if result.success {
                                        StageStatus::Success
                                    } else {
                                        StageStatus::Failed
                                    },
                                    message: Some(result.message),
                                    duration_secs: verify_duration,
                                });
                            }
                            Err(e) => {
                                increment_counter("par2.repair_failure");
                                error!(
                                    error = %e,
                                    blocks_needed,
                                    blocks_available,
                                    damaged,
                                    missing,
                                    "Native PAR2 repair failed"
                                );
                                pipeline_ok = false;
                                failure_code = Some(JobFailureCode::RepairFailed);
                                stages.push(StageResult {
                                    name: "Repair".to_string(),
                                    status: StageStatus::Failed,
                                    message: Some(format!("Repair failed: {e}")),
                                    duration_secs: verify_duration,
                                });
                            }
                        }
                    }
                    Err(e) => {
                        error!(error = %e, "Verify/repair task panicked");
                        pipeline_ok = false;
                        failure_code = Some(JobFailureCode::RepairFailed);
                        stages.push(StageResult {
                            name: "Verify".to_string(),
                            status: StageStatus::Failed,
                            message: Some(format!("Verify task panicked: {e}")),
                            duration_secs: verify_duration,
                        });
                    }
                }
            }
            Err(e) => {
                // Native parse failed — try full repair path as fallback.
                debug!(error = %e, "Native PAR2 parse failed");
                let verify_duration = verify_start.elapsed().as_secs_f64();

                if config.articles_failed == 0 {
                    // No article failures, can't parse par2 — skip.
                    stages.push(StageResult {
                        name: "Verify".to_string(),
                        status: StageStatus::Skipped,
                        message: Some(format!(
                            "PAR2 parse failed ({e}), but zero article failures"
                        )),
                        duration_secs: verify_duration,
                    });
                } else {
                    // Articles failed and can't parse par2 — try repair with fresh parse.
                    stages.push(StageResult {
                        name: "Verify".to_string(),
                        status: StageStatus::Skipped,
                        message: Some(format!("PAR2 parse failed ({e}), attempting repair")),
                        duration_secs: verify_duration,
                    });

                    let repair_result = run_repair_stage(job_dir).await;
                    increment_counter(if repair_result.status == StageStatus::Failed {
                        "par2.repair_failure"
                    } else {
                        "par2.repair_success"
                    });
                    if repair_result.status == StageStatus::Failed {
                        pipeline_ok = false;
                        failure_code = Some(JobFailureCode::RepairFailed);
                    }
                    stages.push(repair_result);
                }
            }
        }
    }

    // ------------------------------------------------------------------
    // Stage 3: Extract
    // ------------------------------------------------------------------
    // Extraction is safe only after verification/repair succeeded (or source
    // content was already known-good). Confirmed unrepaired content damage
    // must not be mistaken for a successful archive.
    let should_extract = pipeline_ok;
    let mut extracted_archives = Vec::new();
    if should_extract {
        let output_dir = config.output_dir.as_deref().unwrap_or(job_dir);
        // Direct unpack has already extracted the outer archive, but its output
        // can itself contain archives. Scan that directory so a nested archive
        // cannot be silently left behind just because direct unpack was used.
        let source_dir = if config.skip_extract {
            info!("Outer extraction completed by direct unpack; checking for nested archives");
            output_dir
        } else {
            job_dir
        };
        let _extract_permit = if let Some(resources) = resources {
            Some(resources.acquire_extract().await)
        } else {
            None
        };
        let (result, processed_archives, extract_failure_code) = run_extract_stage(
            source_dir,
            output_dir,
            config.password.as_deref(),
            config.max_nested_archive_depth,
        )
        .await;
        extracted_archives = processed_archives;
        if result.status == StageStatus::Failed {
            pipeline_ok = false;
            failure_code = extract_failure_code.or(Some(JobFailureCode::ArchiveInvalid));
        } else if result.status == StageStatus::Success {
            // Extraction succeeded despite verify/repair failure — recover
            pipeline_ok = true;
        }
        stages.push(result);
    }

    // ------------------------------------------------------------------
    // Stage 4: Cleanup
    // ------------------------------------------------------------------
    if pipeline_ok && config.cleanup_after_extract {
        let cleanup_root = config.output_dir.as_deref().unwrap_or(job_dir);
        let result = run_cleanup_stage_with_rules(
            job_dir,
            cleanup_root,
            &extracted_archives,
            cleanup_patterns,
            unwanted_extensions,
        );
        stages.push(result);
    }

    let error = if pipeline_ok {
        None
    } else {
        // Collect failure messages from stages
        let msgs: Vec<String> = stages
            .iter()
            .filter(|s| s.status == StageStatus::Failed)
            .filter_map(|s| s.message.clone())
            .collect();
        Some(msgs.join("; "))
    };

    info!(
        success = pipeline_ok,
        stages = stages.len(),
        "Post-processing pipeline finished"
    );

    PostProcResult {
        success: pipeline_ok,
        stages,
        error,
        failure_code: if pipeline_ok {
            None
        } else {
            failure_code.or(Some(JobFailureCode::DownloadFailed))
        },
    }
}

// ---------------------------------------------------------------------------
// PAR2-guided deobfuscation
// ---------------------------------------------------------------------------

/// Rename files on disk to match PAR2 expected filenames.
///
/// Obfuscated Usenet posts often have readable names in NZB subjects but the
/// actual PAR2 metadata references the original obfuscated filenames. This
/// causes PAR2 verify to report all files as "missing" even though they exist.
///
/// This function matches files by MD5 hash of their first 16 KiB (which PAR2
/// stores for each file) and renames them to what PAR2 expects. This is the
/// same approach used by SABnzbd's `decode_par2()`.
fn rename_to_par2_names(file_set: &rust_par2::Par2FileSet, dir: &Path) {
    // Build a map of expected hash_16k → par2 filename
    let mut expected: std::collections::HashMap<[u8; 16], &str> = std::collections::HashMap::new();
    for par2_file in file_set.files.values() {
        expected.insert(par2_file.hash_16k, &par2_file.filename);
    }

    // Check if any PAR2-expected files are already present — if so, no renaming needed
    let any_match = file_set
        .files
        .values()
        .any(|f| dir.join(&f.filename).exists());
    if any_match {
        return;
    }

    // Scan all files on disk and try to match by hash_16k
    let entries: Vec<_> = match std::fs::read_dir(dir) {
        Ok(rd) => rd.filter_map(|e| e.ok()).collect(),
        Err(_) => return,
    };

    let mut renamed = 0u32;
    for entry in &entries {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let current_name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n.to_string(),
            None => continue,
        };

        // Skip PAR2 files themselves — they don't need renaming
        if current_name.to_lowercase().ends_with(".par2") {
            continue;
        }

        let hash = match rust_par2::compute_hash_16k(&path) {
            Ok(h) => h,
            Err(_) => continue,
        };

        if let Some(&par2_name) = expected.get(&hash)
            && current_name != par2_name
        {
            let new_path = dir.join(par2_name);
            if !new_path.exists() {
                if let Err(e) = std::fs::rename(&path, &new_path) {
                    warn!(
                        from = %current_name,
                        to = %par2_name,
                        "Failed to rename file to PAR2 expected name: {e}"
                    );
                } else {
                    renamed += 1;
                    debug!(
                        from = %current_name,
                        to = %par2_name,
                        "Renamed file to match PAR2 metadata"
                    );
                }
            }
        }
    }

    if renamed > 0 {
        info!(
            renamed,
            "PAR2-guided deobfuscation: renamed files to match PAR2 expected names"
        );
    }
}

// ---------------------------------------------------------------------------
// Internal stage runners
// ---------------------------------------------------------------------------

/// Repair stage when we don't have a pre-computed verify result.
/// Uses par2_repair which does its own parse + verify + repair.
async fn run_repair_stage(job_dir: &Path) -> StageResult {
    let start = Instant::now();
    let par2_files = find_par2_files(job_dir);

    if par2_files.is_empty() {
        return StageResult {
            name: "Repair".to_string(),
            status: StageStatus::Skipped,
            message: Some("No par2 files found".to_string()),
            duration_secs: start.elapsed().as_secs_f64(),
        };
    }

    let index_par2 = &par2_files[0];
    info!(file = %index_par2.display(), "Running native par2 repair");

    match par2_repair(index_par2).await {
        Ok(result) => {
            let status = if result.repaired || result.success {
                StageStatus::Success
            } else {
                StageStatus::Failed
            };

            StageResult {
                name: "Repair".to_string(),
                status,
                message: Some(result.message),
                duration_secs: start.elapsed().as_secs_f64(),
            }
        }
        Err(e) => {
            error!(error = %e, "par2 repair failed with error");
            StageResult {
                name: "Repair".to_string(),
                status: StageStatus::Failed,
                message: Some(format!("par2 repair error: {e}")),
                duration_secs: start.elapsed().as_secs_f64(),
            }
        }
    }
}

async fn run_extract_stage(
    source_dir: &Path,
    output_dir: &Path,
    password: Option<&str>,
    max_nested_archive_depth: u8,
) -> (StageResult, Vec<PathBuf>, Option<JobFailureCode>) {
    let start = Instant::now();
    let mut all_ok = true;
    let mut messages: Vec<String> = Vec::new();
    // The completed directory can pre-exist (for example a category
    // directory). Do not recurse into archives that were already there before
    // this job extracted anything. Direct unpack writes to a job-specific
    // output directory, so its initial archives are the intended input.
    let mut processed: HashSet<PathBuf> = if source_dir == output_dir {
        HashSet::new()
    } else {
        find_archives(output_dir)
            .into_iter()
            .map(|(_, path)| path)
            .collect()
    };
    let mut extracted_archives = Vec::new();
    let mut scan_dir = source_dir;
    let mut extracted_any = false;
    let mut failure_code = None;

    for depth in 0..=max_nested_archive_depth {
        let archives: Vec<_> = find_archives(scan_dir)
            .into_iter()
            .filter(|(_, path)| processed.insert(path.clone()))
            .collect();
        if archives.is_empty() {
            break;
        }

        extracted_any = true;
        extracted_archives.extend(archives.iter().map(|(_, path)| path.clone()));
        for (archive_type, path) in &archives {
            info!(depth, kind = %archive_type, file = %path.display(), "Extracting archive");
            let result = match archive_type {
                ArchiveType::Rar => extract_rar(path, output_dir, password).await,
                ArchiveType::SevenZip => extract_7z(path, output_dir, password).await,
                ArchiveType::Tar => extract_tar(path, output_dir).await,
                ArchiveType::Zip => extract_zip(path, output_dir).await,
            };

            match result {
                Ok(unpack_result) if unpack_result.success => {
                    messages.push(format!("depth {depth} {archive_type}: OK"));
                }
                Ok(unpack_result) => {
                    all_ok = false;
                    failure_code = Some(JobFailureCode::ArchiveInvalid);
                    let detail = unpack_result
                        .error_output
                        .trim()
                        .lines()
                        .find(|line| !line.trim().is_empty());
                    messages.push(match detail {
                        Some(detail) => format!("depth {depth} {archive_type}: failed ({detail})"),
                        None => format!("depth {depth} {archive_type}: failed"),
                    });
                }
                Err(e) => {
                    all_ok = false;
                    failure_code = Some(if e.downcast_ref::<ArchivePasswordRequired>().is_some() {
                        JobFailureCode::ArchivePasswordRequired
                    } else {
                        JobFailureCode::ArchiveInvalid
                    });
                    error!(depth, kind = %archive_type, file = %path.display(), error = %e, "Extraction error");
                    messages.push(format!("depth {depth} {archive_type}: {e}"));
                }
            }
        }

        if !all_ok {
            break;
        }
        scan_dir = output_dir;
    }

    if all_ok
        && !find_archives(scan_dir)
            .into_iter()
            .all(|(_, path)| processed.contains(&path))
    {
        all_ok = false;
        failure_code = Some(JobFailureCode::ArchiveInvalid);
        messages.push(format!(
            "nested archive depth limit ({max_nested_archive_depth}) reached; source files retained"
        ));
    }

    if !extracted_any {
        info!("No archives found — skipping extraction");
        return (
            StageResult {
                name: "Extract".to_string(),
                status: StageStatus::Skipped,
                message: Some("No archives found".to_string()),
                duration_secs: start.elapsed().as_secs_f64(),
            },
            Vec::new(),
            None,
        );
    }

    (
        StageResult {
            name: "Extract".to_string(),
            status: if all_ok {
                StageStatus::Success
            } else {
                StageStatus::Failed
            },
            message: Some(messages.join("; ")),
            duration_secs: start.elapsed().as_secs_f64(),
        },
        extracted_archives,
        failure_code,
    )
}

#[cfg(test)]
fn run_cleanup_stage(job_dir: &Path, extracted_archives: &[PathBuf]) -> StageResult {
    run_cleanup_stage_with_rules(job_dir, job_dir, extracted_archives, &[], &[])
}

fn wildcard_match(pattern: &str, value: &str) -> bool {
    let (mut pattern_index, mut value_index) = (0usize, 0usize);
    let mut star = None;
    let mut star_value = 0usize;
    let pattern = pattern.as_bytes();
    let value = value.as_bytes();

    while value_index < value.len() {
        if pattern_index < pattern.len()
            && (pattern[pattern_index] == value[value_index] || pattern[pattern_index] == b'?')
        {
            pattern_index += 1;
            value_index += 1;
        } else if pattern_index < pattern.len() && pattern[pattern_index] == b'*' {
            star = Some(pattern_index);
            pattern_index += 1;
            star_value = value_index;
        } else if let Some(star_index) = star {
            pattern_index = star_index + 1;
            star_value += 1;
            value_index = star_value;
        } else {
            return false;
        }
    }
    while pattern_index < pattern.len() && pattern[pattern_index] == b'*' {
        pattern_index += 1;
    }
    pattern_index == pattern.len()
}

fn run_cleanup_stage_with_rules(
    job_dir: &Path,
    cleanup_root: &Path,
    extracted_archives: &[PathBuf],
    cleanup_patterns: &[String],
    unwanted_extensions: &[String],
) -> StageResult {
    let start = Instant::now();
    let mut files = find_cleanup_files(job_dir);
    files.extend(
        extracted_archives
            .iter()
            .filter(|path| {
                std::fs::symlink_metadata(path)
                    .map(|metadata| metadata.file_type().is_file())
                    .unwrap_or(false)
            })
            .cloned(),
    );

    let normalized_extensions: Vec<String> = unwanted_extensions
        .iter()
        .map(|extension| {
            let extension = extension.trim().to_ascii_lowercase();
            if extension.starts_with('.') {
                extension
            } else {
                format!(".{extension}")
            }
        })
        .filter(|extension| extension.len() > 1)
        .collect();
    let patterns: Vec<String> = cleanup_patterns
        .iter()
        .map(|pattern| pattern.replace('\\', "/").to_ascii_lowercase())
        .filter(|pattern| !pattern.is_empty())
        .collect();
    for root in [job_dir, cleanup_root] {
        for entry in walkdir::WalkDir::new(root).into_iter().flatten() {
            let path = entry.path();
            let Ok(metadata) = std::fs::symlink_metadata(path) else {
                continue;
            };
            if !metadata.file_type().is_file() {
                continue;
            }
            let Ok(relative) = path.strip_prefix(root) else {
                continue;
            };
            let relative = relative.to_string_lossy().replace('\\', "/");
            let filename = path
                .file_name()
                .map(|name| name.to_string_lossy().to_string())
                .unwrap_or_default();
            if normalized_extensions
                .iter()
                .any(|extension| filename.to_ascii_lowercase().ends_with(extension))
                || patterns.iter().any(|pattern| {
                    wildcard_match(pattern, &relative.to_ascii_lowercase())
                        || wildcard_match(pattern, &filename.to_ascii_lowercase())
                })
            {
                files.push(path.to_path_buf());
            }
        }
    }
    files.sort();
    files.dedup();

    if files.is_empty() {
        return StageResult {
            name: "Cleanup".to_string(),
            status: StageStatus::Skipped,
            message: Some("No files to clean up".to_string()),
            duration_secs: start.elapsed().as_secs_f64(),
        };
    }

    let mut removed = 0u32;
    let mut errors = 0u32;

    for path in &files {
        let under_allowed_root = path.starts_with(job_dir) || path.starts_with(cleanup_root);
        let is_regular_file = std::fs::symlink_metadata(path)
            .map(|metadata| metadata.file_type().is_file())
            .unwrap_or(false);
        if !under_allowed_root || !is_regular_file {
            warn!(file = %path.display(), "Skipping cleanup path outside job roots or through a link");
            errors += 1;
            continue;
        }
        match std::fs::remove_file(path) {
            Ok(()) => {
                removed += 1;
            }
            Err(e) => {
                warn!(file = %path.display(), error = %e, "Failed to remove cleanup file");
                errors += 1;
            }
        }
    }

    let status = if errors == 0 {
        StageStatus::Success
    } else {
        StageStatus::Failed
    };

    StageResult {
        name: "Cleanup".to_string(),
        status,
        message: Some(format!("Removed {removed} files, {errors} errors")),
        duration_secs: start.elapsed().as_secs_f64(),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Write;

    fn make_test_dir(files: &[&str]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for name in files {
            let path = dir.path().join(name);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(&path, b"").unwrap();
        }
        dir
    }

    #[test]
    fn test_post_proc_result_default() {
        let result = PostProcResult {
            success: true,
            stages: vec![],
            error: None,
            failure_code: None,
        };
        assert!(result.success);
        assert!(result.stages.is_empty());
        assert!(result.error.is_none());
        assert!(result.failure_code.is_none());
    }

    #[test]
    fn test_config_default() {
        let config = PostProcConfig::default();
        assert!(config.cleanup_after_extract);
        assert!(config.output_dir.is_none());
        assert_eq!(config.articles_failed, 0);
    }

    #[tokio::test]
    async fn test_pipeline_no_files() {
        // An empty directory should skip all stages
        let dir = make_test_dir(&[]);
        let config = PostProcConfig::default();
        let result = run_pipeline(dir.path(), &config).await;

        assert!(result.success, "Pipeline should succeed for empty dir");

        // Verify should be skipped (no par2), Extract should be skipped (no archives)
        let verify_stage = result.stages.iter().find(|s| s.name == "Verify");
        assert!(verify_stage.is_some(), "Verify stage should be present");
        assert_eq!(verify_stage.unwrap().status, StageStatus::Skipped);

        let extract_stage = result.stages.iter().find(|s| s.name == "Extract");
        assert!(extract_stage.is_some(), "Extract stage should be present");
        assert_eq!(extract_stage.unwrap().status, StageStatus::Skipped);
    }

    #[tokio::test]
    async fn test_pipeline_only_text_files() {
        let dir = make_test_dir(&["readme.txt", "info.nfo"]);
        let config = PostProcConfig::default();
        let result = run_pipeline(dir.path(), &config).await;

        assert!(result.success);
        // All stages should be skipped
        for stage in &result.stages {
            assert_eq!(
                stage.status,
                StageStatus::Skipped,
                "Stage '{}' should be skipped",
                stage.name
            );
        }
    }

    #[test]
    fn test_cleanup_removes_files() {
        let dir = make_test_dir(&[
            "movie.par2",
            "movie.vol00+01.par2",
            "movie.rar",
            "movie.r00",
            "movie.mkv", // should NOT be removed
        ]);

        let result = run_cleanup_stage(dir.path(), &[]);
        assert_eq!(result.status, StageStatus::Success);

        // movie.mkv should still exist
        assert!(dir.path().join("movie.mkv").exists());
        // par2 and rar files should be gone
        assert!(!dir.path().join("movie.par2").exists());
        assert!(!dir.path().join("movie.vol00+01.par2").exists());
        assert!(!dir.path().join("movie.rar").exists());
        assert!(!dir.path().join("movie.r00").exists());
    }

    fn write_zip(path: &Path, entries: &[(&str, &[u8])]) {
        let file = fs::File::create(path).unwrap();
        let mut writer = zip::ZipWriter::new(file);
        for (name, contents) in entries {
            writer
                .start_file(*name, zip::write::SimpleFileOptions::default())
                .unwrap();
            writer.write_all(contents).unwrap();
        }
        writer.finish().unwrap();
    }

    #[tokio::test]
    async fn nested_zip_archives_are_extracted_recursively() {
        let source = tempfile::tempdir().unwrap();
        let output = tempfile::tempdir().unwrap();
        let inner = source.path().join("inner.zip");
        write_zip(&inner, &[("payload.txt", b"nested payload")]);
        let outer = source.path().join("outer.zip");
        write_zip(&outer, &[("inner.zip", &fs::read(&inner).unwrap())]);
        fs::remove_file(inner).unwrap();

        let (result, _, failure_code) =
            run_extract_stage(source.path(), output.path(), None, 1).await;

        assert_eq!(result.status, StageStatus::Success, "{result:?}");
        assert_eq!(failure_code, None);
        assert_eq!(
            fs::read(output.path().join("payload.txt")).unwrap(),
            b"nested payload"
        );
    }

    #[tokio::test]
    async fn nested_archive_depth_limit_fails_without_discarding_inner_archive() {
        let source = tempfile::tempdir().unwrap();
        let output = tempfile::tempdir().unwrap();
        let inner = source.path().join("inner.zip");
        write_zip(&inner, &[("payload.txt", b"nested payload")]);
        let outer = source.path().join("outer.zip");
        write_zip(&outer, &[("inner.zip", &fs::read(&inner).unwrap())]);
        fs::remove_file(inner).unwrap();

        let (result, _, failure_code) =
            run_extract_stage(source.path(), output.path(), None, 0).await;

        assert_eq!(result.status, StageStatus::Failed, "{result:?}");
        assert_eq!(failure_code, Some(JobFailureCode::ArchiveInvalid));
        assert!(output.path().join("inner.zip").exists());
        assert!(!output.path().join("payload.txt").exists());
    }

    #[tokio::test]
    async fn recursive_cleanup_removes_nested_archives_from_output_directory() {
        let source = tempfile::tempdir().unwrap();
        let output = tempfile::tempdir().unwrap();
        let inner = source.path().join("inner.zip");
        write_zip(&inner, &[("payload.txt", b"nested payload")]);
        let outer = source.path().join("outer.zip");
        write_zip(&outer, &[("inner.zip", &fs::read(&inner).unwrap())]);
        fs::remove_file(inner).unwrap();

        let config = PostProcConfig {
            output_dir: Some(output.path().to_path_buf()),
            ..Default::default()
        };
        let result = run_pipeline(source.path(), &config).await;

        assert!(result.success, "{result:?}");
        assert_eq!(
            fs::read(output.path().join("payload.txt")).unwrap(),
            b"nested payload"
        );
        assert!(!output.path().join("inner.zip").exists());
        assert!(!source.path().join("outer.zip").exists());
    }

    #[tokio::test]
    async fn cleanup_keeps_unrelated_archives_in_the_output_directory() {
        let source = tempfile::tempdir().unwrap();
        let output = tempfile::tempdir().unwrap();
        let inner = source.path().join("inner.zip");
        write_zip(&inner, &[("payload.txt", b"nested payload")]);
        let outer = source.path().join("outer.zip");
        write_zip(&outer, &[("inner.zip", &fs::read(&inner).unwrap())]);
        fs::remove_file(inner).unwrap();
        let unrelated = output.path().join("keep-me.zip");
        write_zip(&unrelated, &[("unrelated.txt", b"keep")]);

        let config = PostProcConfig {
            output_dir: Some(output.path().to_path_buf()),
            ..Default::default()
        };
        let result = run_pipeline(source.path(), &config).await;

        assert!(result.success, "{result:?}");
        assert!(unrelated.exists());
        assert!(!output.path().join("inner.zip").exists());
    }

    #[tokio::test]
    async fn direct_unpack_still_extracts_nested_archives() {
        let job_dir = tempfile::tempdir().unwrap();
        let output = tempfile::tempdir().unwrap();
        let nested = output.path().join("nested.zip");
        write_zip(&nested, &[("payload.txt", b"nested payload")]);

        let config = PostProcConfig {
            output_dir: Some(output.path().to_path_buf()),
            skip_extract: true,
            ..Default::default()
        };
        let result = run_pipeline(job_dir.path(), &config).await;

        assert!(result.success, "{result:?}");
        assert_eq!(
            fs::read(output.path().join("payload.txt")).unwrap(),
            b"nested payload"
        );
        assert!(!nested.exists());
    }

    #[tokio::test]
    async fn test_pipeline_stage_order() {
        // With an empty dir, we can at least verify the stages that run
        // are in the correct order.
        let dir = make_test_dir(&[]);
        let config = PostProcConfig {
            cleanup_after_extract: false,
            ..Default::default()
        };
        let result = run_pipeline(dir.path(), &config).await;

        // Should have Verify and Extract (both skipped). Cleanup is disabled.
        let stage_names: Vec<&str> = result.stages.iter().map(|s| s.name.as_str()).collect();
        assert!(stage_names.contains(&"Verify"), "Should have Verify stage");
        assert!(
            stage_names.contains(&"Extract"),
            "Should have Extract stage"
        );

        // Verify should come before Extract
        let verify_idx = stage_names.iter().position(|&n| n == "Verify").unwrap();
        let extract_idx = stage_names.iter().position(|&n| n == "Extract").unwrap();
        assert!(
            verify_idx < extract_idx,
            "Verify ({verify_idx}) should come before Extract ({extract_idx})"
        );
    }

    #[tokio::test]
    async fn test_pipeline_skips_verify_with_zero_failures() {
        // With par2 files present and articles_failed == 0, verify is skipped
        // because files are known-good from CRC checks during yEnc decode.
        let dir = make_test_dir(&["movie.par2", "movie.vol00+01.par2", "movie.mkv"]);
        let config = PostProcConfig {
            cleanup_after_extract: false,
            ..Default::default()
        };
        let result = run_pipeline(dir.path(), &config).await;
        assert!(result.success);

        let verify_stage = result.stages.iter().find(|s| s.name == "Verify").unwrap();
        assert_eq!(
            verify_stage.status,
            StageStatus::Skipped,
            "Verify should be skipped when articles_failed == 0"
        );
        assert!(
            verify_stage
                .message
                .as_deref()
                .unwrap_or("")
                .contains("zero article failures"),
            "Skip message should indicate zero failures"
        );
    }

    #[tokio::test]
    async fn test_pipeline_no_par2_with_content_failures_is_terminal() {
        // Confirmed source damage without PAR2 cannot be repaired and must
        // not continue into extraction.
        let dir = make_test_dir(&["movie.mkv"]);
        let config = PostProcConfig {
            cleanup_after_extract: false,
            articles_failed: 5,
            content_articles_failed: 5,
            ..Default::default()
        };
        let result = run_pipeline(dir.path(), &config).await;
        assert!(!result.success);
        assert_eq!(
            result.failure_code,
            Some(JobFailureCode::ArticlesUnavailable)
        );

        let verify_stage = result.stages.iter().find(|s| s.name == "Verify").unwrap();
        assert_eq!(
            verify_stage.status,
            StageStatus::Failed,
            "No-PAR content damage is unrecoverable"
        );
        assert!(
            verify_stage
                .message
                .as_deref()
                .unwrap_or("")
                .contains("no PAR2 recovery set"),
            "Failure should explain that recovery data is unavailable"
        );
        assert!(result.stages.iter().all(|stage| stage.name != "Extract"));
    }

    #[tokio::test]
    async fn test_pipeline_runs_verify_then_repair_when_failures() {
        // With par2 files and articles_failed > 0, native verify should run first.
        // Since these are dummy empty par2 files, native parse will fail and
        // the pipeline should fall back to par2cmdline for repair.
        let dir = make_test_dir(&["movie.par2", "movie.vol00+01.par2", "movie.mkv"]);
        let config = PostProcConfig {
            cleanup_after_extract: false,
            articles_failed: 3,
            ..Default::default()
        };
        let result = run_pipeline(dir.path(), &config).await;

        // Should always have a Verify stage now (native par2 verify runs first)
        let stage_names: Vec<&str> = result.stages.iter().map(|s| s.name.as_str()).collect();
        assert!(
            stage_names.contains(&"Verify"),
            "Should have Verify stage (native par2), got: {stage_names:?}"
        );
        // Repair stage should also be present since dummy par2 files
        // will either fail to parse or report damage
        assert!(
            stage_names.contains(&"Repair"),
            "Should have Repair stage when articles_failed > 0, got: {stage_names:?}"
        );
    }

    // -----------------------------------------------------------------------
    // PAR2-guided deobfuscation tests
    // -----------------------------------------------------------------------

    /// Helper to build a Par2FileSet with given filename→content mappings.
    /// Computes hash_16k by writing content to temp files and using rust_par2.
    fn make_par2_file_set(tmp: &Path, files: &[(&str, &[u8])]) -> rust_par2::Par2FileSet {
        use rust_par2::{Par2File, Par2FileSet};
        let mut map = std::collections::HashMap::new();
        let mut file_order = Vec::with_capacity(files.len());
        for (i, (name, content)) in files.iter().enumerate() {
            // Write to temp file so we can use compute_hash_16k
            let tmp_path = tmp.join(format!("_par2_tmp_{i}"));
            fs::write(&tmp_path, content).unwrap();
            let hash_16k = rust_par2::compute_hash_16k(&tmp_path).unwrap();
            let _ = fs::remove_file(&tmp_path);

            let file_id = [i as u8; 16];
            file_order.push(file_id);
            map.insert(
                file_id,
                Par2File {
                    file_id,
                    hash: [0u8; 16],
                    hash_16k,
                    size: content.len() as u64,
                    filename: name.to_string(),
                    slices: vec![],
                },
            );
        }
        Par2FileSet {
            recovery_set_id: [0u8; 16],
            slice_size: 16384,
            file_order,
            files: map,
            recovery_block_count: 0,
            creator: None,
        }
    }

    #[test]
    fn test_rename_to_par2_names_renames_mismatched() {
        let dir = tempfile::tempdir().unwrap();

        // Write files with "readable" names
        let content_a = b"AAAA test data for part01";
        let content_b = b"BBBB test data for part02";
        fs::write(dir.path().join("Movie.Name.part01.rar"), content_a).unwrap();
        fs::write(dir.path().join("Movie.Name.part02.rar"), content_b).unwrap();

        // PAR2 expects obfuscated names with the same content
        let file_set = make_par2_file_set(
            dir.path(),
            &[
                ("xY7kQ3.part01.rar", content_a),
                ("xY7kQ3.part02.rar", content_b),
            ],
        );

        rename_to_par2_names(&file_set, dir.path());

        // Files should be renamed to PAR2 expected names
        assert!(
            dir.path().join("xY7kQ3.part01.rar").exists(),
            "part01 should be renamed to obfuscated name"
        );
        assert!(
            dir.path().join("xY7kQ3.part02.rar").exists(),
            "part02 should be renamed to obfuscated name"
        );
        assert!(
            !dir.path().join("Movie.Name.part01.rar").exists(),
            "old readable name should no longer exist"
        );
        assert!(
            !dir.path().join("Movie.Name.part02.rar").exists(),
            "old readable name should no longer exist"
        );
    }

    #[test]
    fn test_rename_to_par2_names_skips_when_already_correct() {
        let dir = tempfile::tempdir().unwrap();

        // Files already have the PAR2-expected names
        let content = b"test data already correct";
        fs::write(dir.path().join("xY7kQ3.part01.rar"), content).unwrap();

        let file_set = make_par2_file_set(dir.path(), &[("xY7kQ3.part01.rar", content)]);

        rename_to_par2_names(&file_set, dir.path());

        // File should still exist with same name (no rename needed)
        assert!(dir.path().join("xY7kQ3.part01.rar").exists());
    }

    #[test]
    fn test_rename_to_par2_names_skips_par2_files() {
        let dir = tempfile::tempdir().unwrap();

        // PAR2 files should not be renamed even if hash matches
        let content = b"par2 file content";
        fs::write(dir.path().join("Movie.Name.par2"), content).unwrap();
        fs::write(dir.path().join("Movie.Name.part01.rar"), b"rar data").unwrap();

        let file_set = make_par2_file_set(dir.path(), &[("obfuscated.par2", content)]);

        rename_to_par2_names(&file_set, dir.path());

        // PAR2 file should NOT be renamed
        assert!(
            dir.path().join("Movie.Name.par2").exists(),
            "PAR2 files should be skipped"
        );
    }

    #[test]
    fn test_rename_to_par2_names_no_match() {
        let dir = tempfile::tempdir().unwrap();

        // File content doesn't match any PAR2 entry
        fs::write(dir.path().join("Movie.Name.part01.rar"), b"unrelated data").unwrap();

        let file_set = make_par2_file_set(
            dir.path(),
            &[("xY7kQ3.part01.rar", b"different data" as &[u8])],
        );

        rename_to_par2_names(&file_set, dir.path());

        // File should remain with original name (no hash match)
        assert!(dir.path().join("Movie.Name.part01.rar").exists());
        assert!(!dir.path().join("xY7kQ3.part01.rar").exists());
    }
}
