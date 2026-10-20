//! Queue manager — coordinates downloads across the application.
//!
//! The QueueManager owns the list of active NzbJobs, manages the download
//! engine instances, and exposes a thread-safe API for the HTTP handlers
//! to interact with.

use std::collections::{HashMap, HashSet};
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::sync::{broadcast, mpsc, watch};
use tracing::{debug, error, info, warn};

use crate::nzb_core::config::{
    CategoryConfig, DuplicatePolicy, NotificationConfig, ServerConfig, normalize_history_retention,
};
use crate::nzb_core::db::{Database, HistoryFilter, HistorySummary, nzb_digest};
use crate::nzb_core::models::*;
use crate::nzb_core::nzb_parser;
use nzb_postproc::{
    PostProcConfig, PostProcLimits, PostProcResourcePool, PostProcResourceSnapshot,
    has_usable_output, parse_rar_volume, run_pipeline_with_cleanup, run_repair_pipeline,
};

use crate::damage_recorder::LedgerRecorder;
use crate::direct_unpack::DirectUnpacker;
use crate::log_buffer::LogBuffer;
use crate::notifications::{NotificationEvent, NotificationKind, Notifier};
use nzb_dispatch::{
    BandwidthConfig, BandwidthLimiter, DispatchEngine, DispatchHandle, ProgressUpdate,
};

fn cleanup_terminal_work_dir(
    job_id: &str,
    work_dir: &std::path::Path,
    final_status: JobStatus,
    retain_for_retry: bool,
) {
    if !work_dir.exists() {
        return;
    }

    let cleanup_result = match final_status {
        // Failed downloads can be retried from their retained NZB history, so
        // retaining raw articles only leaks disk without improving recovery.
        JobStatus::Failed if retain_for_retry => {
            info!(job_id, work_dir = %work_dir.display(), "Retaining partial job files for missing-article retry");
            return;
        }
        JobStatus::Failed => std::fs::remove_dir_all(work_dir),
        // A successful job must not lose files if an output move failed. Only
        // remove the directory after the move/pipeline has left it empty.
        JobStatus::Completed => match std::fs::read_dir(work_dir) {
            Ok(mut entries) => {
                if entries.next().is_none() {
                    std::fs::remove_dir(work_dir)
                } else {
                    warn!(
                        job_id,
                        work_dir = %work_dir.display(),
                        "Retaining non-empty completed work directory after output move"
                    );
                    return;
                }
            }
            Err(e) => {
                warn!(
                    job_id,
                    work_dir = %work_dir.display(),
                    "Unable to inspect completed work directory for safe cleanup: {e}"
                );
                return;
            }
        },
        _ => return,
    };

    match cleanup_result {
        Ok(()) => info!(job_id, work_dir = %work_dir.display(), "Removed terminal work directory"),
        Err(e) => warn!(
            job_id,
            work_dir = %work_dir.display(),
            "Failed to remove terminal work directory: {e}"
        ),
    }
}

/// Upper bound on `<name>.<n>` suffixes tried when the complete directory
/// already holds folders with a job's name.
const MAX_OUTPUT_DIR_SUFFIX: u32 = 9999;

/// How long graceful shutdown waits for the per-job progress listeners to
/// drain events the worker pool queued before it stopped. Listeners still
/// running at the deadline are aborted so shutdown stays bounded.
const PROGRESS_DRAIN_TIMEOUT: Duration = Duration::from_secs(5);

/// Atomically reserve a complete-directory folder derived from `base`.
///
/// `base` is tried first, then `<name>.1`, `<name>.2`, ... like SABnzbd.
/// `create_dir` either creates the folder or fails with `AlreadyExists`, so
/// two jobs finishing at the same time can never reserve the same folder.
fn claim_unique_output_dir(base: &std::path::Path) -> std::io::Result<std::path::PathBuf> {
    let (Some(parent), Some(name)) = (base.parent(), base.file_name()) else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "output directory has no parent or name",
        ));
    };
    std::fs::create_dir_all(parent)?;
    for suffix in 0..=MAX_OUTPUT_DIR_SUFFIX {
        let candidate = if suffix == 0 {
            base.to_path_buf()
        } else {
            let mut suffixed = name.to_os_string();
            suffixed.push(format!(".{suffix}"));
            parent.join(suffixed)
        };
        match std::fs::create_dir(&candidate) {
            Ok(()) => return Ok(candidate),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "no free output directory name",
    ))
}

/// Reserve a unique complete-directory folder for a job that has not reserved
/// one yet, recording the final path in `output_dir`.
fn claim_output_dir_fields(job_id: &str, output_dir: &mut std::path::PathBuf, claimed: &mut bool) {
    if *claimed {
        return;
    }
    match claim_unique_output_dir(output_dir) {
        Ok(dir) => {
            if dir != *output_dir {
                info!(
                    job_id,
                    requested = %output_dir.display(),
                    output_dir = %dir.display(),
                    "Output folder already exists; using a unique folder name"
                );
            }
            *output_dir = dir;
            *claimed = true;
        }
        Err(e) => warn!(
            job_id,
            output_dir = %output_dir.display(),
            "Unable to reserve a unique output folder: {e}"
        ),
    }
}

/// Accept the output directory an earlier attempt of the same job reserved
/// (an interrupted post-processing run, or a direct unpack before a pause),
/// provided it is still a real directory named `<base>` or `<base>.<n>`
/// beside the job's derived target.
/// Sweep damage-ledger rows that no queued or listed job can use (WI-1868).
/// Runs when a job leaves the queue and at startup, so expired holes and the
/// rows of removed jobs don't pile up.
fn prune_damage_ledger(db: &Database) {
    match db.ledger_prune(Utc::now()) {
        Ok(0) => {}
        Ok(n) => debug!(rows = n, "Pruned damage-ledger rows"),
        Err(e) => warn!("Failed to prune the damage ledger: {e}"),
    }
}

fn restore_claimed_output_dir(job: &mut NzbJob, checkpoint: &JobCheckpoint) -> bool {
    let Some(claimed) = checkpoint.output_dir.as_ref() else {
        return false;
    };
    let same_parent = claimed.parent().is_some() && claimed.parent() == job.output_dir.parent();
    let same_base = match (claimed.file_name(), job.output_dir.file_name()) {
        (Some(claimed), Some(base)) => {
            let (claimed, base) = (claimed.to_string_lossy(), base.to_string_lossy());
            claimed == base
                || claimed
                    .strip_prefix(&*base)
                    .and_then(|rest| rest.strip_prefix('.'))
                    .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
        }
        _ => false,
    };
    let is_dir = std::fs::symlink_metadata(claimed)
        .map(|metadata| metadata.file_type().is_dir())
        .unwrap_or(false);
    if same_parent && same_base && is_dir {
        job.output_dir = claimed.clone();
        true
    } else {
        false
    }
}

/// Resolve `candidate` to a real directory that is a direct child of the
/// canonical incomplete root. Symlinks, files, the root itself, and anything
/// that resolves outside the root are refused.
fn incomplete_child_dir(
    root: &std::path::Path,
    candidate: &std::path::Path,
) -> Option<std::path::PathBuf> {
    let metadata = std::fs::symlink_metadata(candidate).ok()?;
    if !metadata.file_type().is_dir() {
        return None;
    }
    let resolved = std::fs::canonicalize(candidate).ok()?;
    (resolved.parent() == Some(root)).then_some(resolved)
}

/// Whether `name` has the shape of a job id (a hyphenated UUID). Every job
/// work directory is `incomplete/<job id>`, so the startup sweep only
/// considers such names and never touches anything else a user keeps there.
fn is_job_id_dir_name(name: &std::ffi::OsStr) -> bool {
    let Some(name) = name.to_str() else {
        return false;
    };
    name.len() == 36
        && name.char_indices().all(|(index, ch)| match index {
            8 | 13 | 18 | 23 => ch == '-',
            _ => ch.is_ascii_hexdigit(),
        })
}

/// Total size of the regular files under `path`, without following symlinks.
fn tree_size(path: &std::path::Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(path) else {
        return 0;
    };
    entries
        .flatten()
        .map(|entry| match std::fs::symlink_metadata(entry.path()) {
            Ok(metadata) if metadata.is_dir() => tree_size(&entry.path()),
            Ok(metadata) if metadata.is_file() => metadata.len(),
            _ => 0,
        })
        .sum()
}

/// The work directory recorded in a history row's retry checkpoint, read
/// without materialising the per-article outcomes.
fn retry_checkpoint_work_dir(retry_data: &[u8]) -> Option<std::path::PathBuf> {
    #[derive(Deserialize)]
    struct WorkDirOnly {
        #[serde(default)]
        work_dir: Option<std::path::PathBuf>,
    }
    serde_json::from_slice::<WorkDirOnly>(retry_data)
        .ok()?
        .work_dir
}

/// `(fetched, available)` PAR2 recovery volumes for a job whose download
/// ended with the volume files `held` still unfetched, or `None` when the NZB
/// carries no volumes.
fn par2_volume_counts(job: &NzbJob, held: &[String]) -> Option<(u32, u32)> {
    let volumes = job.files.iter().filter(|file| {
        let lower = file.filename.to_lowercase();
        lower.ends_with(".par2") && lower.contains(".vol")
    });
    let (available, held) = volumes.fold((0_u32, 0_u32), |(available, held_n), file| {
        (available + 1, held_n + u32::from(held.contains(&file.id)))
    });
    (available > 0).then(|| (available - held, available))
}

/// Per-server totals. The `today_*`, `week_*` and `month_*` fields are rolling
/// windows (the last 24 hours, 7 days and 30 days before the request), not
/// calendar periods; the UI labels them "24 hours", "7 days" and "30 days".
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct ServerStatsData {
    pub server_id: String,
    pub server_name: String,
    pub total_bytes: u64,
    pub today_bytes: u64,
    pub week_bytes: u64,
    pub month_bytes: u64,
    pub total_ok: usize,
    pub today_ok: usize,
    pub week_ok: usize,
    pub month_ok: usize,
    pub total_fail: usize,
    pub today_fail: usize,
    pub week_fail: usize,
    pub month_fail: usize,
    pub last_active: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, utoipa::ToSchema)]
pub struct StatisticsPeriodData {
    pub downloads: usize,
    pub completed: usize,
    pub failed: usize,
    pub bytes_downloaded: u64,
    pub total_duration_secs: f64,
    pub average_speed_bps: u64,
    pub fastest_download_bps: u64,
    pub news_server_hits: usize,
    pub articles_served: usize,
    pub articles_missing: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct DailyStatisticsData {
    pub date: String,
    #[serde(flatten)]
    pub totals: StatisticsPeriodData,
}

#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct GlobalStatisticsData {
    pub generated_at: DateTime<Utc>,
    pub lifetime: StatisticsPeriodData,
    /// Rolling last 24 hours before `generated_at` -- not the current
    /// calendar day, so it need not equal today's `daily` bucket.
    pub today: StatisticsPeriodData,
    /// Rolling last 7 days before `generated_at`.
    pub week: StatisticsPeriodData,
    /// Rolling last 30 days before `generated_at`.
    pub month: StatisticsPeriodData,
    pub servers: Vec<ServerStatsData>,
    /// Per-day buckets keyed by UTC calendar date (`YYYY-MM-DD`).
    pub daily: Vec<DailyStatisticsData>,
}

/// Shortest id prefix accepted as a job reference. SABnzbd-style `nzo_id`s
/// carry the first 12 characters of the job id, so anything shorter is too
/// ambiguous to act on.
pub const MIN_JOB_ID_PREFIX_LEN: usize = 12;

/// Whether `requested` refers to the job `id`: an exact match, or a prefix at
/// least [`MIN_JOB_ID_PREFIX_LEN`] long. An empty reference never matches --
/// `str::starts_with("")` is always true, which let an empty or bare
/// `SABnzbd_nzo_` id act on whichever job happened to be first.
pub fn job_id_matches(id: &str, requested: &str) -> bool {
    !requested.is_empty()
        && (id == requested
            || (requested.len() >= MIN_JOB_ID_PREFIX_LEN && id.starts_with(requested)))
}

/// Get free disk space for a path (returns 0 on error).
fn get_disk_free(path: &std::path::Path) -> u64 {
    disk_space(path).0
}

/// Free and total bytes of the filesystem holding `path` (or its nearest
/// existing ancestor). Returns `(0, 0)` on error.
pub(crate) fn disk_space(path: &std::path::Path) -> (u64, u64) {
    let mut candidate = path.to_path_buf();
    while !candidate.exists() {
        if !candidate.pop() {
            return (0, 0);
        }
    }
    #[cfg(unix)]
    {
        use std::ffi::CString;
        use std::mem::MaybeUninit;
        let c_path = match CString::new(candidate.to_string_lossy().as_bytes()) {
            Ok(p) => p,
            Err(_) => return (0, 0),
        };
        unsafe {
            let mut stat = MaybeUninit::<libc::statvfs>::uninit();
            if libc::statvfs(c_path.as_ptr(), stat.as_mut_ptr()) == 0 {
                let stat = stat.assume_init();
                #[allow(clippy::unnecessary_cast)] // u32 on macOS, u64 on Linux
                return (
                    stat.f_bavail as u64 * stat.f_frsize as u64,
                    stat.f_blocks as u64 * stat.f_frsize as u64,
                );
            }
        }
        (0, 0)
    }
    #[cfg(not(unix))]
    {
        let _ = candidate;
        (0, 0)
    }
}

fn disk_space_available(threshold: u64, paths: &[&std::path::Path]) -> bool {
    threshold == 0 || paths.iter().all(|path| get_disk_free(path) >= threshold)
}

#[derive(Debug, Clone, Default)]
struct PostProcScriptConfig {
    scripts_dir: Option<std::path::PathBuf>,
    success: Option<std::path::PathBuf>,
    failure: Option<std::path::PathBuf>,
    timeout: Duration,
    max_output_bytes: usize,
}

fn resolve_script_path(
    scripts_dir: Option<&std::path::Path>,
    configured: &std::path::Path,
) -> io::Result<std::path::PathBuf> {
    let candidate = if configured.is_absolute() {
        configured.to_path_buf()
    } else {
        let root = scripts_dir.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "relative post-processing scripts require scripts_dir",
            )
        })?;
        crate::nzb_core::path::safe_join(root, &configured.to_string_lossy()).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "post-processing script path is unsafe",
            )
        })?
    };
    let resolved = std::fs::canonicalize(candidate)?;
    if !std::fs::metadata(&resolved)?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "post-processing script is not a regular file",
        ));
    }
    if let Some(root) = scripts_dir {
        let root = std::fs::canonicalize(root)?;
        if !resolved.starts_with(root) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "post-processing script is outside scripts_dir",
            ));
        }
    }
    Ok(resolved)
}

async fn read_script_output<R: AsyncRead + Unpin>(
    reader: R,
    max_output_bytes: usize,
) -> io::Result<(Vec<u8>, bool)> {
    let mut output = Vec::new();
    let mut limited = reader.take(max_output_bytes.saturating_add(1) as u64);
    limited.read_to_end(&mut output).await?;
    let truncated = output.len() > max_output_bytes;
    output.truncate(max_output_bytes);
    Ok((output, truncated))
}

/// Run one operator script from `cwd` with `args` and `env`, capturing its
/// output up to the configured bound and killing it at the timeout. The
/// outcome is a `Script` stage that started at `started`.
async fn run_bounded_script(
    script: &std::path::Path,
    cwd: &std::path::Path,
    args: &[String],
    env: Vec<(&'static str, String)>,
    config: &PostProcScriptConfig,
    started: Instant,
) -> StageResult {
    let failed = |message: String| StageResult {
        name: "Script".into(),
        status: StageStatus::Failed,
        message: Some(message),
        duration_secs: started.elapsed().as_secs_f64(),
    };
    let mut command = tokio::process::Command::new(script);
    command
        .current_dir(cwd)
        .args(args)
        .envs(env)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => return failed(format!("Unable to start post-processing script: {error}")),
    };
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let max_output_bytes = config.max_output_bytes;
    let result = async {
        let stdout_reader = async {
            match stdout {
                Some(reader) => read_script_output(reader, max_output_bytes).await,
                None => Ok((Vec::new(), false)),
            }
        };
        let stderr_reader = async {
            match stderr {
                Some(reader) => read_script_output(reader, max_output_bytes).await,
                None => Ok((Vec::new(), false)),
            }
        };
        let (stdout, stderr) = tokio::join!(stdout_reader, stderr_reader);
        let stdout = stdout?;
        let stderr = stderr?;
        let status = child.wait().await?;
        Ok::<_, io::Error>((status, stdout, stderr))
    };

    match tokio::time::timeout(config.timeout, result).await {
        Ok(Ok((status, stdout, stderr))) if status.success() => StageResult {
            name: "Script".into(),
            status: StageStatus::Success,
            message: Some(script_output_message(&stdout, &stderr)),
            duration_secs: started.elapsed().as_secs_f64(),
        },
        Ok(Ok((status, stdout, stderr))) => failed(format!(
            "Post-processing script exited with {status}: {}",
            script_output_message(&stdout, &stderr)
        )),
        Ok(Err(error)) => failed(format!("Post-processing script failed: {error}")),
        Err(_) => failed(format!(
            "Post-processing script exceeded {} second timeout",
            config.timeout.as_secs()
        )),
    }
}

/// SABnzbd's positional arguments to a post-processing script: final
/// directory, original NZB name, job name, report number, category, group,
/// post-processing status and failure URL. RustNZB keeps no separate NZB file
/// name, so the second argument repeats the job name, and the report number,
/// group and URL are empty.
fn postproc_script_args(job: &NzbJob, stages: &[StageResult]) -> Vec<String> {
    vec![
        job.output_dir.to_string_lossy().into_owned(),
        job.name.clone(),
        job.name.clone(),
        String::new(),
        job.category.clone(),
        String::new(),
        postproc_status_code(stages).to_string(),
        String::new(),
    ]
}

/// Environment for a post-processing script.
///
/// It carries three families of names. The `SAB_*` variables follow
/// SABnzbd's script contract, so a SABnzbd script that reads them works
/// unchanged; every variable SABnzbd always sets is present, empty when
/// RustNZB has no truthful value, because a missing key raises `KeyError`
/// in a Python script. `NZBPP_*` follows NZBGet's contract, including the
/// numeric `PARSTATUS`/`UNPACKSTATUS` codes and the `SUCCESS/ALL` shape of
/// `NZBPP_STATUS`. `SAB_JOB`, `SAB_COMPLETE` and `RUSTNZB_STATUS` are the
/// names RustNZB has always sent and are kept for scripts already using
/// them.
fn postproc_script_env(
    job: &NzbJob,
    final_status: JobStatus,
    stages: &[StageResult],
    script: &std::path::Path,
    pp_level: u8,
) -> Vec<(&'static str, String)> {
    let directory = job.output_dir.to_string_lossy().into_owned();
    let code = postproc_status_code(stages);
    let success = final_status == JobStatus::Completed;
    let password = job.password.clone().unwrap_or_default();
    let group = job
        .files
        .iter()
        .find_map(|file| file.groups.first().cloned())
        .unwrap_or_default();
    let script_name = script
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let total_status = if success { "SUCCESS" } else { "FAILURE" };
    vec![
        // SABnzbd always sets these, so they are always present here.
        ("SAB_SCRIPT", script_name),
        ("SAB_NZO_ID", job.id.clone()),
        ("SAB_FINAL_NAME", job.name.clone()),
        ("SAB_FILENAME", job.name.clone()),
        ("SAB_COMPLETE_DIR", directory.clone()),
        ("SAB_COMPLETE", directory.clone()),
        ("SAB_CAT", job.category.clone()),
        ("SAB_STATUS", final_status.to_string()),
        (
            "SAB_FAIL_MSG",
            if success {
                String::new()
            } else {
                job.error_message.clone().unwrap_or_default()
            },
        ),
        ("SAB_BYTES", job.total_bytes.to_string()),
        ("SAB_BYTES_TRIED", job.total_bytes.to_string()),
        ("SAB_BYTES_DOWNLOADED", job.downloaded_bytes.to_string()),
        ("SAB_PP_STATUS", code.to_string()),
        ("SAB_PP", pp_level.to_string()),
        ("SAB_REPAIR", u8::from(pp_level & 1 != 0).to_string()),
        ("SAB_UNPACK", u8::from(pp_level & 2 != 0).to_string()),
        ("SAB_PRIORITY", sab_priority(job.priority).to_string()),
        ("SAB_GROUP", group),
        ("SAB_PASSWORD", password.clone()),
        (
            "SAB_CORRECT_PASSWORD",
            if success { password } else { String::new() },
        ),
        ("SAB_FILES", sab_files_json(job)),
        // RustNZB verifies and repairs PAR2 natively, so there is no
        // par2cmdline to name.
        ("SAB_PAR2_COMMAND", String::new()),
        ("SAB_RAR_COMMAND", sab_tool(nzb_postproc::find_unrar)),
        ("SAB_7ZIP_COMMAND", sab_tool(nzb_postproc::unpack::find_7z)),
        ("SAB_JOB", job.name.clone()),
        // Declared so a script that reads the key does not crash. RustNZB
        // does not track these.
        ("SAB_URL", String::new()),
        ("SAB_FAILURE_URL", String::new()),
        ("SAB_AGE", String::new()),
        ("SAB_AVG_BPS", String::new()),
        ("SAB_DOWNLOAD_TIME", String::new()),
        ("SAB_ORIG_NZB_GZ", String::new()),
        ("SAB_DUPLICATE", String::new()),
        ("SAB_DUPLICATE_KEY", String::new()),
        ("SAB_UNWANTED_EXT", "0".to_string()),
        ("SAB_OVERSIZED", "0".to_string()),
        ("SAB_ENCRYPTED", "0".to_string()),
        // NZBGet's contract. `NZBPP_STATUS` is `<TOTALSTATUS>/<detail>`.
        ("NZBPP_NZBID", job.id.clone()),
        ("NZBPP_NZBNAME", job.name.clone()),
        ("NZBPP_NZBFILENAME", job.name.clone()),
        ("NZBPP_DIRECTORY", directory.clone()),
        ("NZBPP_FINALDIR", directory),
        ("NZBPP_CATEGORY", job.category.clone()),
        ("NZBPP_TOTALSTATUS", total_status.to_string()),
        (
            "NZBPP_STATUS",
            nzbpp_status(total_status, stages, success).to_string(),
        ),
        ("NZBPP_SCRIPTSTATUS", "NONE".to_string()),
        ("NZBPP_TOTALSIZE", job.total_bytes.to_string()),
        ("NZBPP_PARSTATUS", nzbpp_par_status(stages).to_string()),
        (
            "NZBPP_UNPACKSTATUS",
            nzbpp_unpack_status(stages).to_string(),
        ),
        ("RUSTNZB_STATUS", final_status.to_string()),
    ]
}

/// A tool path for `SAB_*_COMMAND`, empty when the tool is absent — as
/// SABnzbd sends an empty `SAB_7ZIP_COMMAND` when 7-Zip is not installed.
fn sab_tool(find: impl FnOnce() -> Option<String>) -> String {
    find().unwrap_or_default()
}

/// SABnzbd's priority numbering: Low -1, Normal 0, High 1, Force 2.
fn sab_priority(priority: Priority) -> i8 {
    match priority {
        Priority::Low => -1,
        Priority::Normal => 0,
        Priority::High => 1,
        Priority::Force => 2,
    }
}

/// `SAB_FILES`: a JSON array of the files the job produced, relative to
/// `SAB_COMPLETE_DIR`, as SABnzbd sends them.
fn sab_files_json(job: &NzbJob) -> String {
    let files: Vec<String> = regular_output_files(&job.output_dir)
        .iter()
        .map(|path| {
            path.strip_prefix(&job.output_dir)
                .unwrap_or(path)
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    serde_json::to_string(&files).unwrap_or_else(|_| "[]".to_string())
}

/// NZBGet's `NZBPP_STATUS`: `<TOTALSTATUS>/<detail>`.
fn nzbpp_status(total: &str, stages: &[StageResult], success: bool) -> String {
    if success {
        return "SUCCESS/ALL".to_string();
    }
    let failed = |name: &str| {
        stages
            .iter()
            .any(|stage| stage.name == name && stage.status == StageStatus::Failed)
    };
    let detail = if failed("Verify") || failed("Repair") {
        "PAR"
    } else if failed("Extract") {
        "UNPACK"
    } else {
        "HEALTH"
    };
    format!("{total}/{detail}")
}

/// NZBGet's `NZBPP_PARSTATUS`: 0 not checked, 1 checked and failed, 2
/// checked successfully (repaired or already intact).
fn nzbpp_par_status(stages: &[StageResult]) -> u8 {
    let ran = stages.iter().any(|stage| {
        (stage.name == "Verify" || stage.name == "Repair") && stage.status != StageStatus::Skipped
    });
    if !ran {
        return 0;
    }
    let failed = stages.iter().any(|stage| {
        (stage.name == "Verify" || stage.name == "Repair") && stage.status == StageStatus::Failed
    });
    if failed { 1 } else { 2 }
}

/// NZBGet's `NZBPP_UNPACKSTATUS`: 0 not run, 1 failed, 2 succeeded.
/// NZBGet's 3 (write error) and 4 (wrong password) are not distinguishable
/// from the stage result.
fn nzbpp_unpack_status(stages: &[StageResult]) -> u8 {
    match stages
        .iter()
        .find(|stage| stage.name == "Extract" && stage.status != StageStatus::Skipped)
    {
        None => 0,
        Some(stage) if stage.status == StageStatus::Failed => 1,
        Some(_) => 2,
    }
}

/// SABnzbd's post-processing status code: 0 when verification and extraction
/// both succeeded, 1 when verification or repair failed, 2 when extraction
/// failed, and 3 when both failed.
fn postproc_status_code(stages: &[StageResult]) -> u8 {
    let failed = |name: &str| {
        stages
            .iter()
            .any(|stage| stage.name == name && stage.status == StageStatus::Failed)
    };
    let verify_failed = failed("Verify") || failed("Repair");
    let extract_failed = failed("Extract");
    u8::from(verify_failed) + 2 * u8::from(extract_failed)
}

/// Whether `job_id` has just finished and no other job in the queue is still
/// active, so the queue has drained. Called under the jobs lock straight
/// after the job moves to history, so exactly one completion sees the drain.
fn queue_drained_by(jobs: &HashMap<String, JobState>, job_id: &str) -> bool {
    jobs.contains_key(job_id)
        && jobs.iter().all(|(id, state)| {
            id == job_id || matches!(state.job.status, JobStatus::Completed | JobStatus::Failed)
        })
}

fn regular_output_files(root: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut pending = vec![root.to_path_buf()];
    let mut files = Vec::new();
    while let Some(directory) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(metadata) = std::fs::symlink_metadata(&path) else {
                continue;
            };
            if metadata.file_type().is_symlink() {
                continue;
            }
            if metadata.is_dir() {
                pending.push(path);
            } else if metadata.is_file() {
                files.push(path);
            }
        }
    }
    files.sort();
    files
}

fn script_output_message(stdout: &(Vec<u8>, bool), stderr: &(Vec<u8>, bool)) -> String {
    let mut message = String::from_utf8_lossy(&stdout.0).trim().to_string();
    let error = String::from_utf8_lossy(&stderr.0).trim().to_string();
    if !error.is_empty() {
        if !message.is_empty() {
            message.push_str("; ");
        }
        message.push_str(&error);
    }
    if stdout.1 || stderr.1 {
        if !message.is_empty() {
            message.push_str("; ");
        }
        message.push_str("output truncated");
    }
    message
}

// ---------------------------------------------------------------------------
// Job checkpoint for resume support
// ---------------------------------------------------------------------------

/// Compact representation of per-file article completion state.
/// Stored as JSON in the `job_data` column for resuming downloads after restart.
#[derive(Serialize, Deserialize)]
struct JobCheckpoint {
    /// Map of file_id -> set of downloaded segment numbers
    files: HashMap<String, Vec<u32>>,
    /// Bytes downloaded so far
    downloaded_bytes: u64,
    /// Number of articles downloaded
    articles_downloaded: usize,
    /// Number of articles failed
    articles_failed: usize,
    /// Number of files completed
    files_completed: usize,
    /// Full article outcomes. This was added after the original segment-only
    /// checkpoint so history retries can distinguish missing articles from
    /// articles that were already written before a failure.
    #[serde(default)]
    articles: HashMap<String, Vec<ArticleCheckpoint>>,
    /// Retained partial work directory for missing-only history retry.
    #[serde(default)]
    work_dir: Option<std::path::PathBuf>,
    /// Complete-directory folder this job already reserved, so a resumed
    /// attempt keeps writing to its own folder instead of reserving another.
    #[serde(default)]
    output_dir: Option<std::path::PathBuf>,
    /// Active download time accumulated across every engine run of this job,
    /// so a resumed job's speed covers all of its bytes.
    #[serde(default)]
    download_time_secs: f64,
    /// Bytes a history retry carried in from the failed attempt's
    /// checkpoint. That attempt's statistics row already counts them.
    #[serde(default)]
    carried_bytes: u64,
    /// Per-server article statistics accumulated so far.
    #[serde(default)]
    server_stats: Vec<ServerArticleStats>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ArticleCheckpoint {
    message_id: String,
    segment_number: u32,
    bytes: u64,
    downloaded: bool,
    data_begin: Option<u64>,
    data_size: Option<u64>,
    crc32: Option<u32>,
    tried_servers: Vec<String>,
    tries: u32,
}

fn checkpoint_for_job(job: &NzbJob, output_dir_claimed: bool) -> JobCheckpoint {
    JobCheckpoint {
        files: job
            .files
            .iter()
            .map(|file| {
                (
                    file.filename.clone(),
                    file.articles
                        .iter()
                        .filter(|article| article.downloaded)
                        .map(|article| article.segment_number)
                        .collect(),
                )
            })
            .collect(),
        downloaded_bytes: job.downloaded_bytes,
        articles_downloaded: job.articles_downloaded,
        articles_failed: job.articles_failed,
        files_completed: job.files_completed,
        articles: job
            .files
            .iter()
            .map(|file| {
                (
                    file.filename.clone(),
                    file.articles
                        .iter()
                        .map(|article| ArticleCheckpoint {
                            message_id: article.message_id.clone(),
                            segment_number: article.segment_number,
                            bytes: article.bytes,
                            downloaded: article.downloaded,
                            data_begin: article.data_begin,
                            data_size: article.data_size,
                            crc32: article.crc32,
                            tried_servers: article.tried_servers.clone(),
                            tries: article.tries,
                        })
                        .collect(),
                )
            })
            .collect(),
        work_dir: Some(job.work_dir.clone()),
        output_dir: output_dir_claimed.then(|| job.output_dir.clone()),
        download_time_secs: 0.0,
        carried_bytes: 0,
        server_stats: job.server_stats.clone(),
    }
}

/// Restore the download accounting a checkpoint carries across a restart:
/// per-server statistics into the job, and the accumulated active time and
/// retry carry-over returned as `(prior_download_secs, carried_bytes)`.
fn restore_checkpoint_accounting(job: &mut NzbJob, checkpoint: &JobCheckpoint) -> (f64, u64) {
    job.server_stats = checkpoint.server_stats.clone();
    (checkpoint.download_time_secs, checkpoint.carried_bytes)
}

fn apply_checkpoint(job: &mut NzbJob, checkpoint: &JobCheckpoint) {
    job.downloaded_bytes = checkpoint.downloaded_bytes;
    job.articles_downloaded = checkpoint.articles_downloaded;
    job.articles_failed = checkpoint.articles_failed;
    job.files_completed = checkpoint.files_completed;
    for file in &mut job.files {
        let outcomes = checkpoint.articles.get(&file.filename);
        let segments = checkpoint
            .files
            .get(&file.filename)
            .or_else(|| checkpoint.files.get(&file.id));
        let mut file_bytes: u64 = 0;
        for article in &mut file.articles {
            if let Some(outcome) = outcomes.and_then(|items| {
                items.iter().find(|item| {
                    item.message_id == article.message_id
                        || item.segment_number == article.segment_number
                })
            }) {
                article.downloaded = outcome.downloaded;
                article.data_begin = outcome.data_begin;
                article.data_size = outcome.data_size;
                article.crc32 = outcome.crc32;
                article.tried_servers = outcome.tried_servers.clone();
                article.tries = outcome.tries;
            } else if segments.is_some_and(|items| items.contains(&article.segment_number)) {
                // Checkpoints written before article outcomes existed only
                // recorded downloaded segment numbers.
                article.downloaded = true;
            }
            if article.downloaded {
                file_bytes = file_bytes.saturating_add(article.data_size.unwrap_or(article.bytes));
            }
        }
        file.bytes_downloaded = file_bytes;
        file.assembled = file.articles.iter().all(|article| article.downloaded);
    }
}

// ---------------------------------------------------------------------------
// Speed tracker (simple rolling window)
// ---------------------------------------------------------------------------

pub(crate) struct SpeedTracker {
    /// Bytes downloaded in the current window.
    window_bytes: AtomicU64,
    /// Current speed in bytes per second.
    current_bps: AtomicU64,
}

impl SpeedTracker {
    pub fn new() -> Self {
        Self {
            window_bytes: AtomicU64::new(0),
            current_bps: AtomicU64::new(0),
        }
    }

    /// Record downloaded bytes.
    pub fn record(&self, bytes: u64) {
        self.window_bytes.fetch_add(bytes, Ordering::Relaxed);
    }

    /// Called periodically to compute speed and reset the window.
    pub fn tick(&self, elapsed_secs: f64) {
        let bytes = self.window_bytes.swap(0, Ordering::Relaxed);
        if elapsed_secs > 0.001 {
            let bps = (bytes as f64 / elapsed_secs) as u64;
            self.current_bps.store(bps, Ordering::Relaxed);
        }
    }

    pub fn bps(&self) -> u64 {
        self.current_bps.load(Ordering::Relaxed)
    }
}

// ---------------------------------------------------------------------------
// Hopeless job detection
// ---------------------------------------------------------------------------

/// Tracks article failure statistics for a single job to determine
/// whether it can possibly complete. Implements a four-tier check:
///
/// 1. **Grace period** — ignore the first few failures (par2 can repair minor gaps)
/// 2. **Early failure check** — if most articles fail, abort fast (Phase 6:
///    no longer capped to the first 25% of articles; the check runs
///    continuously while the job is downloading)
/// 3. **Ongoing availability** — compare missing content with usable PAR2 capacity
/// 4. **No-progress timeout** — abort when the job stops emitting article
///    progress for longer than `no_progress_timeout`, even after partial
///    success. This catches both startup zombies and late-stage stalls.
struct HopelessTracker {
    /// When the tracker (and therefore the download attempt) was instantiated.
    /// Used for operator-facing elapsed-time snapshots.
    created_at: Instant,
    /// Timestamp of the last success/failure event observed for this job.
    last_progress_at: Instant,
    /// Total content bytes (excluding par2 files).
    content_bytes: u64,
    /// Recovery capacity declared by PAR2 volume files. Index files do not
    /// contribute capacity. The block count is retained for diagnostics and
    /// future source-block mapping; bytes are the conservative interim unit
    /// used before the assembled index can be parsed.
    recovery_capacity_bytes: u64,
    recovery_blocks_total: u64,
    /// Declared recovery data lost to definitive volume-segment failures.
    recovery_bytes_unavailable: u64,
    recovery_blocks_unavailable: u64,
    /// Per-set accounting prevents recovery blocks from one PAR2 set from
    /// masking damage in another set.
    recovery_capacity_by_set: HashMap<String, u64>,
    recovery_unavailable_by_set: HashMap<String, u64>,
    missing_content_by_set: HashMap<String, u64>,
    unassociated_missing_bytes: u64,
    content_file_sets: HashMap<String, Option<String>>,
    pending_file_classifications: std::collections::HashSet<String>,
    /// Content bytes confirmed missing (failed articles in non-par2 files).
    content_bytes_missing: u64,
    /// Articles checked so far (downloaded + failed, not par2).
    content_articles_checked: usize,
    /// Content articles that failed.
    content_articles_failed: usize,
    /// Total content articles expected (non-par2).
    content_articles_total: usize,
    /// The job carries at least one PAR2 file, so the parity tier applies
    /// instead of the article-count grace (WI-145).
    has_par2_set: bool,
    /// Every recovery volume's block count is known from its filename. When
    /// one is not, the declared total undercounts and the parity tier stays
    /// out of the way.
    recovery_blocks_known: bool,
    /// Content files with at least one article refused by every server.
    /// PAR2 slices never span files, so each is at least one damaged slice.
    confirmed_missing_files: std::collections::HashSet<String>,
    /// Confirmed-missing content articles not attributed to a file.
    confirmed_missing_unattributed: usize,
}

/// Number of bad articles allowed before the byte-based abort checks kick in.
/// Jobs with a PAR2 set are first judged by the parity tier, which needs no
/// grace; this remains the only guard for jobs with no PAR2 set at all.
const HOPELESS_GRACE_ARTICLES: usize = 5;
/// Minimum content articles checked before the early failure check fires.
const EARLY_CHECK_MIN_ARTICLES: usize = 10;
/// Failure rate threshold for the early check (0.0–1.0).
const EARLY_CHECK_FAILURE_RATE: f64 = 0.80;

impl HopelessTracker {
    fn new(job: &NzbJob) -> Self {
        let mut content_bytes: u64 = 0;
        let mut recovery_capacity_bytes: u64 = 0;
        let mut recovery_blocks_total: u64 = 0;
        let mut content_articles_total: usize = 0;
        let recovery_set_names = job
            .files
            .iter()
            .filter(|file| file.is_par2)
            .filter_map(|file| file.par2_setname.as_deref())
            .map(str::to_ascii_lowercase)
            .collect::<std::collections::HashSet<_>>();
        let mut recovery_capacity_by_set = HashMap::new();
        let mut content_file_sets = HashMap::new();
        let mut pending_file_classifications = std::collections::HashSet::new();
        let mut has_par2_set = false;
        let mut recovery_blocks_known = true;

        for file in &job.files {
            if file.is_par2 {
                has_par2_set = true;
                if file.par2_vol.is_some() && file.par2_blocks.is_none() {
                    recovery_blocks_known = false;
                }
                // A plain .par2 is the index. Only .volNN+MM.par2 or
                // .volNN-MM.par2 recovery volumes declare usable blocks.
                if file.par2_vol.is_some() && file.par2_blocks.is_some_and(|blocks| blocks > 0) {
                    recovery_capacity_bytes = recovery_capacity_bytes.saturating_add(file.bytes);
                    recovery_blocks_total = recovery_blocks_total
                        .saturating_add(u64::from(file.par2_blocks.unwrap_or_default()));
                    if let Some(set_name) = file.par2_setname.as_deref() {
                        let capacity = recovery_capacity_by_set
                            .entry(set_name.to_ascii_lowercase())
                            .or_insert(0u64);
                        *capacity = capacity.saturating_add(file.bytes);
                    }
                }
                content_file_sets.insert(
                    file.id.clone(),
                    file.par2_setname.as_deref().map(str::to_ascii_lowercase),
                );
            } else {
                content_bytes += file.bytes;
                content_articles_total += file.articles.len();
                let filename = file.filename.to_ascii_lowercase();
                let associated_set = if recovery_set_names.len() == 1 {
                    recovery_set_names.iter().next().cloned()
                } else {
                    recovery_set_names
                        .iter()
                        .filter(|set_name| filename.starts_with(set_name.as_str()))
                        .max_by_key(|set_name| set_name.len())
                        .cloned()
                };
                content_file_sets.insert(file.id.clone(), associated_set);
                if !nzb_dispatch::has_known_extension(&file.filename) {
                    pending_file_classifications.insert(file.id.clone());
                }
            }
        }

        Self {
            created_at: Instant::now(),
            last_progress_at: Instant::now(),
            content_bytes,
            recovery_capacity_bytes,
            recovery_blocks_total,
            recovery_bytes_unavailable: 0,
            recovery_blocks_unavailable: 0,
            recovery_capacity_by_set,
            recovery_unavailable_by_set: HashMap::new(),
            missing_content_by_set: HashMap::new(),
            unassociated_missing_bytes: 0,
            content_file_sets,
            pending_file_classifications,
            content_bytes_missing: 0,
            content_articles_checked: 0,
            content_articles_failed: 0,
            content_articles_total,
            has_par2_set,
            recovery_blocks_known,
            confirmed_missing_files: std::collections::HashSet::new(),
            confirmed_missing_unattributed: 0,
        }
    }

    /// Record a successful content article download.
    fn record_success(&mut self, is_par2: bool) {
        self.last_progress_at = Instant::now();
        if !is_par2 {
            self.content_articles_checked += 1;
        }
    }

    /// Correct an obfuscated NZB subject after a yEnc header reveals that the
    /// file is PAR2. The file must leave the content denominator and only a
    /// recovery volume (never the index) may add capacity.
    fn reclassify_as_par2(
        &mut self,
        file_id: &str,
        file_bytes: u64,
        article_count: usize,
        volume: Option<u32>,
        blocks: Option<u32>,
        set_name: Option<&str>,
    ) {
        self.content_bytes = self.content_bytes.saturating_sub(file_bytes);
        self.content_articles_total = self.content_articles_total.saturating_sub(article_count);
        self.has_par2_set = true;
        if volume.is_some() && blocks.is_none() {
            self.recovery_blocks_known = false;
        }
        self.confirmed_missing_files.remove(file_id);
        if volume.is_some() && blocks.is_some_and(|count| count > 0) {
            self.recovery_capacity_bytes = self.recovery_capacity_bytes.saturating_add(file_bytes);
            self.recovery_blocks_total = self
                .recovery_blocks_total
                .saturating_add(u64::from(blocks.unwrap_or_default()));
            if let Some(set_name) = set_name {
                *self
                    .recovery_capacity_by_set
                    .entry(set_name.to_ascii_lowercase())
                    .or_insert(0) += file_bytes;
            }
        }
        self.content_file_sets
            .insert(file_id.to_string(), set_name.map(str::to_ascii_lowercase));
        self.pending_file_classifications.remove(file_id);
    }

    fn mark_file_classified(&mut self, file_id: &str) {
        self.pending_file_classifications.remove(file_id);
    }

    /// Record a failed content article. Returns the estimated byte size
    /// of the missing article.
    ///
    /// Phase 6: takes a typed [`ArticleFailureKind`] so the tracker can
    /// ignore failures that are likely transient (server-down, auth, etc.)
    /// and only count failures that genuinely indicate the article cannot
    /// be retrieved (NotFound, DecodeError).
    #[cfg(test)]
    fn record_failure(
        &mut self,
        is_par2: bool,
        estimated_bytes: u64,
        kind: nzb_dispatch::ArticleFailureKind,
    ) {
        self.record_file_failure(None, is_par2, estimated_bytes, kind);
    }

    fn record_file_failure(
        &mut self,
        file_id: Option<&str>,
        is_par2: bool,
        estimated_bytes: u64,
        kind: nzb_dispatch::ArticleFailureKind,
    ) {
        self.last_progress_at = Instant::now();
        if is_par2 {
            self.recovery_bytes_unavailable = self
                .recovery_bytes_unavailable
                .saturating_add(estimated_bytes)
                .min(self.recovery_capacity_bytes);
            if self.recovery_capacity_bytes > 0 && self.recovery_blocks_total > 0 {
                self.recovery_blocks_unavailable = ((u128::from(self.recovery_bytes_unavailable)
                    * u128::from(self.recovery_blocks_total))
                .div_ceil(u128::from(self.recovery_capacity_bytes)))
                .min(u128::from(self.recovery_blocks_total))
                    as u64;
            }
            if let Some(Some(set_name)) = file_id.and_then(|id| self.content_file_sets.get(id)) {
                *self
                    .recovery_unavailable_by_set
                    .entry(set_name.clone())
                    .or_insert(0) += estimated_bytes;
            }
            return;
        }
        self.content_articles_checked += 1;
        // Only an article refused by every server is a missing slice. A
        // decode error may be a transport fault and stays unestablished.
        if kind == nzb_dispatch::ArticleFailureKind::NotFound {
            match file_id {
                Some(id) => {
                    self.confirmed_missing_files.insert(id.to_string());
                }
                None => self.confirmed_missing_unattributed += 1,
            }
        }
        // Only failures that count toward "definitively not retrievable"
        // increment the failed counter and missing-bytes total. A 502 or
        // AuthFailed on one server doesn't mean the article is gone.
        if kind.counts_toward_hopeless() {
            self.content_articles_failed += 1;
            self.content_bytes_missing += estimated_bytes;
            match file_id.and_then(|id| self.content_file_sets.get(id)) {
                Some(Some(set_name)) => {
                    *self
                        .missing_content_by_set
                        .entry(set_name.clone())
                        .or_insert(0) += estimated_bytes;
                }
                _ => {
                    self.unassociated_missing_bytes = self
                        .unassociated_missing_bytes
                        .saturating_add(estimated_bytes);
                }
            }
        }
    }

    /// Check whether the job should be aborted.
    ///
    /// Returns `Some(HopelessAbort)` if the job is hopeless, carrying both
    /// the human-readable reason and a stable `tier` label that operators
    /// can filter logs by. `None` means the job should continue.
    fn check(
        &self,
        abort_hopeless: bool,
        early_failure_check: bool,
        required_completion_pct: f64,
    ) -> Option<HopelessAbort> {
        if !abort_hopeless {
            return None;
        }

        // Tier 0: parity. With a PAR2 set, judge confirmed damage against
        // the recovery slices the release declares, not an article count.
        if self.has_par2_set
            && self.pending_file_classifications.is_empty()
            && let Some(abort) = self.parity_check()
        {
            return Some(abort);
        }

        // Tier 1: grace period — allow minor gaps that par2 can fix
        if self.content_articles_failed <= HOPELESS_GRACE_ARTICLES {
            return None;
        }

        // Unknown/obfuscated NZB subjects may still reveal PAR2 volumes in
        // their yEnc headers. Do not declare content damage hopeless until
        // those files have been classified or the normal no-progress path
        // takes over.
        if !self.pending_file_classifications.is_empty() {
            return None;
        }

        let usable_recovery_bytes = self
            .recovery_capacity_bytes
            .saturating_sub(self.recovery_bytes_unavailable);
        let available_content_bytes = self
            .content_bytes
            .saturating_sub(self.content_bytes_missing);
        let effective_bytes = available_content_bytes.saturating_add(usable_recovery_bytes);
        let required_bytes =
            ((self.content_bytes as f64) * required_completion_pct / 100.0).ceil() as u64;

        // Tier 2: early failure check — catch completely dead NZBs fast.
        // Phase 6: removed the `<= total/4` window upper bound. The check
        // now fires whenever the failure rate is above the threshold AND
        // there are enough samples to be statistically meaningful. The old
        // window was a footgun: slow-trickle failures crept past the 25%
        // mark before the rate accumulated, and tier 3's bytes-availability
        // check wouldn't fire until many more bytes were confirmed missing.
        if early_failure_check && self.content_articles_checked >= EARLY_CHECK_MIN_ARTICLES {
            let failure_rate =
                self.content_articles_failed as f64 / self.content_articles_checked as f64;
            let projected_missing_bytes = (self.content_bytes as f64 * failure_rate).ceil() as u64;
            let safety_reserve = required_bytes.saturating_sub(self.content_bytes);
            if failure_rate >= EARLY_CHECK_FAILURE_RATE
                && projected_missing_bytes.saturating_add(safety_reserve) > usable_recovery_bytes
            {
                return Some(HopelessAbort {
                    tier: "early_failure",
                    reason: format!(
                        "Aborted: {:.0}% of {} checked articles missing ({} of {} failed); \
                         projected damage {} bytes exceeds {} usable recovery bytes plus reserve",
                        failure_rate * 100.0,
                        self.content_articles_checked,
                        self.content_articles_failed,
                        self.content_articles_checked,
                        projected_missing_bytes,
                        usable_recovery_bytes,
                    ),
                });
            }
        }

        let safety_reserve = required_bytes.saturating_sub(self.content_bytes);
        let set_repairable = (!self.recovery_capacity_by_set.is_empty()).then(|| {
            if self.unassociated_missing_bytes > 0 {
                // Multiple PAR2 sets with an ambiguous content filename need
                // the assembled index for authoritative association. Defer
                // to post-processing instead of borrowing blocks from the
                // wrong set or aborting prematurely.
                return true;
            }
            let mut remaining_reserve = 0u64;
            for (set_name, capacity) in &self.recovery_capacity_by_set {
                let usable = capacity.saturating_sub(
                    self.recovery_unavailable_by_set
                        .get(set_name)
                        .copied()
                        .unwrap_or(0),
                );
                let missing = self
                    .missing_content_by_set
                    .get(set_name)
                    .copied()
                    .unwrap_or(0);
                if missing > usable {
                    return false;
                }
                remaining_reserve =
                    remaining_reserve.saturating_add(usable.saturating_sub(missing));
            }
            self.missing_content_by_set
                .keys()
                .all(|set_name| self.recovery_capacity_by_set.contains_key(set_name))
                && remaining_reserve >= safety_reserve
        });

        // Recovery capacity and the configured safety reserve are the
        // primary ongoing completion model. Prefer per-set proof when set
        // associations are available; otherwise retain the conservative
        // aggregate estimate for older/ambiguous NZBs.
        if set_repairable.unwrap_or(effective_bytes >= required_bytes) {
            return None;
        }

        // Tier 3: effective completion, including usable PAR2 recovery.
        if self.content_bytes > 0 {
            let availability_pct = 100.0 * effective_bytes as f64 / self.content_bytes as f64;
            if availability_pct < required_completion_pct || set_repairable == Some(false) {
                return Some(HopelessAbort {
                    tier: "ongoing_availability",
                    reason: format!(
                        "Aborted: effective completion {availability_pct:.3}% is below \
                         {required_completion_pct:.3}%: {} missing content bytes in {} of {} \
                         articles, {} usable recovery bytes ({} of {} recovery blocks unavailable)",
                        self.content_bytes_missing,
                        self.content_articles_failed,
                        self.content_articles_total,
                        usable_recovery_bytes,
                        self.recovery_blocks_unavailable,
                        self.recovery_blocks_total,
                    ),
                });
            }
        }

        None
    }
}

impl HopelessTracker {
    /// Recovery slices still obtainable: declared by volume filenames, less
    /// those lost to failed volume articles.
    fn recovery_blocks_available(&self) -> u64 {
        self.recovery_blocks_total
            .saturating_sub(self.recovery_blocks_unavailable)
    }

    /// A lower bound on damaged slices from confirmed-missing articles. Only
    /// articles every server refused count; provisional, transport-failed
    /// and unasked articles are unestablished. Slices never span files, so
    /// each file with a confirmed hole holds at least one damaged slice.
    fn damaged_slices_lower_bound(&self) -> u64 {
        let files = self.confirmed_missing_files.len() as u64;
        if files == 0 && self.confirmed_missing_unattributed > 0 {
            1
        } else {
            files
        }
    }

    /// Hopeless when the slices certainly damaged exceed the recovery
    /// slices available. Mid-download the damaged count is a lower bound;
    /// the exact count comes from the slice map at job end
    /// ([`parity_diagnosis`]).
    fn parity_check(&self) -> Option<HopelessAbort> {
        if !self.recovery_blocks_known {
            return None;
        }
        let damaged = self.damaged_slices_lower_bound();
        let available = self.recovery_blocks_available();
        (damaged > available).then(|| HopelessAbort {
            tier: "parity",
            reason: format!(
                "Aborted: at least {damaged} slice(s) missing but the release only \
                 carries {available} recovery slice(s)"
            ),
        })
    }
}

/// The end-of-download parity decision (WI-145). `damaged` slices are those
/// the slice map confirmed; `Unknown` slices are unestablished and do not
/// count. Recovery is the larger of what the PAR2 index carries and what the
/// volume filenames declare, so neither source alone can cause an abort.
/// `declared_available` is `None` when a volume's block count is unknown;
/// then there is no decision.
fn parity_diagnosis(map: &SliceMapSummary, declared_available: Option<u64>) -> Option<String> {
    let damaged = map.damaged as u64;
    let available = declared_available?.max(u64::from(map.recovery_block_count));
    (damaged > available).then(|| {
        format!(
            "{damaged} slice(s) missing but the release only carries {available} recovery slice(s)"
        )
    })
}

/// Result of a positive [`HopelessTracker::check`] — both the reason string
/// (for the user-visible error_message) and a stable `tier` label so logs
/// and metrics can be grouped by which heuristic fired.
#[derive(Debug, Clone)]
pub(crate) struct HopelessAbort {
    pub tier: &'static str,
    pub reason: String,
}

impl HopelessTracker {
    /// Reset the no-progress clock so the article timeout starts fresh.
    ///
    /// Called when a job returns to `Downloading` after a pause. The clock is
    /// a wall-clock `Instant` that only advances on real article progress, so
    /// without this reset the time a job spent paused would count toward the
    /// no-progress timeout and abort it the instant it resumes (GH #123). A
    /// paused job is never actively fetching, so paused time must not count.
    fn reset_progress_clock(&mut self) {
        self.last_progress_at = Instant::now();
    }

    /// Phase 6: time-based hopeless check. Operates on the tracker's
    /// `created_at` field, not on article counters, so it fires even when
    /// the engine has stopped emitting progress events entirely (the
    /// zombie scenario).
    ///
    /// Aborts if the tracker has gone longer than `timeout` without
    /// a success or failure event. The caller is the queue manager's
    /// periodic tick — see
    /// [`QueueManager::scan_for_no_progress_jobs`].
    fn time_based_check(&self, timeout: Duration) -> Option<HopelessAbort> {
        let idle = self.last_progress_at.elapsed();
        if idle >= timeout {
            return Some(HopelessAbort {
                tier: "no_progress_timeout",
                reason: format!(
                    "Aborted: no article completed or failed for {}s ({} checked, {} confirmed missing)",
                    idle.as_secs(),
                    self.content_articles_checked,
                    self.content_articles_failed
                ),
            });
        }
        None
    }
}

// ---------------------------------------------------------------------------
// Per-job state
// ---------------------------------------------------------------------------

struct JobState {
    /// The job data (shared with API for reading).
    job: NzbJob,
    /// Handle to the per-job progress listener task.
    progress_handle: Option<tokio::task::JoinHandle<()>>,
    /// Per-job speed tracker.
    speed: Arc<SpeedTracker>,
    /// Raw NZB data for retry.
    nzb_data: Option<Vec<u8>>,
    /// Direct unpacker for RAR extraction during download.
    direct_unpacker: Option<DirectUnpacker>,
    /// Hopeless job tracker (None until download starts).
    hopeless_tracker: Option<HopelessTracker>,
    /// The download's in-flight PAR2 repair fold, handed to post-processing
    /// (WI-148). In memory only: a restart repairs from disk.
    repair_fold: Option<nzb_decode::SharedFold>,
    /// Active download duration captured at terminal download resolution,
    /// including `prior_download_secs`.
    download_time_secs: Option<f64>,
    /// Active download time from engine runs before the current one (a
    /// restart resumes a job in a fresh engine run).
    prior_download_secs: f64,
    /// Bytes this job carried in from a failed attempt's checkpoint when it
    /// was retried; excluded from this job's statistics row.
    carried_bytes: u64,
    /// Typed terminal failure code, tracked in memory until it is persisted to
    /// the terminal history row.
    failure_code: Option<JobFailureCode>,
    /// PAR2 recovery volumes `(fetched, available)` when the download
    /// finished, recorded on the history row.
    par2_volumes: Option<(u32, u32)>,
    /// Whether `job.output_dir` is a complete-directory folder this job
    /// reserved for itself. Reserving happens when output is first written,
    /// not at add time, so queued jobs never hold a name.
    output_dir_claimed: bool,
    /// The status the last progress save wrote to the queue row, so the save
    /// only takes the job-list and database locks together on a transition.
    persisted_status: Option<JobStatus>,
}

/// Field a manual queue sort orders by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueSortField {
    /// Fraction of the job's bytes still to download.
    Remaining,
    /// Job name, case-insensitively.
    Name,
    /// Total job size in bytes.
    Size,
    /// Time since the job was added; ascending is youngest first, matching
    /// SABnzbd's `avg_age`.
    Age,
    /// Job priority, `Low` first when ascending.
    Priority,
}

impl QueueSortField {
    fn compare(self, left: &NzbJob, right: &NzbJob) -> std::cmp::Ordering {
        match self {
            Self::Remaining => {
                let remaining = |job: &NzbJob| {
                    let total = job.total_bytes.max(1);
                    (total.saturating_sub(job.downloaded_bytes), total)
                };
                let (left_remaining, left_total) = remaining(left);
                let (right_remaining, right_total) = remaining(right);
                (left_remaining as u128 * right_total as u128)
                    .cmp(&(right_remaining as u128 * left_total as u128))
            }
            Self::Name => left
                .name
                .chars()
                .flat_map(char::to_lowercase)
                .cmp(right.name.chars().flat_map(char::to_lowercase)),
            Self::Size => left.total_bytes.cmp(&right.total_bytes),
            // Younger means added later.
            Self::Age => right.added_at.cmp(&left.added_at),
            Self::Priority => (left.priority as u8).cmp(&(right.priority as u8)),
        }
    }
}

impl std::str::FromStr for QueueSortField {
    type Err = ();

    /// Parse a sort field name, case-insensitively. Accepts SABnzbd's
    /// `avg_age` and `bytes` spellings alongside the native names.
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.to_ascii_lowercase().as_str() {
            "remaining" => Ok(Self::Remaining),
            "name" => Ok(Self::Name),
            "size" | "bytes" => Ok(Self::Size),
            "age" | "avg_age" => Ok(Self::Age),
            "priority" => Ok(Self::Priority),
            _ => Err(()),
        }
    }
}

/// Why a job, or the whole queue, is paused.
///
/// Exposed on the native API so a pause applied by the disk-space guard can
/// be told apart from one the user asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum PauseReason {
    /// The user paused this job.
    Manual,
    /// The user paused all downloads (Pause All or a timed pause).
    Global,
    /// The disk-space guard (`min_free_space_bytes`) is holding downloads.
    /// The hold lifts automatically once space frees up.
    DiskSpace,
    /// The job was paused after a news-server error.
    Server,
    /// A time-of-day schedule rule is pausing downloads. The pause lifts
    /// automatically when the window ends.
    Schedule,
}

/// Every reason the queue is globally paused right now.
///
/// Holds are additive rather than a single owner: a disk-space hold can be
/// open while a schedule window is open, and the queue must stay paused until
/// *both* are gone. Each holder lifts only its own flag, so one ending never
/// resumes work the other is still holding.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct PauseHolds {
    /// A pause the user asked for and has not since lifted.
    user: bool,
    /// The disk-space guard tripped.
    disk: Option<DateTime<Utc>>,
    /// The schedule is keeping the queue paused. A window the user has
    /// resumed inside sets this to `false` for the rest of that window.
    schedule: bool,
    /// Every pause-window occurrence currently open, by rule id and the date
    /// the window started on (`rule@YYYY-MM-DD`).
    ///
    /// The set, not a change in it, says whether a window is newly open: a
    /// window opening adds an occurrence, while another of two overlapping
    /// windows closing only removes one and must not re-pause the queue.
    open_windows: Vec<String>,
    /// The occurrences the schedule has already acted on.
    ///
    /// A window pauses when it first appears here. Closing it removes it from
    /// `open_windows` and from here, so the same occurrence reopening later is
    /// new again.
    triggered_windows: Vec<String>,
    /// The occurrences the user has resumed by hand.
    ///
    /// A manual resume outlasts the window it was made in: a settings save or
    /// a tick re-evaluates the same occurrence, so without this it would come
    /// back. A timed user pause running out is not a manual resume and does
    /// not record one.
    resumed_windows: Vec<String>,
    /// The occurrences a timed user pause is exempting: it was started while
    /// they were open, so their automatic hold is suspended, and its expiry
    /// restores it. Without this, "pause for 2h" before a window would pause
    /// for the window's start and then resume straight into it.
    user_paused_over: Vec<String>,
}

impl PauseHolds {
    fn any(&self) -> bool {
        self.user || self.disk.is_some() || self.schedule
    }

    /// The reason to show, most specific first. A pause the user asked for
    /// outranks every automatic hold, a disk hold outranks a schedule window
    /// that merely overlapped it, and the schedule is the reason only when it
    /// is what keeps the queue paused.
    fn reason(&self) -> Option<PauseReason> {
        if self.user {
            Some(PauseReason::Global)
        } else if self.disk.is_some() {
            Some(PauseReason::DiskSpace)
        } else if self.schedule {
            Some(PauseReason::Schedule)
        } else {
            None
        }
    }

    /// The events a scheduled decision adds to the open windows, sorted.
    ///
    /// Every open rule contributes its `rule@date` occurrence, so a rule
    /// overlapping another cannot hide it.
    fn occurrences<'a>(windows: impl IntoIterator<Item = &'a str>) -> Vec<String> {
        let mut occurrences: Vec<String> = windows.into_iter().map(str::to_string).collect();
        occurrences.sort();
        occurrences.dedup();
        occurrences
    }

    /// The occurrences a scheduled decision opens that have not been given a
    /// chance to pause yet.
    fn newly_opened(&self, windows: &[String]) -> Vec<String> {
        windows
            .iter()
            .filter(|window| !contains(&self.triggered_windows, window))
            .cloned()
            .collect()
    }

    /// The occurrences the schedule still holds the queue for, of `windows`.
    ///
    /// An occurrence holds once it has been triggered and is not suspended by
    /// a timed user pause; the remaining ones keep the queue paused while the
    /// window that opened them is still open.
    fn held_occurrences(&self, windows: &[String]) -> Vec<String> {
        windows
            .iter()
            .filter(|window| contains(&self.triggered_windows, window))
            .filter(|window| !contains(&self.resumed_windows, window))
            .filter(|window| !contains(&self.user_paused_over, window))
            .cloned()
            .collect()
    }

    /// Drop the occurrences that are no longer open from the bookkeeping that
    /// only describes open windows. A window that closes and later reopens is
    /// then a new occurrence again, as the user expects.
    fn retain_open(&mut self, open: &[String]) {
        self.open_windows = open.to_vec();
        self.triggered_windows
            .retain(|window| contains(open, window));
        self.resumed_windows.retain(|window| contains(open, window));
        self.user_paused_over
            .retain(|window| contains(open, window));
    }
}

fn contains(haystack: &[String], needle: &str) -> bool {
    haystack.iter().any(|value| value == needle)
}
/// Notification fired immediately when a job is accepted into the queue.
#[derive(Debug, Clone)]
pub struct JobAddedEvent {
    pub id: String,
    pub name: String,
    pub category: String,
    /// Raw NZB bytes, if available at add time. Wrapped in Arc to keep clones cheap.
    pub nzb_data: Option<Arc<Vec<u8>>>,
}

// ---------------------------------------------------------------------------
// QueueManager
// ---------------------------------------------------------------------------

/// Result of [`QueueManager::retry_history_entry`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HistoryRetryOutcome {
    /// A new job was enqueued; carries its id.
    Started(String),
    /// A retry of this entry is already queued or running; carries that
    /// job's id. Nothing new was enqueued.
    InProgress(String),
    /// The history entry has no stored NZB to retry from.
    NoNzbData,
}

/// Thread-safe queue manager that coordinates all downloads.
///
/// Wrapped in `Arc` for sharing between the background task and HTTP handlers.
/// The notification for a job reaching a terminal status, or `None` when
/// the outcome is not reported. A post-processing stop for lack of disk
/// space is reported as a disk event, not as a generic failure.
fn terminal_notification(job: &NzbJob, final_status: JobStatus) -> Option<NotificationEvent> {
    let event = match final_status {
        JobStatus::Completed => NotificationEvent::new(
            NotificationKind::Complete,
            "Download complete",
            job.name.clone(),
        ),
        JobStatus::Failed if job.error_message.as_deref() == Some(POSTPROC_DISK_FULL_MESSAGE) => {
            NotificationEvent::new(
                NotificationKind::DiskFull,
                "Disk space low",
                "Post-processing stopped: free space on a storage volume is below the minimum.",
            )
        }
        JobStatus::Failed => NotificationEvent::new(
            NotificationKind::Failed,
            "Download failed",
            job.error_message
                .clone()
                .unwrap_or_else(|| format!("{} failed", job.name)),
        ),
        _ => return None,
    };
    Some(event.for_job(&job.name, &job.category))
}

/// Error message the post-processing disk check records. Matched when
/// notifying, so a disk stop is reported once, as a disk event.
const POSTPROC_DISK_FULL_MESSAGE: &str = "Insufficient free disk space for post-processing";

pub struct QueueManager {
    /// Active jobs keyed by job ID.
    jobs: Mutex<HashMap<String, JobState>>,
    /// Order of job IDs for display.
    job_order: Mutex<Vec<String>>,
    /// Server configurations.
    servers: Arc<Mutex<Vec<ServerConfig>>>,
    /// Whether all downloads are globally paused.
    globally_paused: AtomicBool,
    /// Flipped to `true` by graceful shutdown: tells every per-job progress
    /// listener to stop accepting events, drain the queued ones and exit.
    progress_shutdown: watch::Sender<bool>,
    /// Serializes global pause/resume transitions with individual resume
    /// attempts so the global gate cannot be bypassed by a racing request.
    pause_transition: Mutex<()>,
    /// Jobs whose `Paused` status was applied by the current global pause.
    ///
    /// Keeping this separate from ordinary per-job pause state means that
    /// `resume_all` only resumes work stopped by `pause_all`.
    globally_paused_jobs: Mutex<HashSet<String>>,
    /// Everything currently holding the queue paused. Holds are additive:
    /// the queue is paused while any of them is set, and resumes when the
    /// last one is lifted. A disk-space hold that overlaps a pause window
    /// therefore does not swallow the window, and the window ending does not
    /// lift the disk hold.
    pause_holds: Mutex<PauseHolds>,
    /// Servers the schedule has taken out of rotation, as configured by the
    /// user. The live list is the configured list with these disabled.
    schedule_disabled_servers: Mutex<Vec<String>>,
    /// The configured server list, before the schedule's overlay. Held so the
    /// overlay can be recomputed whenever either side changes.
    configured_servers: Mutex<Vec<ServerConfig>>,
    /// The user's configured speed limit, which the schedule's cap overlays.
    /// Zero means unlimited.
    configured_speed_limit_bps: AtomicU64,
    /// The cap the schedule currently imposes, if any. Overlays the
    /// configured limit; `None` means the configured limit applies.
    scheduled_speed_cap: Mutex<Option<u64>>,
    /// Global speed tracker.
    speed: SpeedTracker,
    /// Database for persistence.
    db: Mutex<Database>,
    /// App config (incomplete_dir, complete_dir).
    incomplete_dir: Mutex<std::path::PathBuf>,
    complete_dir: Mutex<std::path::PathBuf>,
    /// Timed pause: when to auto-resume (None = not timed).
    pause_until: Mutex<Option<DateTime<Utc>>>,
    /// History retention limit (None = keep all).
    history_retention: Mutex<Option<usize>>,
    /// SAB-compatible history generation. Incremented only when the history
    /// view changes so polling clients can avoid downloading unchanged data.
    history_update: AtomicU64,
    /// Log buffer for capturing per-job logs into history.
    log_buffer: Option<LogBuffer>,
    /// Broadcast channel: fires immediately when a job is accepted into the queue.
    add_tx: broadcast::Sender<JobAddedEvent>,
    /// Max concurrent active downloads (0 = unlimited).
    max_active_downloads: AtomicUsize,
    /// History retries in flight: history entry id -> id of the job the
    /// retry enqueued. Held from the in-flight check through the enqueue so
    /// concurrent retries of one entry admit exactly one job.
    history_retries: Mutex<HashMap<String, String>>,
    /// Automatically keep the queue ordered by remaining percentage.
    auto_sort_remaining_pct: AtomicBool,
    /// What admission does with an NZB matching a queued or history job.
    duplicate_policy: Mutex<DuplicatePolicy>,
    /// Optional bounded post-processing hooks.
    postproc_scripts: Mutex<PostProcScriptConfig>,
    /// Outbound notification targets for job outcomes and disk pressure.
    notifier: Mutex<Arc<Notifier>>,
    /// Action run once the queue drains, set by `change_complete_action`.
    complete_action: Mutex<Option<String>>,
    /// `queue_complete_pers`: keep the action after it has run once.
    queue_complete_persist: AtomicBool,
    /// `par2.trust_inflight_verify`: let post-processing accept the slice map.
    trust_inflight_verify: AtomicBool,
    /// Stage-specific resource gates shared by all post-processing jobs.
    postproc_resources: Arc<PostProcResourcePool>,
    /// Category configs for post-processing decisions.
    categories: Mutex<Vec<CategoryConfig>>,
    /// Minimum free disk space in bytes before pausing downloads.
    min_free_space: AtomicU64,
    /// Bandwidth limiter for throttling downloads.
    bandwidth: Arc<BandwidthLimiter>,
    /// Whether direct unpack (RAR extraction during download) is enabled.
    direct_unpack_enabled: AtomicBool,
    /// Maximum number of nested archive layers to extract after the outer archive.
    max_nested_archive_depth: u8,
    /// Abort downloads that cannot possibly complete.
    abort_hopeless: bool,
    /// Phase 6: maximum time a job may sit in `Downloading` without any
    /// article event before the time-based hopeless tier fires. Settable
    /// at runtime via [`Self::set_no_progress_timeout`].
    no_progress_timeout: Mutex<Duration>,
    /// Quick initial failure check on first N articles.
    early_failure_check: bool,
    /// Canonical article dispatcher owned by `nzb-dispatch`.
    dispatch: Arc<dyn DispatchEngine>,
    /// Minimum effective completion percentage, including usable PAR2 capacity.
    required_completion_pct: f64,
}

impl QueueManager {
    const GLOBAL_PAUSED_JOBS_SETTING: &'static str = "globally_paused_job_ids";
    const DISK_SPACE_HOLD_SETTING: &'static str = "disk_space_hold_since";
    /// Every pause-window occurrence currently open.
    const SCHEDULE_WINDOWS_SETTING: &'static str = "schedule_pause_windows";
    /// The occurrences that have already paused the queue.
    const SCHEDULE_TRIGGERED_SETTING: &'static str = "schedule_triggered_windows";
    /// The occurrences the user has resumed by hand.
    const SCHEDULE_RESUMED_SETTING: &'static str = "schedule_resumed_windows";
    /// The occurrences a timed user pause is exempting.
    const SCHEDULE_USER_PAUSED_SETTING: &'static str = "schedule_user_paused_windows";
    /// The user's own hold, stored as its own flag.
    const USER_HOLD_SETTING: &'static str = "user_pause_hold";
    /// The schedule's own hold, stored as its own flag.
    const SCHEDULE_HOLD_SETTING: &'static str = "schedule_pause_hold";
    /// Whether the schedule itself applied the pause. Only read for records
    /// written before each hold carried its own flag.
    const PAUSE_OWNER_SETTING: &'static str = "global_pause_owner";
    const PAUSE_OWNER_USER: &'static str = "user";
    const PAUSE_OWNER_DISK: &'static str = "disk_space";
    const PAUSE_OWNER_SCHEDULE: &'static str = "schedule";
    const QUEUE_COMPLETE_ACTION_SETTING: &'static str = "queue_complete_action";

    /// Create a new queue manager.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        servers: Vec<ServerConfig>,
        db: Database,
        incomplete_dir: std::path::PathBuf,
        complete_dir: std::path::PathBuf,
        log_buffer: LogBuffer,
        max_active_downloads: usize,
        categories: Vec<CategoryConfig>,
        min_free_space: u64,
        speed_limit_bps: u64,
        direct_unpack: bool,
        max_nested_archive_depth: u8,
        abort_hopeless: bool,
        early_failure_check: bool,
        required_completion_pct: f64,
        article_timeout_secs: u64,
    ) -> Arc<Self> {
        Self::new_with_postproc_limits(
            servers,
            db,
            incomplete_dir,
            complete_dir,
            log_buffer,
            max_active_downloads,
            PostProcLimits::default(),
            categories,
            min_free_space,
            speed_limit_bps,
            direct_unpack,
            max_nested_archive_depth,
            abort_hopeless,
            early_failure_check,
            required_completion_pct,
            article_timeout_secs,
        )
    }

    /// Create a queue manager with explicit post-processing worker limits.
    #[allow(clippy::too_many_arguments)]
    pub fn new_with_postproc_limits(
        servers: Vec<ServerConfig>,
        db: Database,
        incomplete_dir: std::path::PathBuf,
        complete_dir: std::path::PathBuf,
        log_buffer: LogBuffer,
        max_active_downloads: usize,
        postproc_limits: PostProcLimits,
        categories: Vec<CategoryConfig>,
        min_free_space: u64,
        speed_limit_bps: u64,
        direct_unpack: bool,
        max_nested_archive_depth: u8,
        abort_hopeless: bool,
        early_failure_check: bool,
        required_completion_pct: f64,
        article_timeout_secs: u64,
    ) -> Arc<Self> {
        use std::num::NonZeroU32;

        // Saturate rather than wrap: `as u32` turns 4 GiB/s + 1 into 1 B/s.
        let download_bps = NonZeroU32::new(u32::try_from(speed_limit_bps).unwrap_or(u32::MAX));
        let bandwidth = Arc::new(BandwidthLimiter::new(BandwidthConfig { download_bps }));

        let configured_servers = servers.clone();
        let servers_arc = Arc::new(Mutex::new(servers));
        let dispatch: Arc<dyn DispatchEngine> = Arc::new(DispatchHandle::new(
            Arc::clone(&servers_arc),
            Arc::clone(&bandwidth),
            article_timeout_secs,
        ));
        dispatch.start();

        let (add_tx, _) = broadcast::channel(64);

        Arc::new(Self {
            jobs: Mutex::new(HashMap::new()),
            job_order: Mutex::new(Vec::new()),
            servers: servers_arc,
            globally_paused: AtomicBool::new(false),
            progress_shutdown: watch::channel(false).0,
            pause_transition: Mutex::new(()),
            globally_paused_jobs: Mutex::new(HashSet::new()),
            pause_holds: Mutex::new(PauseHolds::default()),
            schedule_disabled_servers: Mutex::new(Vec::new()),
            configured_servers: Mutex::new(configured_servers),
            configured_speed_limit_bps: AtomicU64::new(speed_limit_bps),
            scheduled_speed_cap: Mutex::new(None),
            speed: SpeedTracker::new(),
            db: Mutex::new(db),
            incomplete_dir: Mutex::new(incomplete_dir),
            complete_dir: Mutex::new(complete_dir),
            pause_until: Mutex::new(None),
            history_retention: Mutex::new(None),
            history_update: AtomicU64::new(1),
            log_buffer: Some(log_buffer),
            add_tx,
            max_active_downloads: AtomicUsize::new(max_active_downloads),
            history_retries: Mutex::new(HashMap::new()),
            auto_sort_remaining_pct: AtomicBool::new(false),
            duplicate_policy: Mutex::new(DuplicatePolicy::default()),
            postproc_scripts: Mutex::new(PostProcScriptConfig::default()),
            notifier: Mutex::new(Arc::new(Notifier::disabled())),
            complete_action: Mutex::new(None),
            queue_complete_persist: AtomicBool::new(false),
            trust_inflight_verify: AtomicBool::new(true),
            postproc_resources: PostProcResourcePool::new(postproc_limits),
            categories: Mutex::new(categories),
            min_free_space: AtomicU64::new(min_free_space),
            bandwidth,
            direct_unpack_enabled: AtomicBool::new(direct_unpack),
            max_nested_archive_depth,
            dispatch,
            abort_hopeless,
            early_failure_check,
            required_completion_pct: required_completion_pct.clamp(100.0, 200.0),
            // Phase 6: 5-minute default. Long enough that a slow first
            // article doesn't accidentally abort a real download; short
            // enough that an obvious zombie is killed within minutes.
            no_progress_timeout: Mutex::new(Duration::from_secs(300)),
        })
    }

    /// Update category configs (e.g. after config reload).
    /// Post-processing level for a job: its own override (e.g. SABnzbd's
    /// `pp`), else its category's setting, else repair+unpack (3).
    pub fn post_processing_level(&self, job: &NzbJob) -> u8 {
        self.post_processing_level_for(job.pp_override, &job.category)
    }

    /// [`Self::post_processing_level`] from the job's fields, for callers
    /// that hold a borrow of another part of the job.
    fn post_processing_level_for(&self, pp_override: Option<u8>, category: &str) -> u8 {
        pp_override.unwrap_or_else(|| {
            self.categories
                .lock()
                .iter()
                .find(|c| c.name == category)
                .map(|c| c.post_processing)
                .unwrap_or(3) // default: repair+unpack
        })
    }

    pub fn set_categories(&self, categories: Vec<CategoryConfig>) {
        *self.categories.lock() = categories;
    }

    /// Get history retention limit (None = keep all).
    pub fn get_history_retention(&self) -> Option<usize> {
        *self.history_retention.lock()
    }

    fn persist_globally_paused_jobs(&self) {
        let mut ids: Vec<_> = self.globally_paused_jobs.lock().iter().cloned().collect();
        ids.sort();
        if let Ok(value) = serde_json::to_string(&ids) {
            self.db
                .lock()
                .set_setting(Self::GLOBAL_PAUSED_JOBS_SETTING, &value);
        }
    }

    /// Set history retention limit. `Some(0)` is normalized to `None`
    /// (keep all) so a zero can never wipe history on completion (GH #136).
    pub fn set_history_retention(&self, limit: Option<usize>) {
        *self.history_retention.lock() = normalize_history_retention(limit);
    }

    /// Current generation of the SAB-compatible history view.
    pub fn history_update(&self) -> u64 {
        self.history_update.load(Ordering::Acquire)
    }

    fn history_changed(&self) {
        // SABnzbd also uses a wrapping generation rather than a timestamp.
        // Keep zero reserved for clients that have never polled.
        // `fetch_update` is deprecated on newer stable in favour of
        // `try_update`, which the pinned toolchain does not have yet.
        #[allow(deprecated)]
        let _ = self
            .history_update
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                Some(if current == u64::MAX { 1 } else { current + 1 })
            });
    }

    /// Subscribe to job addition events. The receiver fires immediately when
    /// a job is accepted into the queue, before download begins.
    pub fn subscribe_additions(&self) -> broadcast::Receiver<JobAddedEvent> {
        self.add_tx.subscribe()
    }

    /// Per-server `(server_id, active, limit)` triples for the live NNTP
    /// connection pool. `active` is by-construction `<= limit` because the
    /// pool is semaphore-backed.
    pub fn connection_snapshot(&self) -> Vec<(String, usize, usize)> {
        self.dispatch.connection_snapshot()
    }

    /// Per-server sockets actively transferring articles. Connected workers
    /// waiting for work are reported as free capacity.
    pub fn connected_snapshot(&self) -> Vec<(String, usize, usize)> {
        self.dispatch.active_connection_snapshot()
    }

    /// Total currently-held NNTP connection slots across all servers.
    pub fn connection_total(&self) -> usize {
        self.dispatch.connection_total()
    }

    /// Override the worker idle eviction threshold (Phase 5 watchdog).
    /// Test harnesses use this to make the eviction trigger in seconds.
    pub fn set_max_worker_idle(&self, d: std::time::Duration) {
        self.dispatch.set_max_worker_idle(d);
    }

    /// Lifetime count of worker evictions performed by the Phase 5 idle
    /// watchdog. Each increment means the supervisor reclaimed a worker
    /// that had stalled past `max_worker_idle`.
    pub fn worker_eviction_count(&self) -> u64 {
        self.dispatch.eviction_count()
    }

    /// Phase 6: override the time-based hopeless threshold. Tests use this
    /// to make the no-progress watchdog converge in seconds.
    pub fn set_no_progress_timeout(&self, d: std::time::Duration) {
        *self.no_progress_timeout.lock() = d;
    }

    /// Phase 6: scan all active jobs and abort any whose hopeless tracker
    /// reports a `no_progress_timeout` tier. Called from the speed-tracker
    /// tick (1Hz). Cheap when there's nothing to abort — just a tracker
    /// `Instant::elapsed()` per job.
    fn scan_for_no_progress_jobs(self: &Arc<Self>) {
        if !self.abort_hopeless {
            return;
        }
        let timeout = *self.no_progress_timeout.lock();

        // Snapshot the (job_id, abort) pairs under the jobs lock, then
        // release before calling abort_job (which re-acquires).
        let mut to_abort: Vec<(String, HopelessAbort)> = Vec::new();
        {
            let jobs = self.jobs.lock();
            for (id, state) in jobs.iter() {
                if !matches!(state.job.status, JobStatus::Downloading) {
                    continue;
                }
                if let Some(ref tracker) = state.hopeless_tracker
                    && let Some(abort) = tracker.time_based_check(timeout)
                {
                    to_abort.push((id.clone(), abort));
                }
            }
        }

        for (job_id, abort) in to_abort {
            warn!(
                job_id = %job_id,
                tier = abort.tier,
                reason = %abort.reason,
                "Job is hopeless (no-progress timeout) — aborting"
            );
            {
                let mut jobs = self.jobs.lock();
                if let Some(state) = jobs.get_mut(&job_id) {
                    state.job.error_message = Some(abort.reason.clone());
                }
            }
            if !self.dispatch.abort_job(&job_id, abort.reason) {
                crate::increment_counter("jobs.duplicate_terminal_attempts");
                debug!(job_id = %job_id, "Duplicate terminal abort request ignored");
            }
        }
    }

    /// Set max active downloads.
    ///
    /// Raising the limit (or setting it to 0, which means unlimited) starts
    /// queued jobs up to the new capacity. Lowering it requeues the extra
    /// active downloads — lowest priority, and within a priority the newest
    /// in queue order — so the active count matches the setting. A requeued
    /// job is paused in the dispatcher, which keeps its context and progress;
    /// it resumes through the normal slot claim once capacity frees up.
    pub fn set_max_active_downloads(self: &Arc<Self>, max: usize) {
        self.max_active_downloads.store(max, Ordering::Relaxed);
        self.requeue_over_limit();
        self.start_next_queued();
    }

    /// Move downloading jobs above `max_active_downloads` back to `Queued`.
    ///
    /// `max == 0` is unlimited and requeues nothing. Selection mirrors
    /// preemption: the job with the lowest priority loses its slot, and ties
    /// go to the one furthest down the queue (newest). The dispatcher's
    /// pause keeps the in-flight context, so progress is not discarded.
    fn requeue_over_limit(self: &Arc<Self>) {
        let max = self.max_active_downloads.load(Ordering::Relaxed);
        if max == 0 {
            return;
        }
        loop {
            let victim = {
                let jobs = self.jobs.lock();
                let order = self.job_order.lock();
                let active = jobs
                    .values()
                    .filter(|s| s.job.status == JobStatus::Downloading)
                    .count();
                if active <= max {
                    return;
                }
                let mut worst: Option<(String, u8, usize)> = None;
                for (idx, id) in order.iter().enumerate() {
                    if let Some(state) = jobs.get(id)
                        && state.job.status == JobStatus::Downloading
                    {
                        let priority = state.job.priority as u8;
                        if worst.as_ref().is_none_or(|(_, wp, widx)| {
                            priority < *wp || (priority == *wp && idx > *widx)
                        }) {
                            worst = Some((id.clone(), priority, idx));
                        }
                    }
                }
                worst.map(|(id, _, _)| id)
            };
            let Some(id) = victim else {
                return;
            };
            info!(
                job_id = %id,
                max_downloads = max,
                "Requeueing active download: concurrent-jobs limit lowered"
            );
            // Pause first so workers stop pulling this job's articles. The
            // context stays in the dispatcher; resume unpauses it.
            self.dispatch.pause_job(&id);
            let mut jobs = self.jobs.lock();
            let Some(state) = jobs.get_mut(&id) else {
                continue;
            };
            if state.job.status != JobStatus::Downloading {
                continue;
            }
            state.job.status = JobStatus::Queued;
            state.persisted_status = Some(JobStatus::Queued);
            let db = self.db.lock();
            let _ = db.queue_update_progress(
                &id,
                JobStatus::Queued,
                state.job.downloaded_bytes,
                state.job.articles_downloaded,
                state.job.articles_failed,
                state.job.files_completed,
            );
        }
    }

    /// Set what admission does with an NZB that matches a queued or history
    /// job by name or content digest.
    pub fn set_duplicate_policy(&self, policy: DuplicatePolicy) {
        *self.duplicate_policy.lock() = policy;
    }

    pub fn duplicate_policy(&self) -> DuplicatePolicy {
        *self.duplicate_policy.lock()
    }

    /// Enable or disable automatic remaining-percentage ordering.
    pub fn set_auto_sort_remaining_pct(&self, enabled: bool) {
        self.auto_sort_remaining_pct
            .store(enabled, Ordering::Relaxed);
    }

    pub fn auto_sort_remaining_pct(&self) -> bool {
        self.auto_sort_remaining_pct.load(Ordering::Relaxed)
    }

    /// Configure the optional success and failure hooks used after
    /// post-processing. Script paths are resolved and confined when a job
    /// invokes them; keeping the raw config here allows live updates without
    /// rebuilding the queue manager.
    pub fn set_postproc_scripts(
        &self,
        scripts_dir: Option<std::path::PathBuf>,
        success: Option<std::path::PathBuf>,
        failure: Option<std::path::PathBuf>,
        timeout_secs: u64,
        max_output_bytes: usize,
    ) {
        *self.postproc_scripts.lock() = PostProcScriptConfig {
            scripts_dir,
            success,
            failure,
            timeout: Duration::from_secs(timeout_secs.max(1)),
            max_output_bytes,
        };
    }

    /// Replace the notification targets. Takes effect for events raised after
    /// the call; in-flight deliveries finish with the previous targets.
    pub fn set_notifications(&self, config: &NotificationConfig) {
        *self.notifier.lock() = Arc::new(Notifier::from_config(config));
    }

    /// Send a notification about a job or the disk guard, if any target is set.
    fn notify(&self, event: NotificationEvent) {
        let notifier = Arc::clone(&self.notifier.lock());
        notifier.notify(event);
    }

    /// The action `change_complete_action` set, to run once the queue drains.
    pub fn queue_complete_action(&self) -> Option<String> {
        self.complete_action.lock().clone()
    }

    /// Set `change_complete_action`; `None` or an empty value clears it. The
    /// only action RustNZB runs is `script_<name>`, a script from
    /// `scripts_dir`, which is checked here so a bad name is refused up front.
    /// Shutdown, hibernate and standby are refused: RustNZB cannot act on the
    /// host, and accepting them would silently do nothing.
    pub fn set_queue_complete_action(&self, action: Option<&str>) -> Result<(), String> {
        let action = action
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string);
        if let Some(action) = action.as_deref() {
            let Some(script) = action.strip_prefix("script_") else {
                return Err(format!(
                    "unsupported complete action '{action}': only script_<name> is supported"
                ));
            };
            let scripts_dir = self.postproc_scripts.lock().scripts_dir.clone();
            resolve_script_path(scripts_dir.as_deref(), std::path::Path::new(script))
                .map_err(|error| format!("unable to use script '{script}': {error}"))?;
        }
        self.store_queue_complete_action(action);
        Ok(())
    }

    /// Persist and apply the complete action (`None` clears it).
    fn store_queue_complete_action(&self, action: Option<String>) {
        self.db.lock().set_setting(
            Self::QUEUE_COMPLETE_ACTION_SETTING,
            action.as_deref().unwrap_or_default(),
        );
        *self.complete_action.lock() = action;
    }

    /// Set `queue_complete_pers`, from the `general` config.
    pub fn set_queue_complete_persist(&self, persist: bool) {
        self.queue_complete_persist.store(persist, Ordering::SeqCst);
    }

    /// `par2.trust_inflight_verify`: false makes post-processing verify every
    /// PAR2 set from disk. Applies to post-processing started afterwards.
    pub fn set_trust_inflight_verify(&self, on: bool) {
        self.trust_inflight_verify.store(on, Ordering::SeqCst);
    }

    /// `par2.defer_volumes`: hold PAR2 recovery volumes until the slice map
    /// shows damage. Applies to downloads started afterwards.
    pub fn set_defer_par2_volumes(&self, on: bool) {
        self.dispatch.set_defer_par2_volumes(on);
    }

    /// Start the complete action in the background, for callers that are not
    /// async. Without a Tokio runtime there is nothing to run it on, so the
    /// action is skipped.
    fn spawn_queue_complete_action(self: &Arc<Self>) {
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        let qm = Arc::clone(self);
        tokio::spawn(async move { qm.run_queue_complete_action().await });
    }

    /// Stable sort of the queue by remaining work percentage. The original
    /// queue order is retained for equal percentages, which keeps repeated
    /// manual sorts deterministic and avoids active-job churn.
    pub fn sort_by_remaining_percentage(&self, ascending: bool) {
        self.sort_queue(QueueSortField::Remaining, ascending);
    }

    /// Stable sort of the queue by `field`. Only the queue order changes:
    /// job status is untouched, so active downloads keep running, and the
    /// original order is retained for equal keys, which keeps repeated
    /// manual sorts deterministic and avoids active-job churn.
    pub fn sort_queue(&self, field: QueueSortField, ascending: bool) {
        let jobs = self.jobs.lock();
        let mut order = self.job_order.lock();
        order.sort_by(|left, right| {
            let left = jobs.get(left).map(|state| &state.job);
            let right = jobs.get(right).map(|state| &state.job);
            let ordering = match (left, right) {
                (Some(left), Some(right)) => field.compare(left, right),
                // Ids without a job state keep their relative position.
                _ => std::cmp::Ordering::Equal,
            };
            if ascending {
                ordering
            } else {
                ordering.reverse()
            }
        });
    }

    /// Get max active downloads.
    pub fn get_max_active_downloads(&self) -> usize {
        self.max_active_downloads.load(Ordering::Relaxed)
    }

    /// Configured and observed post-processing concurrency.
    pub fn postproc_resource_snapshot(&self) -> PostProcResourceSnapshot {
        self.postproc_resources.snapshot()
    }

    /// Set the configured download speed limit in bytes per second
    /// (0 = unlimited).
    ///
    /// This is the user's value. An active schedule cap overrides it until
    /// the window ends, at which point this value comes back.
    pub fn set_speed_limit(&self, bps: u64) {
        self.configured_speed_limit_bps.store(bps, Ordering::SeqCst);
        self.apply_effective_speed_limit();
    }

    /// The user's configured speed limit, before any schedule cap.
    pub fn configured_speed_limit(&self) -> u64 {
        self.configured_speed_limit_bps.load(Ordering::SeqCst)
    }

    /// Apply the schedule's cap over the configured limit. `None` lifts it.
    fn set_scheduled_speed_cap(&self, cap: Option<u64>) {
        *self.scheduled_speed_cap.lock() = cap;
        self.apply_effective_speed_limit();
    }

    fn apply_effective_speed_limit(&self) {
        use std::num::NonZeroU32;
        let effective = self
            .scheduled_speed_cap
            .lock()
            .unwrap_or(self.configured_speed_limit());
        // Saturate rather than wrap: `as u32` turns 4 GiB/s + 1 into 1 B/s.
        let limit = NonZeroU32::new(u32::try_from(effective).unwrap_or(u32::MAX));
        self.bandwidth.set_download_bps(limit);
    }

    /// Get the current download speed limit in bytes per second (0 = unlimited).
    pub fn get_speed_limit(&self) -> u64 {
        self.bandwidth
            .get_download_bps()
            .map(|v| v.get() as u64)
            .unwrap_or(0)
    }

    /// Count currently downloading jobs.
    #[allow(dead_code)]
    fn active_download_count(&self) -> usize {
        let jobs = self.jobs.lock();
        jobs.values()
            .filter(|s| s.job.status == JobStatus::Downloading)
            .count()
    }

    /// Atomically find the next queued job that can start, mark it as
    /// `Downloading` in the jobs map, and return its ID.
    ///
    /// Returns `None` if no download slot is available or there are no
    /// queued jobs.  Because the status transition happens under the same
    /// lock acquisition as the active-count check, concurrent callers
    /// cannot both claim the same slot (no TOCTOU race).
    fn claim_next_download_slot(&self, max: usize) -> Option<String> {
        if self.globally_paused.load(Ordering::Relaxed) {
            return None;
        }

        let mut jobs = self.jobs.lock();
        let active = jobs
            .values()
            .filter(|s| s.job.status == JobStatus::Downloading)
            .count();
        if max > 0 && active >= max {
            return None;
        }

        let order = self.job_order.lock();
        let mut best: Option<(String, u8)> = None;
        for id in order.iter() {
            if let Some(s) = jobs.get(id)
                && s.job.status == JobStatus::Queued
            {
                let p = s.job.priority as u8;
                if best.as_ref().is_none_or(|(_, bp)| p > *bp) {
                    best = Some((id.clone(), p));
                }
            }
        }
        let (id, _) = best?;

        // Mark as Downloading while still holding the lock
        if let Some(state) = jobs.get_mut(&id) {
            state.job.status = JobStatus::Downloading;
            info!(job_id = %id, name = %state.job.name, "Starting queued job");
        }
        Some(id)
    }

    /// Start queued jobs up to the concurrency limit.
    fn start_next_queued(self: &Arc<Self>) {
        let max = self.max_active_downloads.load(Ordering::Relaxed);
        while let Some(job_id) = self.claim_next_download_slot(max) {
            if self.dispatch.has_job(&job_id) {
                // A job resumed while the active limit was reached keeps its
                // paused context in the pool; unpause it instead of
                // submitting its work a second time.
                self.resume_queued_context(&job_id);
            } else {
                self.launch_download(&job_id);
            }
        }
    }

    /// Unpause a queued job whose download context is still held (paused)
    /// by the worker pool. The caller has already marked it `Downloading`.
    fn resume_queued_context(&self, job_id: &str) {
        {
            let mut jobs = self.jobs.lock();
            if let Some(state) = jobs.get_mut(job_id) {
                state.job.error_message = None;
                state.failure_code = None;
                // The paused interval must not count toward the no-progress
                // watchdog (GH #123).
                if let Some(tracker) = state.hopeless_tracker.as_mut() {
                    tracker.reset_progress_clock();
                }
                state.persisted_status = Some(JobStatus::Downloading);
                let db = self.db.lock();
                let _ = db.queue_update_progress(
                    job_id,
                    JobStatus::Downloading,
                    state.job.downloaded_bytes,
                    state.job.articles_downloaded,
                    state.job.articles_failed,
                    state.job.files_completed,
                );
            }
        }
        self.dispatch.resume_job(job_id);
    }

    /// Add a job to the queue and start downloading if a slot is available.
    ///
    /// The job should already have its `work_dir` and `output_dir` set.
    /// The job is always inserted as `Queued` (or `Paused` if globally
    /// paused) first, then `start_next_queued` is called to atomically
    /// claim a download slot if one is available.  This eliminates the
    /// TOCTOU race that previously allowed concurrent callers to exceed
    /// the `max_active_downloads` limit.
    pub fn add_job(
        self: &Arc<Self>,
        job: NzbJob,
        nzb_data: Option<Vec<u8>>,
    ) -> crate::nzb_core::Result<()> {
        self.admit_job(job, nzb_data, true)
    }

    /// Shared admission path for [`Self::add_job`] and history retries. A
    /// retry passes `enforce_duplicates: false`, because its own history row
    /// would otherwise match the job it is re-queuing.
    fn admit_job(
        self: &Arc<Self>,
        job: NzbJob,
        nzb_data: Option<Vec<u8>>,
        enforce_duplicates: bool,
    ) -> crate::nzb_core::Result<()> {
        if crate::nzb_core::path::safe_component(&job.category).is_none() {
            return Err(crate::nzb_core::NzbError::Other(
                "category must be a single safe path component".to_string(),
            ));
        }
        crate::nzb_core::path::safe_component(&job.name).ok_or_else(|| {
            crate::nzb_core::NzbError::Other(
                "job name must be a single safe path component".to_string(),
            )
        })?;
        let complete_root = self.complete_dir();
        let configured_root = self
            .categories
            .lock()
            .iter()
            .find(|category| category.name == job.category)
            .and_then(|category| category.output_dir.clone());
        let output_is_allowed = job.output_dir.starts_with(&complete_root)
            || configured_root
                .as_ref()
                .is_some_and(|root| job.output_dir.starts_with(root));
        if !output_is_allowed {
            return Err(crate::nzb_core::NzbError::Other(
                "job output directory is outside configured storage roots".to_string(),
            ));
        }
        // Persist to DB. The duplicate check runs under the same lock as the
        // insert, so two identical uploads cannot both pass it.
        {
            let db = self.db.lock();
            if enforce_duplicates {
                let digest = nzb_data.as_deref().map(nzb_digest);
                self.check_duplicate_admission(&db, &job, digest.as_deref())?;
            }
            // Ensure work directory exists. This follows the check so a
            // refused NZB leaves no directory behind.
            std::fs::create_dir_all(&job.work_dir)?;
            db.queue_insert(&job)?;
            // Store raw NZB data if available
            if let Some(ref data) = nzb_data {
                let _ = db.queue_store_nzb_data(&job.id, data);
            }
            // A history retry starts from the failed attempt's checkpoint.
            // Persist it now so a restart before the first progress write
            // keeps both the resumed articles and the carried-over bytes.
            if job.downloaded_bytes > 0 {
                let mut checkpoint = checkpoint_for_job(&job, false);
                checkpoint.carried_bytes = job.downloaded_bytes;
                if let Ok(data) = serde_json::to_vec(&checkpoint) {
                    let _ = db.queue_store_job_data(&job.id, &data);
                }
            }
        }

        self.activate_admitted_job(job, nzb_data);
        Ok(())
    }

    /// Admit one NZB exactly once for a caller-provided idempotency key.
    ///
    /// A first admission inserts the queue row, the durable admission binding,
    /// and activates the job. A replay with the same key and identical payload
    /// returns the existing admission without creating a second job; a replay
    /// with a different payload is a conflict.
    pub fn add_job_idempotent(
        self: &Arc<Self>,
        job: NzbJob,
        nzb_data: Vec<u8>,
        idempotency_key: &str,
        payload_digest: &str,
    ) -> crate::nzb_core::Result<QueueAdmissionOutcome> {
        if let Some(observation) = self.db.lock().queue_admission_observe(idempotency_key)? {
            if observation.admission.payload_digest != payload_digest {
                return Err(crate::nzb_core::NzbError::AdmissionConflict);
            }
            return Ok(QueueAdmissionOutcome::Existing(observation.admission));
        }

        let outcome = {
            let mut db = self.db.lock();
            // The policy applies to a first admission only. A key that is
            // already bound is answered as a replay, not as a duplicate.
            if db.queue_admission_observe(idempotency_key)?.is_none() {
                self.check_duplicate_admission(&db, &job, Some(&nzb_digest(&nzb_data)))?;
            }
            std::fs::create_dir_all(&job.work_dir)?;
            db.queue_admit(&job, &nzb_data, idempotency_key, payload_digest)?
        };
        match &outcome {
            QueueAdmissionOutcome::Inserted(_) => {
                self.activate_admitted_job(job, Some(nzb_data));
            }
            QueueAdmissionOutcome::Existing(_) => {
                if let Err(error) = std::fs::remove_dir(&job.work_dir)
                    && error.kind() != std::io::ErrorKind::NotFound
                {
                    warn!(
                        work_dir = %job.work_dir.display(),
                        "Unable to remove an unused replay work directory: {error}"
                    );
                }
            }
        }
        Ok(outcome)
    }

    /// Observe one durable admission without scanning bounded queue or history lists.
    pub fn queue_admission_observe(
        &self,
        idempotency_key: &str,
    ) -> crate::nzb_core::Result<Option<QueueAdmissionObservation>> {
        self.db.lock().queue_admission_observe(idempotency_key)
    }

    /// Apply the duplicate policy to an incoming job. The caller holds the
    /// database lock, so the match cannot change before the job is inserted.
    /// `Skip` refuses the job; `Warn` admits it and records the match.
    fn check_duplicate_admission(
        &self,
        db: &Database,
        job: &NzbJob,
        digest: Option<&str>,
    ) -> crate::nzb_core::Result<()> {
        let policy = self.duplicate_policy();
        if policy == DuplicatePolicy::Allow {
            return Ok(());
        }
        let matches = db.find_duplicate_candidates(&job.name, digest)?;
        let Some(first) = matches
            .iter()
            .find(|candidate| candidate.by_digest)
            .or(matches.first())
        else {
            return Ok(());
        };
        let location = match first.location {
            DuplicateLocation::Queue => "queue",
            DuplicateLocation::History => "history",
        };
        let matched_by = match (first.by_name, first.by_digest) {
            (true, true) => "name and content",
            (true, false) => "name",
            _ => "content",
        };
        if policy == DuplicatePolicy::Skip {
            crate::increment_counter("jobs.duplicate_refused");
            return Err(crate::nzb_core::NzbError::DuplicateNzb(format!(
                "\"{}\" duplicates {location} job \"{}\" by {matched_by}",
                job.name, first.name
            )));
        }
        crate::increment_counter("jobs.duplicate_admitted");
        warn!(
            name = %job.name,
            matched_id = %first.id,
            matched_name = %first.name,
            location,
            matched_by,
            "NZB duplicates an existing job; admitted because duplicate_policy is warn"
        );
        Ok(())
    }

    /// Activate an already-persisted job: emit the added event and place it in
    /// the in-memory queue (or paused set), starting a download slot if free.
    /// The queue row must already exist — `add_job` and `add_job_idempotent`
    /// persist it before calling this.
    fn activate_admitted_job(self: &Arc<Self>, mut job: NzbJob, nzb_data: Option<Vec<u8>>) {
        let job_id = job.id.clone();
        info!(
            job_id = %job_id,
            name = %job.name,
            files = job.file_count,
            articles = job.article_count,
            "Job added to queue"
        );

        let _ = self.add_tx.send(JobAddedEvent {
            id: job_id.clone(),
            name: job.name.clone(),
            category: job.category.clone(),
            nzb_data: nzb_data.as_ref().map(|data| Arc::new(data.clone())),
        });

        // A job admitted as Paused (e.g. SABnzbd priority -2) stays paused
        // individually: it must not start, and Resume All must not resume it.
        let requested_paused = job.status == JobStatus::Paused;
        // Only a history retry is admitted with progress: the bytes it
        // carries in from the failed attempt's checkpoint.
        let carried_bytes = job.downloaded_bytes;

        // If globally paused, add as paused
        if requested_paused || self.globally_paused.load(Ordering::Relaxed) {
            job.status = JobStatus::Paused;
            let state = JobState {
                job,
                progress_handle: None,
                speed: Arc::new(SpeedTracker::new()),
                nzb_data,
                direct_unpacker: None,
                hopeless_tracker: None,
                repair_fold: None,
                download_time_secs: None,
                prior_download_secs: 0.0,
                carried_bytes,
                failure_code: None,
                par2_volumes: None,
                output_dir_claimed: false,
                persisted_status: None,
            };
            self.jobs.lock().insert(job_id.clone(), state);
            if !requested_paused {
                self.globally_paused_jobs.lock().insert(job_id.clone());
                self.persist_globally_paused_jobs();
            }
            self.job_order.lock().push(job_id);
            return;
        }

        // Insert as Queued — start_next_queued will atomically claim a
        // download slot if one is available.
        job.status = JobStatus::Queued;
        let state = JobState {
            job,
            progress_handle: None,
            speed: Arc::new(SpeedTracker::new()),
            nzb_data,
            direct_unpacker: None,
            hopeless_tracker: None,
            repair_fold: None,
            download_time_secs: None,
            prior_download_secs: 0.0,
            carried_bytes,
            failure_code: None,
            par2_volumes: None,
            output_dir_claimed: false,
            persisted_status: None,
        };
        self.jobs.lock().insert(job_id.clone(), state);
        self.job_order.lock().push(job_id);

        // Try to start this or other queued jobs
        self.start_next_queued();
    }

    /// Retry one history entry at most once at a time.
    ///
    /// The first caller claims the entry, rebuilds the job (reusing a
    /// retained partial work directory via [`Self::prepare_retry_job`]) and
    /// enqueues it. While that job is still in the queue -- queued, paused,
    /// downloading or post-processing -- any further retry of the same entry
    /// returns [`HistoryRetryOutcome::InProgress`] with the existing job id
    /// instead of enqueueing a duplicate. Once the job has left the queue the
    /// entry can be retried again.
    ///
    /// This is an in-flight guard, not the durable `Idempotency-Key`
    /// admission: a history entry stays retryable after its retry finishes,
    /// whereas an admission key binds exactly one job forever.
    pub fn retry_history_entry(
        self: &Arc<Self>,
        entry: &HistoryEntry,
    ) -> crate::nzb_core::Result<HistoryRetryOutcome> {
        let mut retries = self.history_retries.lock();
        {
            let jobs = self.jobs.lock();
            retries.retain(|_, job_id| jobs.contains_key(job_id));
            if let Some(job_id) = retries.get(&entry.id) {
                return Ok(HistoryRetryOutcome::InProgress(job_id.clone()));
            }
        }

        let Some(nzb_data) = self.history_get_nzb_data(&entry.id)? else {
            return Ok(HistoryRetryOutcome::NoNzbData);
        };
        let retry_data = self.history_get_retry_data(&entry.id)?;
        let job = self.prepare_retry_job(entry, &nzb_data, retry_data.as_deref())?;
        let job_id = job.id.clone();
        info!(
            name = %job.name,
            id = %job_id,
            original_id = %entry.id,
            "Retrying NZB from history"
        );
        self.admit_job(job, Some(nzb_data), false)?;
        retries.insert(entry.id.clone(), job_id.clone());
        Ok(HistoryRetryOutcome::Started(job_id))
    }

    /// Rebuild a history job for retry. Newer history rows carry a checkpoint
    /// and retain a partial work directory when at least one article was
    /// written, so the dispatcher can enqueue only unresolved articles and
    /// append them to the existing assembled files. Older rows, and rows
    /// whose partial directory is gone, deliberately fall back to a full
    /// retry.
    pub fn prepare_retry_job(
        &self,
        entry: &HistoryEntry,
        nzb_data: &[u8],
        retry_data: Option<&[u8]>,
    ) -> crate::nzb_core::Result<NzbJob> {
        let parse_started = std::time::Instant::now();
        let mut job = tracing::info_span!("nzb_parse", name = %entry.name, bytes = nzb_data.len())
            .in_scope(|| nzb_parser::parse_nzb(&entry.name, nzb_data))?;
        crate::record_stage("parse", parse_started.elapsed().as_secs_f64());
        job.category = entry.category.clone();
        job.output_dir = self.output_dir_for(&job.category, &job.name)?;
        job.work_dir = self.incomplete_dir().join(&job.id);

        if entry.status == JobStatus::Failed
            && let Some(data) = retry_data
            && let Ok(checkpoint) = serde_json::from_slice::<JobCheckpoint>(data)
            && let Some(work_dir) = checkpoint.work_dir.as_ref()
            && std::fs::canonicalize(self.incomplete_dir())
                .ok()
                .zip(std::fs::canonicalize(work_dir).ok())
                .is_some_and(|(root, retained)| retained.starts_with(root))
            && std::fs::symlink_metadata(work_dir)
                .map(|metadata| metadata.file_type().is_dir())
                .unwrap_or(false)
        {
            apply_checkpoint(&mut job, &checkpoint);
            job.articles_failed = 0;
            // A retry re-attempts every missing article. Forget the servers
            // that failed it last time: the dispatcher treats an article with
            // `tried_servers` as an already-resolved failure.
            for article in job
                .files
                .iter_mut()
                .flat_map(|file| file.articles.iter_mut())
            {
                if !article.downloaded {
                    article.tried_servers.clear();
                    article.tries = 0;
                }
            }
            job.work_dir = work_dir.clone();
        }

        Ok(job)
    }

    /// Launch the download task for a job that is already in the jobs map
    /// with status `Downloading`.
    ///
    /// Builds a [`JobContext`] and submits work items to the shared worker
    /// pool, then spawns the per-job progress listener. If the pre-flight
    /// disk-space check fails, the job is set to `Paused`.
    fn launch_download(self: &Arc<Self>, job_id: &str) {
        // Lazily load NZB data if not in memory (queued jobs skip loading at restore time)
        {
            let mut jobs = self.jobs.lock();
            if let Some(state) = jobs.get_mut(job_id)
                && state.nzb_data.is_none()
            {
                let db = self.db.lock();
                if let Some(data) = db.queue_get_nzb_data(job_id).unwrap_or(None) {
                    // Parse NZB to populate files/articles
                    match nzb_parser::parse_nzb(&state.job.name, &data) {
                        Ok(parsed) => {
                            state.job.files = parsed.files;
                            // Apply checkpoint if available
                            if let Some(cp_data) = db.queue_load_job_data(job_id).unwrap_or(None)
                                && let Ok(checkpoint) =
                                    serde_json::from_slice::<JobCheckpoint>(&cp_data)
                            {
                                apply_checkpoint(&mut state.job, &checkpoint);
                                (state.prior_download_secs, state.carried_bytes) =
                                    restore_checkpoint_accounting(&mut state.job, &checkpoint);
                                if !state.output_dir_claimed {
                                    state.output_dir_claimed =
                                        restore_claimed_output_dir(&mut state.job, &checkpoint);
                                }
                                info!(
                                    job_id = %job_id,
                                    name = %state.job.name,
                                    articles_downloaded = state.job.articles_downloaded,
                                    "Lazy-loaded job checkpoint"
                                );
                            }
                        }
                        Err(e) => {
                            warn!(job_id = %job_id, "Failed to lazy-load NZB data: {e}");
                        }
                    }
                    state.nzb_data = Some(data);
                }
            }
        }

        // Read job data from the map (we need a copy for the spawned task)
        let (job, _nzb_data) = {
            let jobs = self.jobs.lock();
            let Some(state) = jobs.get(job_id) else {
                return;
            };
            (state.job.clone(), state.nzb_data.clone())
        };

        // Pre-flight disk space check
        let incomplete_dir = self.incomplete_dir();
        let output_dir = job.output_dir.clone();
        let free = get_disk_free(&incomplete_dir);
        if !disk_space_available(
            self.min_free_space(),
            [incomplete_dir.as_path(), output_dir.as_path()].as_slice(),
        ) {
            warn!(
                job_id = %job_id,
                free_bytes = free,
                min_free_space = self.min_free_space(),
                "Paused job due to low disk space on a job storage volume"
            );
            let mut jobs = self.jobs.lock();
            let mut alert = None;
            if let Some(state) = jobs.get_mut(job_id) {
                // Notify once per hold, not on every re-check of a held job.
                if state.failure_code != Some(JobFailureCode::StorageUnavailable) {
                    alert = Some(
                        NotificationEvent::new(
                            NotificationKind::DiskFull,
                            "Disk space low",
                            "Downloads paused: free space on a storage volume is below the minimum.",
                        )
                        .for_job(&state.job.name, &state.job.category),
                    );
                }
                state.job.status = JobStatus::Paused;
                state.job.error_message = Some("Paused: low disk space".to_string());
                state.failure_code = Some(JobFailureCode::StorageUnavailable);
            }
            drop(jobs);
            if let Some(alert) = alert {
                self.notify(alert);
            }
            return;
        }

        // Dispatch/queue wait: time since the job was queued. A job resumed
        // after a restart counts from its original add time.
        let queue_wait = (chrono::Utc::now() - job.added_at)
            .to_std()
            .unwrap_or_default();
        crate::record_stage("queue_wait", queue_wait.as_secs_f64());
        info!(
            job_id = %job_id,
            name = %job.name,
            queue_wait_secs = queue_wait.as_secs_f64(),
            total_bytes = job.total_bytes,
            article_count = job.article_count,
            file_count = job.file_count,
            "Starting download job"
        );

        let job_speed = Arc::new(SpeedTracker::new());
        // Phase 7: bounded progress channel. The handler reads at ~articles
        // per second; under DB-lock contention or post-processing pauses it
        // can fall behind. Unbounded was a memory hazard. With a 10K cap
        // the worst case is bounded buffering plus a `WARN` from
        // `try_send_or_warn` when the channel is full.
        let (progress_tx, progress_rx) = mpsc::channel::<ProgressUpdate>(
            nzb_dispatch::download_engine::PROGRESS_CHANNEL_CAPACITY,
        );

        {
            let srv = self.servers.lock();
            let enabled_count = srv.iter().filter(|s| s.enabled).count();
            info!(
                job_id = %job_id,
                total_servers = srv.len(),
                enabled_servers = enabled_count,
                "Dispatching job to shared worker pool"
            );
            if enabled_count == 0 {
                warn!(job_id = %job_id, "No enabled servers — job will stall until servers are added");
            }
        }

        self.dispatch.submit_job(&job, progress_tx);

        // Spawn the per-job progress handler and record its handle.
        let qm = Arc::clone(self);
        let jid = job_id.to_string();
        let speed_for_task = Arc::clone(&job_speed);
        let progress_handle = tokio::spawn(async move {
            qm.handle_progress(jid, progress_rx, speed_for_task).await;
        });

        // Update the existing map entry with the handle and trackers.
        {
            let mut jobs = self.jobs.lock();
            if let Some(state) = jobs.get_mut(job_id) {
                state.progress_handle = Some(progress_handle);
                state.speed = Arc::clone(&job_speed);
                let tracker = HopelessTracker::new(&state.job);
                info!(
                    job_id = %job_id,
                    par2_recovery_volumes = tracker.recovery_capacity_by_set.len(),
                    par2_recovery_blocks = tracker.recovery_blocks_total,
                    par2_recovery_bytes = tracker.recovery_capacity_bytes,
                    content_bytes = tracker.content_bytes,
                    "PAR2 recovery capacity initialized"
                );
                state.hopeless_tracker = Some(tracker);
            }
        }
    }

    /// Handle progress updates from the download engine.
    async fn handle_progress(
        self: Arc<Self>,
        job_id: String,
        mut progress_rx: mpsc::Receiver<ProgressUpdate>,
        job_speed: Arc<SpeedTracker>,
    ) {
        let mut last_db_update = Instant::now();
        let mut shutdown = self.progress_shutdown.subscribe();
        let mut draining = false;
        let mut ledger = LedgerRecorder::default();

        loop {
            let update = tokio::select! {
                biased;
                update = progress_rx.recv() => match update {
                    Some(update) => update,
                    None => break,
                },
                _ = shutdown.wait_for(|stop| *stop), if !draining => {
                    // Graceful shutdown: the pool has stopped, but the job
                    // context still holds the sender, so the channel never
                    // closes on its own. Close it from this side; `recv`
                    // keeps yielding the events already queued and then
                    // returns `None`, so every finished article is recorded
                    // before shutdown writes the checkpoint.
                    progress_rx.close();
                    draining = true;
                    continue;
                }
            };
            // Write buffered ledger evidence before a terminal article or job
            // event, so it sees every outcome that led to it. Other events
            // leave the batch to fill.
            if matches!(
                update,
                ProgressUpdate::ArticleFailed { .. }
                    | ProgressUpdate::JobFinished { .. }
                    | ProgressUpdate::JobAborted { .. }
            ) {
                self.flush_ledger(&job_id, &mut ledger);
            }
            match update {
                ProgressUpdate::ArticleOutcome {
                    file_id,
                    segment_number,
                    message_id,
                    outcome,
                    ..
                } => {
                    ledger.push(file_id, segment_number, message_id, outcome);
                    if ledger.is_full() {
                        self.flush_ledger(&job_id, &mut ledger);
                    }
                }
                ProgressUpdate::WaitingForProviders { message, .. } => {
                    let mut changed = false;
                    {
                        let mut jobs = self.jobs.lock();
                        if let Some(state) = jobs.get_mut(&job_id)
                            && state.job.error_message.as_deref() != Some(&message)
                        {
                            // Keep the job downloading: provider recovery is
                            // automatic and this is not missing content.
                            state.job.error_message = Some(message.clone());
                            changed = true;
                        }
                    }
                    if changed
                        && let Err(error) = self
                            .db
                            .lock()
                            .queue_update_error_message(&job_id, Some(&message))
                    {
                        warn!(job_id = %job_id, "Failed to persist provider waiting status: {error}");
                    }
                }
                ProgressUpdate::ProvidersAvailable { .. } => {
                    let mut cleared = false;
                    {
                        let mut jobs = self.jobs.lock();
                        if let Some(state) = jobs.get_mut(&job_id)
                            && state.job.error_message.as_deref().is_some_and(|message| {
                                message.starts_with("Waiting for providers:")
                            })
                        {
                            state.job.error_message = None;
                            state.failure_code = None;
                            cleared = true;
                        }
                    }
                    if cleared
                        && let Err(error) = self.db.lock().queue_update_error_message(&job_id, None)
                    {
                        warn!(job_id = %job_id, "Failed to clear provider waiting status: {error}");
                    }
                }
                ProgressUpdate::ArticleComplete {
                    file_id,
                    segment_number,
                    decoded_bytes,
                    file_complete,
                    server_id,
                    yenc_filename,
                    ..
                } => {
                    self.speed.record(decoded_bytes);
                    job_speed.record(decoded_bytes);

                    // Update in-memory job state
                    {
                        let mut jobs = self.jobs.lock();
                        if let Some(state) = jobs.get_mut(&job_id) {
                            if let Some(yenc_name) = yenc_filename.as_deref() {
                                let clean_name = std::path::Path::new(yenc_name)
                                    .file_name()
                                    .and_then(|name| name.to_str())
                                    .unwrap_or(yenc_name);
                                let revealed_par2 =
                                    clean_name.to_ascii_lowercase().ends_with(".par2");
                                if !revealed_par2
                                    && nzb_dispatch::has_known_extension(clean_name)
                                    && let Some(tracker) = state.hopeless_tracker.as_mut()
                                {
                                    tracker.mark_file_classified(&file_id);
                                }
                                let file_info = state
                                    .job
                                    .files
                                    .iter()
                                    .find(|file| file.id == file_id)
                                    .filter(|file| revealed_par2 && !file.is_par2)
                                    .map(|file| (file.bytes, file.articles.len()));
                                if let Some((file_bytes, article_count)) = file_info {
                                    let (set_name, volume, blocks) =
                                        crate::nzb_core::nzb_parser::parse_par2_filename(
                                            clean_name,
                                        );
                                    if let Some(tracker) = state.hopeless_tracker.as_mut() {
                                        tracker.reclassify_as_par2(
                                            &file_id,
                                            file_bytes,
                                            article_count,
                                            volume,
                                            blocks,
                                            set_name.as_deref(),
                                        );
                                    }
                                    if let Some(file) =
                                        state.job.files.iter_mut().find(|file| file.id == file_id)
                                    {
                                        file.is_par2 = true;
                                        file.par2_setname = set_name;
                                        file.par2_vol = volume;
                                        file.par2_blocks = blocks;
                                    }
                                    info!(
                                        job_id = %job_id,
                                        file_id = %file_id,
                                        yenc_filename = %clean_name,
                                        recovery_blocks = blocks.unwrap_or_default(),
                                        "Obfuscated file reclassified as PAR2 from yEnc header"
                                    );
                                }
                            }
                            state.job.downloaded_bytes += decoded_bytes;
                            state.job.articles_downloaded += 1;

                            // Update per-server stats
                            if let Some(ref sid) = server_id {
                                let stats = &mut state.job.server_stats;
                                if let Some(ss) = stats.iter_mut().find(|s| s.server_id == *sid) {
                                    ss.articles_downloaded += 1;
                                    ss.bytes_downloaded += decoded_bytes;
                                } else {
                                    // Find server name from config
                                    let sname = self
                                        .servers
                                        .lock()
                                        .iter()
                                        .find(|s| s.id == *sid)
                                        .map(|s| s.name.clone())
                                        .unwrap_or_else(|| sid.clone());
                                    stats.push(ServerArticleStats {
                                        server_id: sid.clone(),
                                        server_name: sname,
                                        articles_downloaded: 1,
                                        articles_failed: 0,
                                        bytes_downloaded: decoded_bytes,
                                    });
                                }
                            }

                            let file_is_par2 = state
                                .job
                                .files
                                .iter()
                                .find(|f| f.id == file_id)
                                .is_some_and(|f| f.is_par2);
                            if let Some(ref mut tracker) = state.hopeless_tracker {
                                tracker.record_success(file_is_par2);
                            }

                            for file in &mut state.job.files {
                                if file.id == file_id {
                                    file.bytes_downloaded += decoded_bytes;
                                    for article in &mut file.articles {
                                        if article.segment_number == segment_number {
                                            article.downloaded = true;
                                            article.data_size = Some(decoded_bytes);
                                        }
                                    }
                                    if file_complete && !file.assembled {
                                        file.assembled = true;
                                        state.job.files_completed += 1;
                                        info!(
                                            job_id = %job_id,
                                            file = %file.filename,
                                            completed = state.job.files_completed,
                                            total = state.job.file_count,
                                            "File assembly complete"
                                        );

                                        // Direct unpack: feed completed RAR volumes to the
                                        // unpacker so extraction overlaps with download.
                                        if self.direct_unpack_enabled.load(Ordering::Relaxed)
                                            && state.job.articles_failed == 0
                                            // Download-only and repair-only jobs
                                            // deliver their archives unpacked.
                                            && self.post_processing_level_for(
                                                state.job.pp_override,
                                                &state.job.category,
                                            ) >= 2
                                            && let Some(vol_info) = parse_rar_volume(&file.filename)
                                        {
                                            if state.direct_unpacker.is_none() {
                                                // Direct unpack writes into the
                                                // complete folder, so reserve it now.
                                                // (`state.job.files` is borrowed by
                                                // the loop, so pass disjoint fields.)
                                                claim_output_dir_fields(
                                                    &state.job.id,
                                                    &mut state.job.output_dir,
                                                    &mut state.output_dir_claimed,
                                                );
                                                state.direct_unpacker = DirectUnpacker::new(
                                                    &state.job.work_dir,
                                                    &state.job.output_dir,
                                                    state.job.password.clone(),
                                                );
                                                if state.direct_unpacker.is_some() {
                                                    info!(
                                                        job_id = %job_id,
                                                        "Direct unpack enabled — starting RAR extraction during download"
                                                    );
                                                }
                                            }
                                            if let Some(ref du) = state.direct_unpacker {
                                                let path = state.job.work_dir.join(&file.filename);
                                                du.add_volume(
                                                    &vol_info.set_name,
                                                    vol_info.volume_number,
                                                    path,
                                                );
                                            }
                                        }
                                    }
                                    break;
                                }
                            }
                        }
                    }

                    // Batch DB writes (every 2 seconds)
                    if last_db_update.elapsed() >= Duration::from_secs(2) {
                        self.persist_job_progress_blocking(&job_id).await;
                        last_db_update = Instant::now();
                    }
                    if self.auto_sort_remaining_pct() {
                        self.sort_by_remaining_percentage(true);
                    }
                }
                ProgressUpdate::ArticleFailed {
                    file_id,
                    segment_number,
                    failure,
                    ..
                } => {
                    let _ = &failure.message; // forwarded into logs below
                    let should_abort = {
                        let mut jobs = self.jobs.lock();
                        if let Some(state) = jobs.get_mut(&job_id) {
                            state.job.articles_failed += 1;

                            if let Some(article) = state
                                .job
                                .files
                                .iter_mut()
                                .find(|file| file.id == file_id)
                                .and_then(|file| {
                                    file.articles
                                        .iter_mut()
                                        .find(|article| article.segment_number == segment_number)
                                })
                            {
                                article.tries = article.tries.saturating_add(1);
                                if !article.tried_servers.contains(&failure.server_id) {
                                    article.tried_servers.push(failure.server_id.clone());
                                }
                            }

                            // Update per-server failed stats
                            let sid = &failure.server_id;
                            let stats = &mut state.job.server_stats;
                            if let Some(ss) = stats.iter_mut().find(|s| s.server_id == *sid) {
                                ss.articles_failed += 1;
                            } else {
                                let sname = self
                                    .servers
                                    .lock()
                                    .iter()
                                    .find(|s| s.id == *sid)
                                    .map(|s| s.name.clone())
                                    .unwrap_or_else(|| sid.clone());
                                stats.push(ServerArticleStats {
                                    server_id: sid.clone(),
                                    server_name: sname,
                                    articles_downloaded: 0,
                                    articles_failed: 1,
                                    bytes_downloaded: 0,
                                });
                            }

                            // Abort direct unpack on first article failure —
                            // PAR2 repair may be needed before extraction.
                            if let Some(du) = state.direct_unpacker.take() {
                                info!(
                                    job_id = %job_id,
                                    "Aborting direct unpack — article failure detected, falling back to normal pipeline"
                                );
                                du.abort();
                            }

                            // Track failure in hopeless detector and check
                            // whether this job can still complete.
                            if let Some(ref mut tracker) = state.hopeless_tracker {
                                let file_is_par2 = state
                                    .job
                                    .files
                                    .iter()
                                    .find(|f| f.id == file_id)
                                    .is_some_and(|f| f.is_par2);
                                // Use the NZB's declared segment size. Averaging
                                // a file across articles distorts the last segment
                                // and can flip decisions close to the reserve.
                                let declared_bytes = state
                                    .job
                                    .files
                                    .iter()
                                    .find(|f| f.id == file_id)
                                    .and_then(|f| {
                                        f.articles.iter().find(|article| {
                                            article.segment_number == segment_number
                                        })
                                    })
                                    .map(|article| article.bytes)
                                    .unwrap_or(0);
                                tracker.record_file_failure(
                                    Some(&file_id),
                                    file_is_par2,
                                    declared_bytes,
                                    failure.kind,
                                );
                                // Observability: dump tracker state on every
                                // failure so operators can see the ratio
                                // evolving towards hopeless-abort thresholds.
                                // Helps diagnose "why does this job take 4
                                // minutes to fail" — exposes grace period,
                                // early_failure, and ongoing_availability
                                // check progress in real time.
                                let effective_pct = if tracker.content_bytes > 0 {
                                    let avail: u64 = tracker
                                        .content_bytes
                                        .saturating_sub(tracker.content_bytes_missing)
                                        .saturating_add(
                                            tracker
                                                .recovery_capacity_bytes
                                                .saturating_sub(tracker.recovery_bytes_unavailable),
                                        );
                                    100.0 * (avail as f64 / tracker.content_bytes as f64)
                                } else {
                                    100.0
                                };
                                let fail_rate = if tracker.content_articles_checked > 0 {
                                    tracker.content_articles_failed as f64
                                        / tracker.content_articles_checked as f64
                                } else {
                                    0.0
                                };
                                debug!(
                                    job_id = %job_id,
                                    checked = tracker.content_articles_checked,
                                    failed = tracker.content_articles_failed,
                                    total = tracker.content_articles_total,
                                    failure_rate = format!("{fail_rate:.3}"),
                                    effective_completion_pct = format!("{effective_pct:.3}"),
                                    missing_content_bytes = tracker.content_bytes_missing,
                                    usable_recovery_bytes = tracker.recovery_capacity_bytes.saturating_sub(tracker.recovery_bytes_unavailable),
                                    recovery_blocks_total = tracker.recovery_blocks_total,
                                    recovery_blocks_unavailable = tracker.recovery_blocks_unavailable,
                                    required_pct = format!("{:.1}", self.required_completion_pct),
                                    kind = failure.kind.as_str(),
                                    "Hopeless tracker updated after article failure"
                                );
                                tracker.check(
                                    self.abort_hopeless,
                                    self.early_failure_check,
                                    self.required_completion_pct,
                                )
                            } else {
                                None
                            }
                        } else {
                            None
                        }
                    };

                    if let Some(abort) = should_abort {
                        warn!(
                            job_id = %job_id,
                            tier = abort.tier,
                            reason = %abort.reason,
                            "Job is hopeless — aborting"
                        );
                        {
                            let mut jobs = self.jobs.lock();
                            if let Some(state) = jobs.get_mut(&job_id) {
                                state.job.error_message = Some(abort.reason.clone());
                            }
                        }
                        // Tell the worker pool to drain the job and emit
                        // JobAborted — the JobAborted arm below handles the
                        // rest of the teardown.
                        if !self.dispatch.abort_job(&job_id, abort.reason) {
                            crate::increment_counter("jobs.duplicate_terminal_attempts");
                            debug!(job_id = %job_id, "Duplicate terminal abort request ignored");
                        }
                    } else {
                        warn!(
                            job_id = %job_id,
                            kind = failure.kind.as_str(),
                            server = %failure.server_id,
                            "Article failed: {}", failure.message
                        );
                    }
                }
                ProgressUpdate::JobFinished {
                    success,
                    articles_failed,
                    download_time_secs,
                    slice_map,
                    repair_fold,
                    ..
                } => {
                    // The slice map gives the exact damage: refuse to hand a
                    // release to PAR2 when it holds more damaged slices than
                    // recovery slices (WI-145).
                    let diagnosis = self
                        .abort_hopeless
                        .then(|| {
                            let map = slice_map.as_ref()?;
                            let jobs = self.jobs.lock();
                            let tracker = jobs.get(&job_id)?.hopeless_tracker.as_ref()?;
                            let declared = tracker
                                .recovery_blocks_known
                                .then(|| tracker.recovery_blocks_available());
                            parity_diagnosis(map, declared)
                        })
                        .flatten();
                    if let Some(diagnosis) = diagnosis {
                        warn!(
                            job_id = %job_id,
                            tier = "parity",
                            reason = %diagnosis,
                            "Job is hopeless — not repairable"
                        );
                        if let Some(state) = self.jobs.lock().get_mut(&job_id) {
                            state.job.slice_map = slice_map;
                        }
                        self.finish_aborted(
                            &job_id,
                            format!("Aborted: {diagnosis}"),
                            articles_failed,
                            download_time_secs,
                        )
                        .await;
                        break;
                    }
                    let repairable_damage = self.jobs.lock().get(&job_id).is_some_and(|state| {
                        state.hopeless_tracker.as_ref().is_some_and(|tracker| {
                            let usable = tracker
                                .recovery_capacity_bytes
                                .saturating_sub(tracker.recovery_bytes_unavailable);
                            usable >= tracker.content_bytes_missing
                                && tracker.content_articles_failed > 0
                        })
                    });
                    if repairable_damage {
                        crate::increment_counter("jobs.repairable_damage");
                    }
                    info!(
                        job_id = %job_id,
                        success,
                        articles_failed,
                        "Job download finished"
                    );

                    // The final article has resolved, so no worker can write
                    // another segment for this job. Release the worker-pool
                    // context now to close its persistent assembler handles
                    // before PAR2/unpack opens the completed files. Read the
                    // volumes it left held first, for the history row.
                    let held = self.dispatch.held_par2_files(&job_id).unwrap_or_default();
                    self.dispatch.release_completed_job(&job_id);

                    // Mark as PostProcessing immediately so the slot is freed
                    // for the next queued job. This lets the next download ramp
                    // up while post-processing (par2/unpack) runs concurrently.
                    {
                        let mut jobs = self.jobs.lock();
                        if let Some(state) = jobs.get_mut(&job_id) {
                            state.download_time_secs =
                                Some(state.prior_download_secs + download_time_secs);
                            state.job.status = JobStatus::PostProcessing;
                            state.job.completed_at = Some(chrono::Utc::now());
                            state.job.slice_map = slice_map;
                            state.repair_fold = repair_fold;
                            state.par2_volumes = par2_volume_counts(&state.job, &held);
                        }
                    }
                    self.history_changed();
                    self.start_next_queued();

                    self.on_job_finished(&job_id, success, articles_failed)
                        .await;
                    break;
                }
                ProgressUpdate::JobAborted {
                    reason,
                    articles_failed,
                    download_time_secs,
                    ..
                } => {
                    self.finish_aborted(&job_id, reason, articles_failed, download_time_secs)
                        .await;
                    break;
                }
            }
        }
        self.flush_ledger(&job_id, &mut ledger);
    }

    /// Tear down a job the download stopped as hopeless: record the failure
    /// with `reason` and move it straight to history, skipping PAR2,
    /// extraction and cleanup.
    async fn finish_aborted(
        self: &Arc<Self>,
        job_id: &str,
        reason: String,
        articles_failed: usize,
        download_time_secs: f64,
    ) {
        crate::increment_counter("jobs.hopeless_aborts");
        warn!(
            job_id = %job_id,
            reason = %reason,
            articles_failed,
            "Job aborted by download engine"
        );
        // The terminal update is emitted only after queued and
        // in-flight articles drain. Dropping this context closes
        // all assembler handles before history or any later stage
        // can inspect the work directory.
        self.dispatch.release_completed_job(job_id);
        let queue_drained = {
            let mut jobs = self.jobs.lock();
            let finished = if let Some(state) = jobs.get_mut(job_id) {
                state.download_time_secs = Some(state.prior_download_secs + download_time_secs);
                state.job.status = JobStatus::Failed;
                state.job.error_message = Some(reason.clone());
                state.failure_code = Some(JobFailureCode::ArticlesUnavailable);
                state.job.articles_failed = state.job.articles_failed.max(articles_failed);
                state.job.completed_at = Some(chrono::Utc::now());
                let tracker = state.hopeless_tracker.as_ref();
                warn!(
                    job_id = %job_id,
                    terminal_reason = %reason,
                    missing_content_bytes = tracker.map_or(0, |t| t.content_bytes_missing),
                    usable_recovery_bytes = tracker.map_or(0, |t| t.recovery_capacity_bytes.saturating_sub(t.recovery_bytes_unavailable)),
                    recovery_blocks_total = tracker.map_or(0, |t| t.recovery_blocks_total),
                    recovery_blocks_unavailable = tracker.map_or(0, |t| t.recovery_blocks_unavailable),
                    "Terminal job summary"
                );
                // Confirmed hopeless damage must not enter PAR2,
                // extraction, or cleanup. Persist the original
                // reason directly as a failed history row.
                self.move_to_history(state, Vec::new());
                true
            } else {
                false
            };
            finished && queue_drained_by(&jobs, job_id)
        };
        self.persist_job_progress_blocking(job_id).await;
        self.start_next_queued();
        if queue_drained {
            self.run_queue_complete_action().await;
        }
        let jid = job_id.to_string();
        let qm = Arc::clone(self);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(8)).await;
            qm.jobs.lock().remove(&jid);
            qm.job_order.lock().retain(|id| id != &jid);
        });
    }

    /// Ids of the servers a fetch could use; they fingerprint ledger records.
    fn enabled_server_ids(&self) -> Vec<String> {
        self.servers
            .lock()
            .iter()
            .filter(|s| s.enabled)
            .map(|s| s.id.clone())
            .collect()
    }

    /// Write a job's buffered article outcomes to the damage ledger.
    fn flush_ledger(&self, job_id: &str, ledger: &mut LedgerRecorder) {
        if ledger.is_empty() {
            return;
        }
        if ledger.needs_file_index()
            && let Some(state) = self.jobs.lock().get(job_id)
        {
            ledger.set_file_order(state.job.files.iter().map(|f| f.id.as_str()));
        }
        let enabled = self.enabled_server_ids();
        ledger.flush(job_id, &self.db.lock(), &enabled);
    }

    /// Called when a job's download phase completes.
    ///
    /// Note: the job's status is already set to `PostProcessing` and
    /// `start_next_queued()` has already been called by `handle_progress`,
    /// so the next download is ramping up concurrently with this work.
    async fn on_job_finished(
        self: &Arc<Self>,
        job_id: &str,
        success: bool,
        articles_failed: usize,
    ) {
        let pipeline_start = Instant::now();
        // A download slot is already free at this point. Bound the independent
        // post-processing job before taking any stage-specific resource.
        let _pipeline_permit = self.postproc_resources.acquire_pipeline().await;

        // Extract info needed for post-processing and take the direct unpacker.
        let (
            work_dir,
            output_dir,
            category,
            pp_level,
            direct_unpacker,
            password,
            content_articles_failed,
            delete_archives,
            slice_map,
            repair_fold,
        ) = {
            let mut jobs = self.jobs.lock();
            let Some(state) = jobs.get_mut(job_id) else {
                return;
            };

            if success {
                info!(job_id = %job_id, "Job moving to post-processing");
            } else {
                info!(
                    job_id = %job_id,
                    articles_failed,
                    "Job moving to post-processing ({articles_failed} article(s) failed, par2 may repair)"
                );
            }

            let cat = state.job.category.clone();
            let pp = self.post_processing_level(&state.job);
            // Post-processing writes into the complete folder from here on.
            Self::claim_output_dir(state);
            let du = state.direct_unpacker.take();
            let pw = state.job.password.clone();
            let content_failed = state
                .hopeless_tracker
                .as_ref()
                .map_or(articles_failed, |tracker| tracker.content_articles_failed);
            (
                state.job.work_dir.clone(),
                state.job.output_dir.clone(),
                cat,
                pp,
                du,
                pw,
                content_failed,
                state.job.delete_archives.unwrap_or(true),
                state.job.slice_map.clone(),
                state.repair_fold.take(),
            )
        };
        // Record the reserved folder so a restart resumes into it.
        self.persist_job_progress_blocking(job_id).await;

        // Repair and extraction can write to both the incomplete and the
        // category output volumes. Apply the same guard to both paths before
        // any post-processing work begins.
        if !disk_space_available(
            self.min_free_space(),
            [work_dir.as_path(), output_dir.as_path()].as_slice(),
        ) {
            let queue_drained = {
                let mut jobs = self.jobs.lock();
                let finished = if let Some(state) = jobs.get_mut(job_id) {
                    let message = POSTPROC_DISK_FULL_MESSAGE.to_string();
                    state.job.status = JobStatus::Failed;
                    state.job.error_message = Some(message.clone());
                    self.move_to_history(
                        state,
                        vec![StageResult {
                            name: "Disk".into(),
                            status: StageStatus::Failed,
                            message: Some(message),
                            duration_secs: 0.0,
                        }],
                    );
                    true
                } else {
                    false
                };
                finished && queue_drained_by(&jobs, job_id)
            };
            self.persist_job_progress_blocking(job_id).await;
            self.start_next_queued();
            if queue_drained {
                self.run_queue_complete_action().await;
            }
            let qm = Arc::clone(self);
            let jid = job_id.to_string();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(8)).await;
                qm.jobs.lock().remove(&jid);
                qm.job_order.lock().retain(|id| id != &jid);
            });
            return;
        }

        // Wait for direct unpack to finish (if active). It may still be
        // extracting the last volume when the download completes.
        let direct_unpack_success = if let Some(du) = direct_unpacker {
            let results = du.finish().await;
            let all_ok = !results.is_empty() && results.iter().all(|r| r.success);
            if all_ok {
                info!(
                    job_id = %job_id,
                    sets = results.len(),
                    "Direct unpack completed successfully — checking output for nested archives"
                );
            } else {
                for r in &results {
                    if !r.success {
                        warn!(
                            job_id = %job_id,
                            set = %r.set_name,
                            error = ?r.error,
                            password_required = r.password_required,
                            "Direct unpack failed for set — falling back to normal extraction"
                        );
                    }
                }
            }
            all_ok
        } else {
            false
        };

        // Run post-processing pipeline (par2 can repair failed articles)
        let stages = if pp_level > 0 {
            info!(
                job_id = %job_id,
                category = %category,
                pp_level,
                "Running post-processing pipeline"
            );

            let (cleanup_patterns, unwanted_extensions) = self
                .categories
                .lock()
                .iter()
                .find(|configured| configured.name == category)
                .map(|configured| {
                    (
                        configured.cleanup_patterns.clone(),
                        configured.unwanted_extensions.clone(),
                    )
                })
                .unwrap_or_default();
            let config = PostProcConfig {
                cleanup_after_extract: true,
                delete_archives,
                output_dir: Some(output_dir.clone()),
                articles_failed,
                content_articles_failed,
                skip_extract: direct_unpack_success,
                password: password.clone(),
                max_nested_archive_depth: self.max_nested_archive_depth,
                slice_map,
                trust_inflight_verify: self.trust_inflight_verify.load(Ordering::SeqCst),
                repair_fold,
            };

            let pp_span = tracing::info_span!(parent: None, "postproc", job_id = %job_id, pp_level);
            let result = if pp_level >= 2 {
                tracing::Instrument::instrument(
                    run_pipeline_with_cleanup(
                        &work_dir,
                        &config,
                        Some(&self.postproc_resources),
                        &cleanup_patterns,
                        &unwanted_extensions,
                    ),
                    pp_span,
                )
                .await
            } else {
                // Repair only (SABnzbd `pp=1`): the archives are the output.
                tracing::Instrument::instrument(
                    run_repair_pipeline(&work_dir, &config, Some(&self.postproc_resources)),
                    pp_span,
                )
                .await
            };

            info!(
                job_id = %job_id,
                success = result.success,
                stages = result.stages.len(),
                elapsed_secs = pipeline_start.elapsed().as_secs_f64(),
                "Post-processing pipeline finished"
            );

            // Update job status based on pipeline result
            {
                let mut jobs = self.jobs.lock();
                if let Some(state) = jobs.get_mut(job_id)
                    && !result.success
                {
                    state.job.status = JobStatus::Failed;
                    state.job.error_message = result.error.clone();
                    state.failure_code =
                        result.failure_code.or(Some(JobFailureCode::DownloadFailed));
                }
            }

            result.stages
        } else {
            info!(job_id = %job_id, pp_level, "Post-processing disabled for job, skipping pipeline");
            // Download only (SABnzbd `pp=0`): nothing is verified or
            // repaired, so the raw files are delivered as downloaded, as
            // SABnzbd does. Hopeless jobs never reach this point.
            if success {
                Vec::new()
            } else {
                vec![StageResult {
                    name: "Verify".to_string(),
                    status: StageStatus::Skipped,
                    message: Some(format!(
                        "Post-processing disabled; {articles_failed} article(s) failed to download"
                    )),
                    duration_secs: 0.0,
                }]
            }
        };

        // Run the configured hook after the final status is known. Its output
        // is bounded and the hook cannot change the job's filesystem roots.
        let script_status = {
            let jobs = self.jobs.lock();
            jobs.get(job_id).map(|state| {
                if state.job.status == JobStatus::Failed {
                    JobStatus::Failed
                } else {
                    JobStatus::Completed
                }
            })
        };
        let script_stage = match script_status {
            Some(status) => self.run_postproc_script(job_id, status, &stages).await,
            None => None,
        };
        let mut stages = stages;
        if let Some(stage) = script_stage {
            if stage.status == StageStatus::Failed {
                let mut jobs = self.jobs.lock();
                if let Some(state) = jobs.get_mut(job_id) {
                    state.job.status = JobStatus::Failed;
                    if state.job.error_message.is_none() {
                        state.job.error_message = stage.message.clone();
                    }
                }
            }
            stages.push(stage);
        }

        // Move to history with real stage results. The drain check runs under
        // the same lock, so only the completion that empties the queue sees it.
        let queue_drained = {
            let mut jobs = self.jobs.lock();
            match jobs.get_mut(job_id) {
                Some(state) => {
                    self.move_to_history(state, stages);
                    queue_drained_by(&jobs, job_id)
                }
                None => false,
            }
        };

        // Persist final state
        self.persist_job_progress_blocking(job_id).await;

        if queue_drained {
            self.run_queue_complete_action().await;
        }

        // Keep the completed/failed job visible in the queue briefly so the
        // UI has a chance to show the transition.  Fast downloads can go from
        // Queued → Downloading → PostProcessing → History in under a second,
        // before the UI's poll interval (1-5s) can observe them.
        let jid = job_id.to_string();
        let qm = Arc::clone(self);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(8)).await;
            qm.jobs.lock().remove(&jid);
            qm.job_order.lock().retain(|id| id != &jid);
        });
    }

    async fn run_postproc_script(
        &self,
        job_id: &str,
        final_status: JobStatus,
        stages: &[StageResult],
    ) -> Option<StageResult> {
        let (job, script_config) = {
            let jobs = self.jobs.lock();
            let state = jobs.get(job_id)?;
            (state.job.clone(), self.postproc_scripts.lock().clone())
        };
        let configured = match final_status {
            JobStatus::Completed => script_config.success.clone(),
            JobStatus::Failed => script_config.failure.clone(),
            _ => None,
        }?;
        let started = Instant::now();
        let script = match resolve_script_path(script_config.scripts_dir.as_deref(), &configured) {
            Ok(path) => path,
            Err(error) => {
                return Some(StageResult {
                    name: "Script".into(),
                    status: StageStatus::Failed,
                    message: Some(format!("Unable to resolve post-processing script: {error}")),
                    duration_secs: started.elapsed().as_secs_f64(),
                });
            }
        };

        let args = postproc_script_args(&job, stages);
        let pp_level = self.post_processing_level(&job);
        let env = postproc_script_env(&job, final_status, stages, &script, pp_level);
        Some(
            run_bounded_script(
                &script,
                &job.output_dir,
                &args,
                env,
                &script_config,
                started,
            )
            .await,
        )
    }

    /// Run the `script_<name>` set by `change_complete_action` once the queue
    /// has drained. The script runs from the complete directory under the
    /// post-processing limits. A failure is logged, so it is also listed as a
    /// warning.
    async fn run_queue_complete_action(&self) {
        let Some(action) = self.queue_complete_action() else {
            return;
        };
        let Some(script) = action.strip_prefix("script_") else {
            return;
        };
        let config = self.postproc_scripts.lock().clone();
        let complete_dir = self.complete_dir.lock().clone();
        let path = match resolve_script_path(
            config.scripts_dir.as_deref(),
            std::path::Path::new(script),
        ) {
            Ok(path) => path,
            Err(error) => {
                warn!(script, %error, "Queue-complete script cannot be resolved");
                self.reset_queue_complete_action();
                return;
            }
        };
        let stage = run_bounded_script(
            &path,
            &complete_dir,
            &[],
            Vec::new(),
            &config,
            Instant::now(),
        )
        .await;
        if stage.status == StageStatus::Failed {
            warn!(
                script,
                message = stage.message.as_deref().unwrap_or_default(),
                "Queue-complete script failed"
            );
        } else {
            info!(script, "Queue-complete script finished");
        }
        self.reset_queue_complete_action();
    }

    /// SABnzbd runs the action once and then resets it, unless the action is
    /// persistent (`queue_complete_pers`).
    fn reset_queue_complete_action(&self) {
        if !self.queue_complete_persist.load(Ordering::SeqCst) {
            self.store_queue_complete_action(None);
        }
    }

    /// Move a job's files to output and insert a history entry.
    fn move_to_history(&self, state: &mut JobState, mut stages: Vec<StageResult>) {
        let move_start = Instant::now();

        let mut final_status = if state.job.status == JobStatus::Failed {
            // Already marked failed (by pipeline or download with pp disabled)
            JobStatus::Failed
        } else {
            // Pipeline ran successfully (or no articles failed) — job is complete.
            // Par2 may have repaired missing articles, so articles_failed > 0 is OK.
            JobStatus::Completed
        };

        // A post-processing job that contains only raw archive/PAR2 artifacts
        // is not a usable completion. Check before moving residual work files
        // so a bad job cannot pollute the completed directory.
        // Only unpacking jobs promise extracted output: download-only (0) and
        // repair-only (1) jobs deliver the archives themselves.
        let pp_level = self.post_processing_level(&state.job);
        if final_status == JobStatus::Completed && pp_level >= 2 && !stages.is_empty() {
            let output_has_payload = has_usable_output(&state.job.output_dir).unwrap_or(false);
            let work_has_payload = has_usable_output(&state.job.work_dir).unwrap_or(false);
            if !output_has_payload && !work_has_payload {
                let message = "No usable output produced; only archive or PAR2 artifacts remain";
                warn!(job_id = %state.job.id, output_dir = %state.job.output_dir.display(), "{message}");
                final_status = JobStatus::Failed;
                state.job.error_message = Some(message.to_string());
                state.failure_code = Some(JobFailureCode::ArchiveInvalid);
                stages.push(StageResult {
                    name: "Output".to_string(),
                    status: StageStatus::Failed,
                    message: Some(message.to_string()),
                    duration_secs: 0.0,
                });
            }
        }

        // Move files from work_dir to output_dir (if not already done by pipeline extract).
        if final_status == JobStatus::Completed {
            Self::claim_output_dir(state);
            if let Err(e) = std::fs::create_dir_all(&state.job.output_dir) {
                warn!(job_id = %state.job.id, "Failed to create output dir: {e}");
            }
            if let Ok(entries) = std::fs::read_dir(&state.job.work_dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    let is_regular = std::fs::symlink_metadata(&path)
                        .map(|metadata| metadata.file_type().is_file())
                        .unwrap_or(false);
                    let Some(dest) = crate::nzb_core::path::safe_join(
                        &state.job.output_dir,
                        &entry.file_name().to_string_lossy(),
                    ) else {
                        warn!(
                            job_id = %state.job.id,
                            file = %path.display(),
                            "Refusing to move file with an unsafe output name"
                        );
                        continue;
                    };
                    if is_regular {
                        if std::fs::symlink_metadata(&dest)
                            .map(|metadata| metadata.file_type().is_symlink())
                            .unwrap_or(false)
                        {
                            warn!(
                                job_id = %state.job.id,
                                file = %dest.display(),
                                "Refusing to replace symlink in output directory"
                            );
                            continue;
                        }
                        if let Err(e) = std::fs::rename(&path, &dest) {
                            if let Err(e2) = std::fs::copy(&path, &dest) {
                                warn!(
                                    job_id = %state.job.id,
                                    file = %path.display(),
                                    "Failed to move file: rename={e}, copy={e2}"
                                );
                            } else {
                                let _ = std::fs::remove_file(&path);
                            }
                        }
                    }
                }
            }
        }

        let file_move_secs = move_start.elapsed().as_secs_f64();
        info!(
            job_id = %state.job.id,
            final_status = %final_status,
            file_move_secs = format!("{file_move_secs:.3}"),
            stage_count = stages.len(),
            "Moving job to history"
        );

        state.job.status = final_status;

        // Insert into history with real stage results
        if final_status == JobStatus::Failed && state.output_dir_claimed {
            // A failed job keeps nothing in its reserved folder unless
            // extraction already wrote there; release an empty reservation so
            // a retry can claim the plain name again.
            if std::fs::remove_dir(&state.job.output_dir).is_ok() {
                state.output_dir_claimed = false;
            }
        }

        let retry_data =
            serde_json::to_vec(&checkpoint_for_job(&state.job, state.output_dir_claimed)).ok();
        let history_entry = HistoryEntry {
            id: state.job.id.clone(),
            name: state.job.name.clone(),
            category: state.job.category.clone(),
            status: final_status,
            total_bytes: state.job.total_bytes,
            downloaded_bytes: state.job.downloaded_bytes,
            added_at: state.job.added_at,
            completed_at: state.job.completed_at.unwrap_or_else(chrono::Utc::now),
            download_time_secs: state.download_time_secs,
            output_dir: state.job.output_dir.clone(),
            stages,
            error_message: state.job.error_message.clone(),
            failure_code: (final_status == JobStatus::Failed)
                .then(|| state.failure_code.unwrap_or(JobFailureCode::DownloadFailed)),
            post_processing: Some(pp_level),
            delete_archives: state.job.delete_archives,
            server_stats: state.job.server_stats.clone(),
            nzb_data: state.nzb_data.clone(),
            retry_data,
            par2_volumes_fetched: state.par2_volumes.map(|(fetched, _)| fetched),
            par2_volumes_available: state.par2_volumes.map(|(_, available)| available),
        };

        let db = self.db.lock();
        let history_persisted = match db.history_get(&state.job.id) {
            Ok(Some(existing)) => {
                warn!(
                    job_id = %state.job.id,
                    existing_status = %existing.status,
                    attempted_status = %final_status,
                    "History row already exists; terminal persistence is idempotent"
                );
                true
            }
            Ok(None) => {
                if let Err(e) =
                    db.history_insert_with_carried_bytes(&history_entry, state.carried_bytes)
                {
                    error!(job_id = %state.job.id, "Failed to insert history: {e}");
                    false
                } else {
                    self.history_changed();
                    true
                }
            }
            Err(e) => {
                error!(job_id = %state.job.id, "Failed to check existing history row: {e}");
                false
            }
        };

        // Capture and persist per-job logs from the ring buffer
        if let Some(ref log_buffer) = self.log_buffer {
            let logs = log_buffer.get_entries(Some(&state.job.id), None, None, 5000);
            if !logs.is_empty() {
                let logs_json = serde_json::to_string(&logs).unwrap_or_default();
                if let Err(e) = db.history_store_logs(&state.job.id, &logs_json) {
                    warn!(job_id = %state.job.id, "Failed to store logs in history: {e}");
                }
            }
        }

        if let Err(e) = db.queue_remove(&state.job.id) {
            error!(job_id = %state.job.id, "Failed to remove from queue: {e}");
        }
        prune_damage_ledger(&db);

        // Enforce retention
        if let Some(max) = *self.history_retention.lock()
            && let Err(e) = db.history_enforce_retention(max)
        {
            warn!("Failed to enforce history retention: {e}");
        }
        drop(db);

        if history_persisted {
            self.notify_terminal(state, final_status);
            let retain_for_retry = final_status == JobStatus::Failed
                && state.nzb_data.is_some()
                && state
                    .job
                    .files
                    .iter()
                    .any(|file| file.articles.iter().any(|article| article.downloaded));
            cleanup_terminal_work_dir(
                &state.job.id,
                &state.job.work_dir,
                final_status,
                retain_for_retry,
            );
        } else {
            warn!(
                job_id = %state.job.id,
                work_dir = %state.job.work_dir.display(),
                "Retaining terminal work directory because history persistence failed"
            );
        }
    }

    /// Notify about a job that has just reached history.
    fn notify_terminal(&self, state: &JobState, final_status: JobStatus) {
        if let Some(event) = terminal_notification(&state.job, final_status) {
            self.notify(event);
        }
    }

    /// Reserve a unique complete-directory folder for this job if it has not
    /// reserved one yet, and record the final path on the job.
    fn claim_output_dir(state: &mut JobState) {
        claim_output_dir_fields(
            &state.job.id,
            &mut state.job.output_dir,
            &mut state.output_dir_claimed,
        );
    }

    /// Drop a reservation before the job's target folder changes, removing
    /// the reserved folder when nothing was written into it yet.
    fn release_output_dir_claim(state: &mut JobState) {
        if state.output_dir_claimed {
            let _ = std::fs::remove_dir(&state.job.output_dir);
            state.output_dir_claimed = false;
        }
    }

    /// Persist current job progress to the database, including article-level
    /// checkpoint data for resume support.
    /// Persist a job's progress counters and resume checkpoint.
    ///
    /// The snapshot is taken under the job-list lock, which is released
    /// before the checkpoint is serialised and before the database lock is
    /// taken: the two locks are never held together, so `get_jobs()` (queue
    /// and status polls) never waits behind a database write or a slow query.
    fn persist_job_progress(&self, job_id: &str) {
        let snapshot = {
            let jobs = self.jobs.lock();
            let Some(state) = jobs.get(job_id) else {
                return;
            };
            let mut checkpoint = checkpoint_for_job(&state.job, state.output_dir_claimed);
            checkpoint.carried_bytes = state.carried_bytes;
            (
                state.persisted_status != Some(state.job.status),
                state.job.downloaded_bytes,
                state.job.articles_downloaded,
                state.job.articles_failed,
                state.job.files_completed,
                state.download_time_secs,
                state.prior_download_secs,
                checkpoint,
            )
        };
        let (
            status_changed,
            downloaded_bytes,
            articles_downloaded,
            articles_failed,
            files_completed,
            final_download_secs,
            prior_download_secs,
            mut checkpoint,
        ) = snapshot;
        // Accumulated active time: the final total once the download
        // resolved, otherwise earlier runs plus the live engine run.
        checkpoint.download_time_secs = final_download_secs.unwrap_or_else(|| {
            prior_download_secs + self.dispatch.job_active_secs(job_id).unwrap_or(0.0)
        });
        let data = serde_json::to_vec(&checkpoint);

        // Counters and checkpoint only: the snapshot's status may already be
        // stale (a pause or resume can land before the database lock is
        // free), so it is never written from here.
        {
            let db = self.db.lock();
            if let Err(e) = db.queue_update_counters(
                job_id,
                downloaded_bytes,
                articles_downloaded,
                articles_failed,
                files_completed,
            ) {
                warn!(job_id = %job_id, "Failed to persist progress: {e}");
            }
            if let Ok(data) = data
                && let Err(e) = db.queue_store_job_data(job_id, &data)
            {
                warn!(job_id = %job_id, "Failed to persist checkpoint: {e}");
            }
        }

        // On a status transition, write the job list's current status under
        // jobs -> db, the same order the pause and resume paths use.
        if status_changed {
            let mut jobs = self.jobs.lock();
            let Some(state) = jobs.get_mut(job_id) else {
                return;
            };
            let status = state.job.status;
            if state.persisted_status != Some(status) {
                let db = self.db.lock();
                match db.queue_update_status(job_id, status) {
                    Ok(()) => state.persisted_status = Some(status),
                    Err(e) => warn!(job_id = %job_id, "Failed to persist status: {e}"),
                }
            }
        }
    }

    /// [`Self::persist_job_progress`] on a blocking thread, for async callers:
    /// the database write never runs on an async worker.
    async fn persist_job_progress_blocking(self: &Arc<Self>, job_id: &str) {
        let qm = Arc::clone(self);
        let job_id = job_id.to_string();
        if let Err(e) = tokio::task::spawn_blocking(move || qm.persist_job_progress(&job_id)).await
        {
            warn!("Progress persist task failed: {e}");
        }
    }

    // -----------------------------------------------------------------------
    // Job control
    // -----------------------------------------------------------------------

    /// Change the priority of a specific job and reorder the queue.
    ///
    /// A priority change only reorders the queue; it never pauses an
    /// actively-downloading job (GH #124). The new order takes effect the
    /// next time a download slot frees up. If a slot is already free, any
    /// queued job is started in the new order, but no running download is
    /// preempted.
    pub fn set_job_priority(
        self: &Arc<Self>,
        id: &str,
        priority: Priority,
    ) -> crate::nzb_core::Result<()> {
        let max = self.max_active_downloads.load(Ordering::Relaxed);

        // 1. Update priority
        {
            let mut jobs = self.jobs.lock();
            let job_state = jobs
                .get_mut(id)
                .ok_or_else(|| crate::nzb_core::NzbError::JobNotFound(id.to_string()))?;
            job_state.job.priority = priority;
            let db = self.db.lock();
            db.queue_update_priority(id, priority as i32)?;
            info!(
                job_id = %id,
                ?priority,
                priority_val = priority as u8,
                max_active = max,
                "Job priority changed"
            );
        }

        // 2. Reorder job_order by priority (stable: preserves order within same priority)
        {
            let jobs = self.jobs.lock();
            let mut order = self.job_order.lock();
            let before: Vec<String> = order.clone();
            order.sort_by(|a, b| {
                let pa = jobs.get(a).map(|s| s.job.priority as u8).unwrap_or(0);
                let pb = jobs.get(b).map(|s| s.job.priority as u8).unwrap_or(0);
                pb.cmp(&pa) // descending: highest priority first
            });
            if *order != before {
                info!(
                    before = ?before.iter().take(6).collect::<Vec<_>>(),
                    after = ?order.iter().take(6).collect::<Vec<_>>(),
                    "Queue reordered by priority"
                );
            }
        }

        // 3. Fill any free download slot in the new order. A priority change
        // must not pause a running download (GH #124), so start queued jobs
        // only — never preempt an active one.
        self.start_next_queued();

        Ok(())
    }

    fn queued_job_outranks_active(
        queued_priority: u8,
        queued_index: usize,
        active_priority: u8,
        active_index: usize,
    ) -> bool {
        queued_priority > active_priority
            || (queued_priority == active_priority && queued_index < active_index)
    }

    /// Check whether a queued job has higher priority than a running download,
    /// and if so, pause the lower-priority download to make room.
    fn preempt_if_needed(self: &Arc<Self>) {
        let max = self.max_active_downloads.load(Ordering::Relaxed);

        loop {
            // Snapshot current state
            let (active, best_queued, worst_downloading) = {
                let jobs = self.jobs.lock();
                let order = self.job_order.lock();

                let active = jobs
                    .values()
                    .filter(|s| s.job.status == JobStatus::Downloading)
                    .count();

                let mut best_q: Option<(String, u8, usize, String)> = None;
                let mut worst_d: Option<(String, u8, usize, String)> = None;

                for (idx, id) in order.iter().enumerate() {
                    if let Some(s) = jobs.get(id) {
                        let p = s.job.priority as u8;
                        let name = s.job.name.clone();
                        if s.job.status == JobStatus::Queued {
                            if best_q
                                .as_ref()
                                .is_none_or(|(_, bp, bidx, _)| p > *bp || (p == *bp && idx < *bidx))
                            {
                                best_q = Some((id.clone(), p, idx, name));
                            }
                        } else if s.job.status == JobStatus::Downloading
                            && worst_d
                                .as_ref()
                                .is_none_or(|(_, wp, widx, _)| p < *wp || (p == *wp && idx > *widx))
                        {
                            worst_d = Some((id.clone(), p, idx, name));
                        }
                    }
                }
                (active, best_q, worst_d)
            };

            // If unlimited slots or free slots available, just start queued jobs
            if max == 0 || active < max {
                self.start_next_queued();
                return;
            }

            // All slots full — check if preemption is warranted
            match (&best_queued, &worst_downloading) {
                (Some((q_id, q_pri, q_idx, q_name)), Some((d_id, d_pri, d_idx, d_name)))
                    if Self::queued_job_outranks_active(*q_pri, *q_idx, *d_pri, *d_idx) =>
                {
                    info!(
                        preempted_id = %d_id,
                        preempted_name = %d_name,
                        preempted_priority = d_pri,
                        preempted_order = d_idx,
                        starting_id = %q_id,
                        starting_name = %q_name,
                        starting_priority = q_pri,
                        starting_order = q_idx,
                        active_downloads = active,
                        max_downloads = max,
                        "Preempting active download for queued job with higher effective priority"
                    );

                    // Pause the lower-priority download via the worker pool.
                    self.dispatch.pause_job(d_id);
                    {
                        let mut jobs = self.jobs.lock();
                        if let Some(state) = jobs.get_mut(d_id.as_str()) {
                            state.job.status = JobStatus::Paused;
                            // Persist to DB
                            state.persisted_status = Some(JobStatus::Paused);
                            let db = self.db.lock();
                            let _ = db.queue_update_progress(
                                d_id,
                                JobStatus::Paused,
                                state.job.downloaded_bytes,
                                state.job.articles_downloaded,
                                state.job.articles_failed,
                                state.job.files_completed,
                            );
                        }
                    }
                    // Loop back — active count decreased, start_next_queued will run
                }
                _ => {
                    info!(
                        active_downloads = active,
                        max_downloads = max,
                        best_queued_pri = best_queued.as_ref().map(|q| q.1),
                        best_queued_order = best_queued.as_ref().map(|q| q.2),
                        worst_dl_pri = worst_downloading.as_ref().map(|d| d.1),
                        worst_dl_order = worst_downloading.as_ref().map(|d| d.2),
                        "No preemption needed"
                    );
                    return;
                }
            }
        }
    }

    /// Pause a specific job.
    pub fn pause_job(self: &Arc<Self>, id: &str) -> crate::nzb_core::Result<()> {
        // Tell the pool first — workers stop pulling this job's items.
        self.dispatch.pause_job(id);
        {
            let mut jobs = self.jobs.lock();
            let state = jobs
                .get_mut(id)
                .ok_or_else(|| crate::nzb_core::NzbError::JobNotFound(id.to_string()))?;

            state.job.status = JobStatus::Paused;
            // An explicit pause supersedes a disk-space hold, so the disk
            // guard must not resume this job when space frees up.
            if state.failure_code == Some(JobFailureCode::StorageUnavailable) {
                state.failure_code = None;
                state.job.error_message = None;
            }

            state.persisted_status = Some(JobStatus::Paused);
            let db = self.db.lock();
            db.queue_update_progress(
                id,
                JobStatus::Paused,
                state.job.downloaded_bytes,
                state.job.articles_downloaded,
                state.job.articles_failed,
                state.job.files_completed,
            )?;

            info!(job_id = %id, "Job paused");
        }

        // If the job was paused by the global control, this explicit action
        // changes it into an individual pause that must survive Resume All.
        self.globally_paused_jobs.lock().remove(id);
        self.persist_globally_paused_jobs();

        // Release the download slot so queued jobs can start
        self.start_next_queued();
        Ok(())
    }

    /// Resume a specific job.
    pub fn resume_job(self: &Arc<Self>, id: &str) -> crate::nzb_core::Result<()> {
        let _transition = self.pause_transition.lock();
        if self.globally_paused.load(Ordering::SeqCst) {
            return Err(crate::nzb_core::NzbError::Other(
                "Cannot resume an individual job while downloads are globally paused".to_string(),
            ));
        }

        let ctx_alive = self.dispatch.has_job(id);

        let needs_launch = {
            let mut jobs = self.jobs.lock();
            if !jobs.contains_key(id) {
                return Err(crate::nzb_core::NzbError::JobNotFound(id.to_string()));
            }

            let active = jobs
                .values()
                .filter(|s| s.job.status == JobStatus::Downloading)
                .count();

            let max = self.max_active_downloads.load(Ordering::Relaxed);
            let at_limit = max > 0 && active >= max;
            let state = jobs.get_mut(id).unwrap();

            if at_limit {
                // Respect the active download limit whether or not the pool
                // still holds this job's context. A paused context stays
                // paused in the pool; `start_next_queued` resumes it once a
                // slot frees up.
                state.job.status = JobStatus::Queued;
                state.persisted_status = Some(JobStatus::Queued);
                let db = self.db.lock();
                let _ = db.queue_update_progress(
                    id,
                    JobStatus::Queued,
                    state.job.downloaded_bytes,
                    state.job.articles_downloaded,
                    state.job.articles_failed,
                    state.job.files_completed,
                );
                info!(job_id = %id, "Job queued (active download limit reached)");
                return Ok(());
            }

            if ctx_alive {
                // Job context still lives in the pool — just unpause it.
                state.job.status = JobStatus::Downloading;
                state.job.error_message = None;
                state.failure_code = None;
                // The no-progress watchdog measures wall-clock idle time and
                // does not stop while paused, so restart its clock here or the
                // paused interval counts toward the article timeout and aborts
                // the job on the next scan (GH #123).
                if let Some(tracker) = state.hopeless_tracker.as_mut() {
                    tracker.reset_progress_clock();
                }
                state.persisted_status = Some(JobStatus::Downloading);
                let db = self.db.lock();
                let _ = db.queue_update_progress(
                    id,
                    JobStatus::Downloading,
                    state.job.downloaded_bytes,
                    state.job.articles_downloaded,
                    state.job.articles_failed,
                    state.job.files_completed,
                );
                false
            } else {
                // Pool has no context — we need to rebuild work items and submit.
                state.job.status = JobStatus::Downloading;
                state.job.error_message = None;
                state.failure_code = None;
                true
            }
        };

        if ctx_alive {
            self.dispatch.resume_job(id);
        } else if needs_launch {
            self.launch_download(id);
        }

        info!(job_id = %id, "Job resumed");
        Ok(())
    }

    /// Remove a specific job from the queue.
    ///
    /// If the job was sitting in an error state (e.g. paused because no
    /// server was reachable, or stalled) when removed, it's preserved as a
    /// `Failed` history entry first — otherwise the only record of the
    /// failure (the error message) is lost the moment the user clears it.
    /// A job removed with no error (the user simply doesn't want it) is
    /// just deleted, matching prior behavior.
    pub fn remove_job(self: &Arc<Self>, id: &str) -> crate::nzb_core::Result<()> {
        // Post-processing owns the work directory and JobState until its
        // terminal history transaction completes. Treat an external queue
        // cleanup request during this window as deferred view cleanup; do
        // not cancel the task or delete files beneath PAR2/extraction.
        if self.jobs.lock().get(id).is_some_and(|state| {
            matches!(
                state.job.status,
                JobStatus::PostProcessing
                    | JobStatus::Verifying
                    | JobStatus::Repairing
                    | JobStatus::Extracting
            )
        }) {
            info!(job_id = %id, "Queue removal deferred while post-processing is active");
            return Ok(());
        }

        // Silently cancel in the pool — drains queued items and unregisters.
        self.dispatch.cancel_job(id);
        let removed = self.jobs.lock().remove(id);
        if let Some(state) = removed {
            // Removing the last active job drains the queue. Checked under the
            // same lock as the removal, so only the removal that empties it
            // sees the drain.
            let was_active = !matches!(state.job.status, JobStatus::Completed | JobStatus::Failed);
            let drained = was_active
                && self.jobs.lock().values().all(|other| {
                    matches!(other.job.status, JobStatus::Completed | JobStatus::Failed)
                });
            if drained {
                self.spawn_queue_complete_action();
            }
            self.globally_paused_jobs.lock().remove(id);
            self.persist_globally_paused_jobs();
            if let Some(handle) = state.progress_handle {
                handle.abort();
            }

            let db = self.db.lock();
            let history_already_persisted = db.history_get(id)?.is_some();

            if state.job.error_message.is_some() && !history_already_persisted {
                let history_entry = HistoryEntry {
                    id: state.job.id.clone(),
                    name: state.job.name.clone(),
                    category: state.job.category.clone(),
                    status: JobStatus::Failed,
                    total_bytes: state.job.total_bytes,
                    downloaded_bytes: state.job.downloaded_bytes,
                    added_at: state.job.added_at,
                    completed_at: state.job.completed_at.unwrap_or_else(chrono::Utc::now),
                    download_time_secs: state.download_time_secs,
                    output_dir: state.job.output_dir.clone(),
                    stages: Vec::new(),
                    error_message: state.job.error_message.clone(),
                    failure_code: Some(
                        state.failure_code.unwrap_or(JobFailureCode::DownloadFailed),
                    ),
                    post_processing: Some(self.post_processing_level(&state.job)),
                    delete_archives: state.job.delete_archives,
                    server_stats: state.job.server_stats.clone(),
                    nzb_data: state.nzb_data.clone(),
                    retry_data: None,
                    par2_volumes_fetched: None,
                    par2_volumes_available: None,
                };
                if let Err(e) =
                    db.history_insert_with_carried_bytes(&history_entry, state.carried_bytes)
                {
                    error!(job_id = %id, "Failed to insert history for removed failed job: {e}");
                } else {
                    self.history_changed();
                    if let Some(max) = *self.history_retention.lock()
                        && let Err(e) = db.history_enforce_retention(max)
                    {
                        warn!("Failed to enforce history retention: {e}");
                    }
                }
            } else if history_already_persisted {
                debug!(job_id = %id, "Removing terminal queue view; history already persisted");
            }

            // Remove from DB
            let _ = db.queue_remove(id);
            prune_damage_ledger(&db);
            drop(db);

            // Remove from order
            self.job_order.lock().retain(|jid| jid != id);

            // Try to clean up work directory. A terminal job whose history
            // row already exists may have retained its partial download for
            // retry; deleting that history entry removes it instead.
            if !history_already_persisted && state.job.work_dir.exists() {
                let _ = std::fs::remove_dir_all(&state.job.work_dir);
            }

            info!(job_id = %id, "Job removed");
        }
        Ok(())
    }

    /// Rename a job in the queue.
    ///
    /// The new name is sanitized the same way as names taken from uploaded
    /// NZB filenames (see [`crate::nzb_core::path::sanitize_job_name`]), so
    /// `Show: Part 1` is stored as `Show - Part 1` rather than rejected.
    pub fn rename_job(&self, id: &str, new_name: &str) -> crate::nzb_core::Result<()> {
        let new_name = crate::nzb_core::path::sanitize_job_name(new_name);
        let mut jobs = self.jobs.lock();
        let state = jobs.iter_mut().find(|(_, s)| job_id_matches(&s.job.id, id));
        match state {
            Some((_, s)) => {
                let output_dir = self.output_dir_for(&s.job.category, &new_name)?;
                s.job.name = new_name.clone();
                Self::release_output_dir_claim(s);
                s.job.output_dir = output_dir;
                info!(job_id = %id, new_name = %new_name, "Job renamed");
                Ok(())
            }
            None => Err(crate::nzb_core::NzbError::JobNotFound(id.to_string())),
        }
    }

    /// Change a job's category in the queue.
    pub fn change_job_category(&self, id: &str, category: &str) -> crate::nzb_core::Result<()> {
        if crate::nzb_core::path::safe_component(category).is_none() {
            return Err(crate::nzb_core::NzbError::Other(
                "category must be a single safe path component".to_string(),
            ));
        }
        let job_name = self
            .jobs
            .lock()
            .iter()
            .find(|(_, state)| job_id_matches(&state.job.id, id))
            .map(|(_, state)| state.job.name.clone())
            .ok_or_else(|| crate::nzb_core::NzbError::JobNotFound(id.to_string()))?;
        let output_dir = self.output_dir_for(category, &job_name)?;
        let mut jobs = self.jobs.lock();
        let state = jobs.iter_mut().find(|(_, s)| job_id_matches(&s.job.id, id));
        match state {
            Some((_, s)) => {
                s.job.category = category.to_string();
                // Update the output directory to match the new category
                Self::release_output_dir_claim(s);
                s.job.output_dir = output_dir;
                info!(job_id = %id, category = %category, "Job category changed");
                Ok(())
            }
            None => Err(crate::nzb_core::NzbError::JobNotFound(id.to_string())),
        }
    }

    /// Move a job to a new position in the queue order.
    pub fn move_job(self: &Arc<Self>, id: &str, position: usize) -> crate::nzb_core::Result<()> {
        {
            let mut order = self.job_order.lock();
            let current_pos = order
                .iter()
                .position(|x| x == id)
                .ok_or_else(|| crate::nzb_core::NzbError::JobNotFound(id.to_string()))?;
            let id_str = order.remove(current_pos);
            let new_pos = position.min(order.len());
            order.insert(new_pos, id_str);
        }
        self.preempt_if_needed();
        Ok(())
    }

    /// Pause all downloads globally.
    pub fn pause_all(self: &Arc<Self>) {
        self.pause_for_hold(|holds| holds.user = true);
    }

    /// Pause all downloads on behalf of the disk-space guard, which lifts the
    /// hold itself once space frees up.
    fn pause_all_with_hold(self: &Arc<Self>, disk_space_hold: Option<DateTime<Utc>>) {
        self.pause_for_hold(|holds| {
            if let Some(since) = disk_space_hold {
                holds.disk = Some(since);
            }
        });
    }

    /// Add a hold and pause the queue if it was not already paused.
    ///
    /// Holds are additive and each holder lifts only its own, so a pause that
    /// is already in effect keeps everything else that caused it. `apply`
    /// mutates the holds while the transition lock is held.
    fn pause_for_hold(self: &Arc<Self>, apply: impl FnOnce(&mut PauseHolds)) {
        let _transition = self.pause_transition.lock();
        // Every pause is indefinite unless `pause_for` sets a deadline after
        // this returns, so drop any pending timed resume whether or not the
        // queue was already held: a second pause must not inherit the first
        // one's clock.
        *self.pause_until.lock() = None;
        let was_paused = {
            let mut holds = self.pause_holds.lock();
            apply(&mut holds);
            if holds.user {
                // The user's pause now covers every open window: suspend their
                // hold, so a manually lifted disk hold does not resume a window
                // the user is paused through. The exemption lasts only as long
                // as the user's pause, so a window opening later keeps its own.
                holds.user_paused_over = holds.open_windows.clone();
                holds.schedule = false;
            }
            let paused = self.globally_paused.load(Ordering::SeqCst);
            self.publish_holds(&holds);
            paused
        };
        if !was_paused {
            self.pause_jobs_for_global_hold();
        }
    }

    /// Apply an evaluated window set: pause on a window that newly opened,
    /// and resume when the last window the schedule was holding closes.
    ///
    /// `windows` names every open occurrence (`rule@date`). A window pauses
    /// when its occurrence is first seen; one closing out of an overlap only
    /// removes itself, so the windows still open keep the queue paused and do
    /// not pause it again. A user resume lasts for the rest of that window; the
    /// next occurrence pauses again.
    ///
    /// Applying the same decision again is not a new window: occurrences
    /// already seen stay seen, so a settings save or the next monitor tick
    /// cannot undo a manual resume.
    fn apply_schedule_hold(self: &Arc<Self>, windows: &[String]) {
        let _transition = self.pause_transition.lock();
        let (pause_requested, resume_requested) = {
            let mut holds = self.pause_holds.lock();
            let before = holds.clone();
            let was_held = holds.schedule;
            let newly_opened = holds.newly_opened(windows);
            // Every window that newly opens takes a hold, even while a user
            // pause covers it: the user's pause outranks the hold for as long
            // as it runs, and the hold is what keeps the queue paused
            // afterwards.
            holds.triggered_windows.extend(newly_opened);
            holds.triggered_windows.sort();
            holds.triggered_windows.dedup();
            holds.retain_open(windows);
            // A user pause covers every window it is started in, so the
            // schedule takes no hold while it is in force.
            holds.schedule = !holds.held_occurrences(windows).is_empty() && !holds.user;
            let paused = self.globally_paused.load(Ordering::SeqCst);
            let pause_requested = holds.schedule && !was_held && !paused;
            let resume_requested = !holds.schedule && was_held && !holds.any();
            // A tick that changes nothing writes nothing: the startup tick
            // with an idle schedule must not materialise pause settings on a
            // database that never chose one.
            if *holds != before {
                self.publish_holds(&holds);
            }
            (pause_requested, resume_requested)
        };
        if pause_requested {
            self.pause_jobs_for_global_hold();
        }
        if resume_requested {
            self.resume_jobs_after_holds();
        }
    }

    /// Lift the user's hold, leaving every automatic hold in place.
    ///
    /// This is a timed pause running out, which is the user's own pause
    /// ending rather than a manual override: a window that opened while the
    /// pause covered it takes effect now, instead of the queue resuming
    /// straight into the window.
    fn expire_user_hold(self: &Arc<Self>) {
        let _transition = self.pause_transition.lock();
        let (pause_requested, resume_requested) = {
            let mut holds = self.pause_holds.lock();
            let was_held = holds.schedule;
            holds.user = false;
            // Hand the suspended windows their hold back, as if the user
            // pause had never covered them.
            holds.user_paused_over.clear();
            holds.schedule = !holds.held_occurrences(&holds.open_windows).is_empty() && !holds.user;
            // The user's pause was holding the queue, so a window opening
            // under it only changes which hold explains the pause. Pausing
            // jobs again would be a no-op, but keeping the guard makes this
            // path behave exactly like `apply_schedule_hold`.
            let paused = self.globally_paused.load(Ordering::SeqCst);
            let pause_requested = holds.schedule && !was_held && !paused;
            let resume_requested = !holds.any();
            self.publish_holds(&holds);
            (pause_requested, resume_requested)
        };
        if pause_requested {
            self.pause_jobs_for_global_hold();
        } else if resume_requested {
            self.resume_jobs_after_holds();
        }
    }

    /// Publish the global gate and persist the current holds.
    ///
    /// The gate is set while the holds still say paused, never cleared here:
    /// only [`Self::resume_hold`] may lift the last hold and resume.
    fn publish_holds(&self, holds: &PauseHolds) {
        self.store_holds(holds);
        self.persist_holds(holds);
    }

    /// Publish the global gate for `holds` without touching the database.
    ///
    /// Startup uses this: loading state must not write it back, or a database
    /// that never chose a pause would gain the default rows on first boot and
    /// look like it had.
    fn store_holds(&self, holds: &PauseHolds) {
        self.globally_paused.store(holds.any(), Ordering::SeqCst);
    }

    /// Write `holds` as the settings a restart reads back.
    fn persist_holds(&self, holds: &PauseHolds) {
        let db = self.db.lock();
        db.set_setting(
            "globally_paused",
            if holds.any() { "true" } else { "false" },
        );
        // Each hold is persisted as its own flag. A single ranked "owner"
        // string cannot describe two holds at once: a disk hold overlapping a
        // window would restore only one of them, and the restart would drop
        // the other.
        db.set_setting(
            Self::USER_HOLD_SETTING,
            if holds.user { "true" } else { "false" },
        );
        db.set_setting(
            Self::SCHEDULE_HOLD_SETTING,
            if holds.schedule { "true" } else { "false" },
        );
        let value = holds.disk.map(|at| at.to_rfc3339()).unwrap_or_default();
        db.set_setting(Self::DISK_SPACE_HOLD_SETTING, &value);
        // Kept for readers of the old key, and for records written before each
        // hold had its own flag.
        db.set_setting(
            Self::PAUSE_OWNER_SETTING,
            if holds.user {
                Self::PAUSE_OWNER_USER
            } else if holds.schedule {
                Self::PAUSE_OWNER_SCHEDULE
            } else if holds.disk.is_some() {
                Self::PAUSE_OWNER_DISK
            } else {
                ""
            },
        );
        for (key, windows) in [
            (Self::SCHEDULE_WINDOWS_SETTING, &holds.open_windows),
            (Self::SCHEDULE_TRIGGERED_SETTING, &holds.triggered_windows),
            (Self::SCHEDULE_RESUMED_SETTING, &holds.resumed_windows),
            (Self::SCHEDULE_USER_PAUSED_SETTING, &holds.user_paused_over),
        ] {
            let value = serde_json::to_string(windows).unwrap_or_else(|_| "[]".into());
            db.set_setting(key, &value);
        }
    }

    /// Pause every active job as part of a global pause.
    ///
    /// The caller holds [`Self::pause_transition`] and has already published
    /// the gate, so no resume path can start work while the contexts are
    /// being paused.
    fn pause_jobs_for_global_hold(&self) {
        // Collect ids to pause in the pool, to avoid holding the jobs lock
        // while calling into the worker pool.
        let to_pause: Vec<String> = {
            let mut jobs = self.jobs.lock();
            let mut ids = Vec::new();
            for (id, state) in jobs.iter_mut() {
                match state.job.status {
                    JobStatus::Downloading => {
                        ids.push(id.clone());
                        state.job.status = JobStatus::Paused;
                    }
                    JobStatus::Queued => {
                        ids.push(id.clone());
                        state.job.status = JobStatus::Paused;
                    }
                    _ => {}
                }
            }
            ids
        };
        self.globally_paused_jobs
            .lock()
            .extend(to_pause.iter().cloned());
        self.persist_globally_paused_jobs();
        for id in to_pause {
            self.dispatch.pause_job(&id);
        }
        info!("All downloads paused");
    }

    /// Lift one holder's pause, resuming only when no hold is left.
    ///
    /// `apply` mutates the holds while the transition lock is held; if the
    /// queue is still held afterwards the gate stays put and the reason moves
    /// to the holder that remains.
    fn resume_hold(self: &Arc<Self>, apply: impl FnOnce(&mut PauseHolds)) {
        let _transition = self.pause_transition.lock();
        let resume = {
            let mut holds = self.pause_holds.lock();
            apply(&mut holds);
            self.publish_holds(&holds);
            !holds.any()
        };
        if resume {
            self.resume_jobs_after_holds();
        }
    }

    /// Mark every globally paused job Queued and start work, once the last
    /// hold is gone. The caller holds [`Self::pause_transition`].
    fn resume_jobs_after_holds(self: &Arc<Self>) {
        *self.pause_until.lock() = None;

        let paused_by_global = std::mem::take(&mut *self.globally_paused_jobs.lock());
        self.persist_globally_paused_jobs();

        // Mark every globally paused job Queued; start_next_queued then
        // unpauses a live pool context or re-submits the job, up to the
        // active download limit.
        {
            let mut jobs = self.jobs.lock();
            for id in paused_by_global {
                let Some(state) = jobs.get_mut(&id) else {
                    continue;
                };
                if state.job.status == JobStatus::Paused {
                    state.job.error_message = None;
                    state.failure_code = None;
                    state.job.status = JobStatus::Queued;
                }
            }
        }

        // Start queued jobs up to the concurrency limit
        self.start_next_queued();

        info!("All downloads resumed");
    }

    /// Pause all downloads for a specified duration.
    ///
    /// Durations are clamped to a year. `chrono::Duration::seconds` panics on
    /// values past its range, and a wrapped negative duration would resume
    /// immediately after `pause_all` had already run.
    pub fn pause_for(self: &Arc<Self>, duration_secs: u64) {
        const MAX_PAUSE_SECS: u64 = 365 * 24 * 60 * 60;
        let duration_secs = duration_secs.min(MAX_PAUSE_SECS);
        self.pause_all();
        let until_value = Utc::now()
            + chrono::Duration::try_seconds(i64::try_from(duration_secs).unwrap_or(i64::MAX))
                .expect("clamped pause duration fits in chrono");
        *self.pause_until.lock() = Some(until_value);

        let qm = Arc::clone(self);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(duration_secs)).await;
            // Only this exact timer may resume the queue. A newer timed pause
            // must not be cancelled when an older timer wakes up.
            let should_resume = {
                let until = qm.pause_until.lock();
                until
                    .as_ref()
                    .is_some_and(|current| *current == until_value)
            };
            if should_resume {
                *qm.pause_until.lock() = None;
                // The user's own pause has ended, so lift only their hold. A
                // disk hold keeps its own, and a window that opened while the
                // timed pause covered it takes effect now instead of being
                // resumed straight into.
                qm.expire_user_hold();
                info!("Auto-resumed after timed pause");
            }
        });

        info!(duration_secs, "Paused for duration");
    }

    /// Apply a schedule decision to the queue.
    ///
    /// The schedule holds only what it applied: it will not resume a pause the
    /// user or the disk guard started, and it lifts its own hold only when
    /// the window ends. Holds are additive, so a window that opens while the
    /// disk guard already holds the queue still takes effect: the queue keeps
    /// running paused through the window once the guard lets go, and resumes
    /// at the end of it. Speed caps and server exclusions are overlays — the
    /// configured values are never rewritten.
    ///
    /// `saved` marks a decision applied because the settings were saved, as
    /// opposed to a monitor tick. A save is applied immediately *and* by the
    /// next tick, and a save that leaves a window open must not read as a new
    /// window — otherwise saving settings inside a window would undo a manual
    /// resume. It adopts the open window instead of opening it again.
    pub fn apply_schedule(
        self: &Arc<Self>,
        decision: &crate::nzb_core::schedule::ScheduleDecision,
        enabled: bool,
    ) {
        let idle = crate::nzb_core::schedule::ScheduleDecision::default();
        let decision = if enabled { decision } else { &idle };

        // Windows are keyed by occurrence, not just "open": a pause rule
        // pauses when its window opens, so a manual resume inside it holds
        // until the next window. The next window for the same rule has a
        // different start date, so it does pause again.
        //
        // A decision built by hand carries no occurrence. One anonymous window
        // per open period is enough: closing it forgets the occurrence, so
        // reopening the same decision pauses again.
        let windows = PauseHolds::occurrences(decision.pause_windows.iter().map(String::as_str));
        let windows = if decision.pause && windows.is_empty() {
            vec!["anonymous".to_string()]
        } else {
            windows
        };
        self.apply_schedule_hold(&windows);

        // Speed cap: only touch the limiter when the value changes, so a
        // running download does not see a needless limiter update each tick.
        if self.scheduled_speed_cap.lock().as_ref() != decision.speed_limit_bps.as_ref() {
            self.set_scheduled_speed_cap(decision.speed_limit_bps);
        }

        // Server exclusions.
        if *self.schedule_disabled_servers.lock() != decision.disabled_servers {
            *self.schedule_disabled_servers.lock() = decision.disabled_servers.clone();
            self.apply_server_overlay();
        }
    }

    /// Whether the schedule is the reason the queue is paused right now.
    ///
    /// Its window may be open while a disk hold or a user pause also holds
    /// the queue; in that case the schedule is not what is keeping it
    /// paused, and this reports `false`.
    pub fn schedule_pause_active(&self) -> bool {
        self.pause_holds.lock().reason() == Some(PauseReason::Schedule)
    }

    /// The speed cap the schedule is applying right now, if any.
    pub fn scheduled_speed_cap(&self) -> Option<u64> {
        *self.scheduled_speed_cap.lock()
    }

    /// Get remaining pause time in seconds (None if not timed).
    pub fn pause_remaining_secs(&self) -> Option<i64> {
        let until = self.pause_until.lock();
        until.map(|u| {
            let remaining = u - Utc::now();
            remaining.num_seconds().max(0)
        })
    }

    /// Resume all downloads: lift every hold, the user's included.
    ///
    /// The window's occurrence is left in place, so the rest of that window
    /// is not treated as newly opened: the tick that follows does not pause
    /// again. The next window has a different occurrence and does.
    pub fn resume_all(self: &Arc<Self>) {
        self.resume_hold(|holds| {
            holds.user = false;
            holds.disk = None;
            // The user takes over the open windows for the rest of this
            // occurrence: the schedule must not pause them again, even on the
            // next tick.
            // The occurrence being resumed is the one the schedule last
            // evaluated, which is also the one on the queue's holds.
            holds
                .resumed_windows
                .extend(holds.triggered_windows.iter().cloned());
            holds.resumed_windows.sort();
            holds.resumed_windows.dedup();
            holds.schedule = false;
            holds.user_paused_over.clear();
        });
    }

    // -----------------------------------------------------------------------
    // Server management
    // -----------------------------------------------------------------------

    /// Update the server list at runtime.
    ///
    /// If any enabled servers are present, jobs that were paused due to
    /// server errors (e.g. auth failure / service unavailable) are
    /// automatically resumed.
    pub fn update_servers(self: &Arc<Self>, servers: Vec<ServerConfig>) {
        info!(total = servers.len(), "Updating server list");
        *self.configured_servers.lock() = servers;
        self.apply_server_overlay();
    }

    /// Push the configured list, with the schedule's exclusions disabled, to
    /// the dispatcher.
    fn apply_server_overlay(self: &Arc<Self>) {
        let disabled = self.schedule_disabled_servers.lock().clone();
        let effective: Vec<ServerConfig> = {
            let configured = self.configured_servers.lock();
            configured
                .iter()
                .map(|server| {
                    let mut server = server.clone();
                    if disabled.iter().any(|id| id == &server.id) {
                        server.enabled = false;
                    }
                    server
                })
                .collect()
        };
        let enabled = effective.iter().filter(|s| s.enabled).count();
        info!(total = effective.len(), enabled, "Applying server list");

        self.dispatch.update_servers(effective);

        // Auto-resume jobs paused by server errors now that servers are back.
        if enabled > 0 {
            self.resume_server_paused_jobs();
        }
    }

    /// Resume jobs that were paused due to server unavailability.
    ///
    /// Only targets legacy/restored jobs where `error_message` is set, not
    /// user-paused jobs. New transient provider failures remain downloading.
    fn resume_server_paused_jobs(self: &Arc<Self>) {
        let _transition = self.pause_transition.lock();
        if self.globally_paused.load(Ordering::SeqCst) {
            debug!("Global pause active; deferring automatic server-error resumes");
            return;
        }

        let mut resumed = 0u32;
        {
            let mut jobs = self.jobs.lock();
            for state in jobs.values_mut() {
                if state.job.status == JobStatus::Paused && state.job.error_message.is_some() {
                    state.job.error_message = None;
                    state.failure_code = None;
                    // start_next_queued unpauses or re-submits it within the
                    // active download limit.
                    state.job.status = JobStatus::Queued;
                    resumed += 1;
                }
            }
        }
        if resumed > 0 {
            info!(
                count = resumed,
                "Resumed server-paused jobs after config change"
            );
            self.start_next_queued();
        }
    }

    /// Get the configured server list, before the schedule's overlay.
    pub fn get_servers(&self) -> Vec<ServerConfig> {
        self.configured_servers.lock().clone()
    }

    /// The server list as the dispatcher sees it: configured servers with
    /// the schedule's exclusions disabled.
    pub fn live_servers(&self) -> Vec<ServerConfig> {
        self.servers.lock().clone()
    }

    /// Servers the schedule currently has out of rotation.
    pub fn schedule_disabled_servers(&self) -> Vec<String> {
        self.schedule_disabled_servers.lock().clone()
    }

    // -----------------------------------------------------------------------
    // Query methods (for API handlers)
    // -----------------------------------------------------------------------

    /// Get a snapshot of all jobs in the queue.
    pub fn get_jobs(&self) -> Vec<NzbJob> {
        let mut result = {
            let jobs = self.jobs.lock();
            let order = self.job_order.lock();
            let mut result = Vec::with_capacity(order.len());
            for id in order.iter() {
                if let Some(state) = jobs.get(id) {
                    let mut job = state.job.clone();
                    job.speed_bps = state.speed.bps();
                    result.push(job);
                }
            }
            result
        };
        // The dispatcher's job map is a separate lock; never take it under
        // the job list.
        for job in &mut result {
            self.fill_held_par2_bytes(job);
        }
        result
    }

    /// Set `job.held_par2_bytes` from the volumes the dispatcher still holds.
    fn fill_held_par2_bytes(&self, job: &mut NzbJob) {
        job.held_par2_bytes = match self.dispatch.held_par2_files(&job.id) {
            Some(held) if !held.is_empty() => job
                .files
                .iter()
                .filter(|file| held.contains(&file.id))
                .map(|file| file.bytes)
                .sum(),
            _ => 0,
        };
    }

    /// Get jobs that are still actionable in the active download queue.
    ///
    /// Completed and failed jobs remain in the in-memory snapshot briefly so
    /// internal consumers can observe the terminal transition, but they have
    /// already been persisted to history and must not keep queue views busy.
    pub fn get_active_jobs(&self) -> Vec<NzbJob> {
        self.get_jobs()
            .into_iter()
            .filter(|job| !matches!(job.status, JobStatus::Completed | JobStatus::Failed))
            .collect()
    }

    /// Whether a job with exactly this id is in the queue.
    pub fn contains_job(&self, job_id: &str) -> bool {
        self.jobs.lock().contains_key(job_id)
    }

    /// Get a single job by ID (with files included).
    pub fn get_job(&self, job_id: &str) -> Option<NzbJob> {
        let mut job = self.jobs.lock().get(job_id).map(|state| {
            let mut job = state.job.clone();
            job.speed_bps = state.speed.bps();
            job
        })?;
        self.fill_held_par2_bytes(&mut job);
        Some(job)
    }

    /// Get the current download speed in bytes per second.
    pub fn get_speed(&self) -> u64 {
        self.speed.bps()
    }

    /// Check if downloads are globally paused.
    pub fn is_paused(&self) -> bool {
        self.globally_paused.load(Ordering::SeqCst)
    }

    /// Why downloads are globally paused, or `None` when they are not.
    pub fn global_pause_reason(&self) -> Option<PauseReason> {
        if !self.is_paused() {
            return None;
        }
        self.pause_holds.lock().reason()
    }

    /// When the disk-space guard paused downloads, if it is holding them now.
    pub fn disk_space_hold_since(&self) -> Option<DateTime<Utc>> {
        self.pause_holds.lock().disk
    }

    /// Lift the disk guard's hold, leaving every other hold in place.
    fn clear_disk_hold(self: &Arc<Self>) {
        self.resume_hold(|holds| holds.disk = None);
    }

    /// Why a queued job is paused, or `None` when it is not paused.
    pub fn pause_reason(&self, job_id: &str) -> Option<PauseReason> {
        let (storage_hold, server_error) = {
            let jobs = self.jobs.lock();
            let state = jobs.get(job_id)?;
            if state.job.status != JobStatus::Paused {
                return None;
            }
            (
                state.failure_code == Some(JobFailureCode::StorageUnavailable),
                state.job.error_message.is_some(),
            )
        };
        if storage_hold {
            return Some(PauseReason::DiskSpace);
        }
        if self.globally_paused_jobs.lock().contains(job_id) {
            return Some(self.global_pause_reason().unwrap_or(PauseReason::Global));
        }
        Some(if server_error {
            PauseReason::Server
        } else {
            PauseReason::Manual
        })
    }

    /// Run the disk-space guard once: pause all downloads when a configured
    /// storage volume is below `min_free_space`, and lift the guard's own
    /// holds once every volume has room again. User pauses are never lifted.
    pub fn enforce_disk_guard(self: &Arc<Self>) {
        let threshold = self.min_free_space();
        let paths = self.disk_guard_paths();
        let path_refs: Vec<_> = paths.iter().map(std::path::PathBuf::as_path).collect();
        if !disk_space_available(threshold, &path_refs) {
            if self.disk_space_hold_since().is_none() {
                warn!(
                    free_bytes = paths.first().map_or(0, |path| get_disk_free(path)),
                    min_free_space = threshold,
                    "Low disk space on a configured storage volume, auto-pausing downloads"
                );
                self.pause_all_with_hold(Some(Utc::now()));
                self.notify(NotificationEvent::new(
                    NotificationKind::DiskFull,
                    "Disk space low",
                    "Downloads paused: free space on a storage volume is below the minimum.",
                ));
            }
            return;
        }

        if self.disk_space_hold_since().is_some() {
            info!("Disk space recovered, lifting the disk guard's hold");
            // Only the guard's own hold: a pause window that opened during
            // the hold keeps the queue paused for the rest of the window.
            self.clear_disk_hold();
        }
        self.resume_storage_held_jobs();
    }

    /// Requeue jobs the start-of-download disk check paused, once space is back.
    fn resume_storage_held_jobs(self: &Arc<Self>) {
        let _transition = self.pause_transition.lock();
        if self.is_paused() {
            return;
        }
        let mut resumed = 0u32;
        {
            let mut jobs = self.jobs.lock();
            for state in jobs.values_mut() {
                if state.job.status == JobStatus::Paused
                    && state.failure_code == Some(JobFailureCode::StorageUnavailable)
                {
                    state.job.error_message = None;
                    state.failure_code = None;
                    state.job.status = JobStatus::Queued;
                    resumed += 1;
                }
            }
        }
        if resumed > 0 {
            info!(
                count = resumed,
                "Resumed jobs held for low disk space now that space is available"
            );
            self.start_next_queued();
        }
    }

    /// Get the number of jobs in the queue.
    pub fn queue_size(&self) -> usize {
        self.jobs
            .lock()
            .values()
            .filter(|state| !matches!(state.job.status, JobStatus::Completed | JobStatus::Failed))
            .count()
    }

    /// Get the current incomplete directory.
    pub fn incomplete_dir(&self) -> std::path::PathBuf {
        self.incomplete_dir.lock().clone()
    }

    /// Set the incomplete directory at runtime.
    pub fn set_incomplete_dir(&self, dir: std::path::PathBuf) {
        *self.incomplete_dir.lock() = dir;
    }

    /// Get the current complete directory.
    pub fn complete_dir(&self) -> std::path::PathBuf {
        self.complete_dir.lock().clone()
    }

    /// Set the complete directory at runtime.
    pub fn set_complete_dir(&self, dir: std::path::PathBuf) {
        *self.complete_dir.lock() = dir;
    }

    /// Configured categories, including any per-category output directory.
    pub fn categories(&self) -> Vec<crate::nzb_core::config::CategoryConfig> {
        self.categories.lock().clone()
    }

    /// Resolve a category and job name to the configured output directory.
    /// Both values originate from API/NZB input, so they must remain single
    /// path components before they are joined to a trusted configured root.
    pub fn output_dir_for(
        &self,
        category: &str,
        name: &str,
    ) -> crate::nzb_core::Result<std::path::PathBuf> {
        crate::nzb_core::path::safe_component(category).ok_or_else(|| {
            crate::nzb_core::NzbError::Other("category must be a single safe path component".into())
        })?;
        crate::nzb_core::path::safe_component(name).ok_or_else(|| {
            crate::nzb_core::NzbError::Other("job name must be a single safe path component".into())
        })?;

        let categories = self.categories.lock();
        let category_config = categories
            .iter()
            .find(|configured| configured.name == category);
        if let Some(base) = category_config.and_then(|configured| configured.output_dir.as_ref()) {
            let root = crate::nzb_core::path::category_output_root(&self.complete_dir(), base)
                .ok_or_else(|| {
                    crate::nzb_core::NzbError::Other("category output path is unsafe".into())
                })?;
            return crate::nzb_core::path::safe_join(&root, name).ok_or_else(|| {
                crate::nzb_core::NzbError::Other("category output path is unsafe".into())
            });
        }
        let category_dir = crate::nzb_core::path::safe_join(&self.complete_dir(), category)
            .ok_or_else(|| {
                crate::nzb_core::NzbError::Other("category output path is unsafe".into())
            })?;
        crate::nzb_core::path::safe_join(&category_dir, name)
            .ok_or_else(|| crate::nzb_core::NzbError::Other("job output path is unsafe".into()))
    }

    /// Return every configured filesystem root that may receive job data.
    /// Relative category roots are resolved below the complete directory.
    fn disk_guard_paths(&self) -> Vec<std::path::PathBuf> {
        let complete = self.complete_dir();
        let mut paths = vec![self.incomplete_dir(), complete.clone()];
        for category in self.categories.lock().iter() {
            let Some(root) = category.output_dir.as_ref() else {
                continue;
            };
            let resolved = if root.is_absolute() {
                root.clone()
            } else if let Some(resolved) =
                crate::nzb_core::path::safe_join(&complete, &root.to_string_lossy())
            {
                resolved
            } else {
                continue;
            };
            if !paths.contains(&resolved) {
                paths.push(resolved);
            }
        }
        paths
    }

    /// Get the minimum free disk space threshold.
    pub fn min_free_space(&self) -> u64 {
        self.min_free_space.load(Ordering::Relaxed)
    }

    /// Update the disk guard threshold for both preflight and periodic checks.
    pub fn set_min_free_space(&self, bytes: u64) {
        self.min_free_space.store(bytes, Ordering::Relaxed);
    }

    /// Lock the database and execute a closure with direct access.
    ///
    /// This allows callers (e.g. app-specific handlers) to run arbitrary
    /// queries against the underlying `Database`, such as newsgroup-browsing
    /// operations that only exist when the `groups-db` feature is enabled.
    pub fn with_db<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&Database) -> R,
    {
        let db = self.db.lock();
        f(&db)
    }

    // -----------------------------------------------------------------------
    // History query methods (delegate to DB)
    // -----------------------------------------------------------------------

    /// List history entries.
    pub fn history_list(&self, limit: usize) -> crate::nzb_core::Result<Vec<HistoryEntry>> {
        let db = self.db.lock();
        db.history_list(limit)
    }

    /// One SQL-filtered page of history plus the total match count.
    pub fn history_page(
        &self,
        filter: &HistoryFilter,
        offset: usize,
        limit: usize,
    ) -> crate::nzb_core::Result<(Vec<HistoryEntry>, usize)> {
        self.db.lock().history_page(filter, offset, limit)
    }

    /// Lightweight rows for every history entry matching `filter`.
    pub fn history_summaries(
        &self,
        filter: &HistoryFilter,
    ) -> crate::nzb_core::Result<Vec<HistorySummary>> {
        self.db.lock().history_summaries(filter)
    }

    /// List-shaped history entries for `ids`, in the order given.
    pub fn history_get_listed(&self, ids: &[String]) -> crate::nzb_core::Result<Vec<HistoryEntry>> {
        self.db.lock().history_get_listed(ids)
    }

    /// Aggregate per-server article and byte statistics from active jobs and
    /// persisted history for display in the settings UI.
    pub fn server_stats_get_all(&self, servers: &[ServerConfig]) -> Vec<ServerStatsData> {
        let now = Utc::now();
        let day_cutoff = now - chrono::Duration::days(1);
        let week_cutoff = now - chrono::Duration::days(7);
        let month_cutoff = now - chrono::Duration::days(30);

        let mut stats_by_server: HashMap<String, ServerStatsData> = servers
            .iter()
            .map(|server| {
                (
                    server.id.clone(),
                    ServerStatsData {
                        server_id: server.id.clone(),
                        server_name: server.name.clone(),
                        total_bytes: 0,
                        today_bytes: 0,
                        week_bytes: 0,
                        month_bytes: 0,
                        total_ok: 0,
                        today_ok: 0,
                        week_ok: 0,
                        month_ok: 0,
                        total_fail: 0,
                        today_fail: 0,
                        week_fail: 0,
                        month_fail: 0,
                        last_active: None,
                    },
                )
            })
            .collect();

        let mut apply = |timestamp: DateTime<Utc>,
                         per_server: &[ServerArticleStats],
                         active: bool| {
            for entry in per_server {
                let stats = stats_by_server
                    .entry(entry.server_id.clone())
                    .or_insert_with(|| ServerStatsData {
                        server_id: entry.server_id.clone(),
                        server_name: entry.server_name.clone(),
                        total_bytes: 0,
                        today_bytes: 0,
                        week_bytes: 0,
                        month_bytes: 0,
                        total_ok: 0,
                        today_ok: 0,
                        week_ok: 0,
                        month_ok: 0,
                        total_fail: 0,
                        today_fail: 0,
                        week_fail: 0,
                        month_fail: 0,
                        last_active: None,
                    });
                if stats.server_name.is_empty() {
                    stats.server_name = entry.server_name.clone();
                }

                stats.total_bytes = stats.total_bytes.saturating_add(entry.bytes_downloaded);
                stats.total_ok = stats.total_ok.saturating_add(entry.articles_downloaded);
                stats.total_fail = stats.total_fail.saturating_add(entry.articles_failed);

                if timestamp >= day_cutoff {
                    stats.today_bytes = stats.today_bytes.saturating_add(entry.bytes_downloaded);
                    stats.today_ok = stats.today_ok.saturating_add(entry.articles_downloaded);
                    stats.today_fail = stats.today_fail.saturating_add(entry.articles_failed);
                }
                if timestamp >= week_cutoff {
                    stats.week_bytes = stats.week_bytes.saturating_add(entry.bytes_downloaded);
                    stats.week_ok = stats.week_ok.saturating_add(entry.articles_downloaded);
                    stats.week_fail = stats.week_fail.saturating_add(entry.articles_failed);
                }
                if timestamp >= month_cutoff {
                    stats.month_bytes = stats.month_bytes.saturating_add(entry.bytes_downloaded);
                    stats.month_ok = stats.month_ok.saturating_add(entry.articles_downloaded);
                    stats.month_fail = stats.month_fail.saturating_add(entry.articles_failed);
                }

                if (entry.articles_downloaded > 0
                    || entry.articles_failed > 0
                    || entry.bytes_downloaded > 0)
                    && stats
                        .last_active
                        .is_none_or(|existing| timestamp > existing)
                {
                    stats.last_active = Some(if active { now } else { timestamp });
                }
            }
        };

        let ledger = {
            let db = self.db.lock();
            db.download_statistics_list().unwrap_or_default()
        };
        {
            // A terminal job stays in the map for a few seconds after
            // move_to_history wrote its statistics row; count it once, from
            // the ledger. Live progress is happening now, whenever the job
            // was queued.
            let recorded: HashSet<&str> =
                ledger.iter().map(|entry| entry.job_id.as_str()).collect();
            let jobs = self.jobs.lock();
            for state in jobs.values() {
                if !recorded.contains(state.job.id.as_str()) {
                    apply(now, &state.job.server_stats, true);
                }
            }
        }
        for entry in &ledger {
            apply(entry.completed_at, &entry.server_stats, false);
        }

        let mut stats: Vec<_> = stats_by_server.into_values().collect();
        stats.sort_by(|a, b| {
            a.server_name
                .cmp(&b.server_name)
                .then(a.server_id.cmp(&b.server_id))
        });
        stats
    }

    /// Return permanent global download, speed and NNTP article statistics.
    /// Completed jobs come from the compact statistics ledger, which is not
    /// affected by history retention or user-initiated history deletion.
    pub fn global_statistics(&self, servers: &[ServerConfig]) -> GlobalStatisticsData {
        let generated_at = Utc::now();
        let today_cutoff = generated_at - chrono::Duration::days(1);
        let week_cutoff = generated_at - chrono::Duration::days(7);
        let month_cutoff = generated_at - chrono::Duration::days(30);
        let records = {
            let db = self.db.lock();
            db.download_statistics_list().unwrap_or_default()
        };

        let aggregate = |items: &[&DownloadStatistic]| {
            let mut totals = StatisticsPeriodData::default();
            for item in items {
                totals.downloads += 1;
                match item.status {
                    JobStatus::Completed => totals.completed += 1,
                    JobStatus::Failed => totals.failed += 1,
                    _ => {}
                }
                totals.bytes_downloaded = totals
                    .bytes_downloaded
                    .saturating_add(item.downloaded_bytes);
                totals.total_duration_secs += item.duration_secs;
                totals.fastest_download_bps =
                    totals.fastest_download_bps.max(item.average_speed_bps);
                for server in &item.server_stats {
                    totals.articles_served = totals
                        .articles_served
                        .saturating_add(server.articles_downloaded);
                    totals.articles_missing = totals
                        .articles_missing
                        .saturating_add(server.articles_failed);
                }
            }
            totals.news_server_hits = totals
                .articles_served
                .saturating_add(totals.articles_missing);
            if totals.total_duration_secs > 0.0 {
                totals.average_speed_bps =
                    (totals.bytes_downloaded as f64 / totals.total_duration_secs) as u64;
            }
            totals
        };

        let all: Vec<_> = records.iter().collect();
        let today: Vec<_> = records
            .iter()
            .filter(|item| item.completed_at >= today_cutoff)
            .collect();
        let week: Vec<_> = records
            .iter()
            .filter(|item| item.completed_at >= week_cutoff)
            .collect();
        let month: Vec<_> = records
            .iter()
            .filter(|item| item.completed_at >= month_cutoff)
            .collect();

        let mut by_day: HashMap<String, Vec<&DownloadStatistic>> = HashMap::new();
        for item in &records {
            if item.completed_at >= month_cutoff {
                by_day
                    .entry(item.completed_at.format("%Y-%m-%d").to_string())
                    .or_default()
                    .push(item);
            }
        }
        let mut daily: Vec<_> = by_day
            .into_iter()
            .map(|(date, items)| DailyStatisticsData {
                date,
                totals: aggregate(&items),
            })
            .collect();
        daily.sort_by(|a, b| a.date.cmp(&b.date));

        GlobalStatisticsData {
            generated_at,
            lifetime: aggregate(&all),
            today: aggregate(&today),
            week: aggregate(&week),
            month: aggregate(&month),
            servers: self.server_stats_get_all(servers),
            daily,
        }
    }

    /// Get a single history entry.
    pub fn history_get(&self, id: &str) -> crate::nzb_core::Result<Option<HistoryEntry>> {
        let db = self.db.lock();
        db.history_get(id)
    }

    /// Get raw NZB data for retry.
    pub fn history_get_nzb_data(&self, id: &str) -> crate::nzb_core::Result<Option<Vec<u8>>> {
        let db = self.db.lock();
        db.history_get_nzb_data(id)
    }

    /// Get per-article retry outcomes persisted with a history entry.
    pub fn history_get_retry_data(&self, id: &str) -> crate::nzb_core::Result<Option<Vec<u8>>> {
        let db = self.db.lock();
        db.history_get_retry_data(id)
    }

    /// Remove a history entry.
    pub fn history_remove(&self, id: &str) -> crate::nzb_core::Result<()> {
        let retained = {
            let db = self.db.lock();
            let entry = db.history_get(id)?;
            let retained = entry
                .as_ref()
                .map(|entry| self.history_work_dirs(&db, &entry.id, entry.status))
                .unwrap_or_default();
            db.history_remove(id)?;
            if entry.is_some() {
                self.history_changed();
            }
            retained
        };
        for work_dir in retained {
            self.remove_unreferenced_work_dir(&work_dir, "history entry deleted");
        }
        Ok(())
    }

    /// Move history entries into the undo window instead of deleting them.
    ///
    /// Their retained work directories stay put: the row still owns them
    /// until the undo window closes, so a restore is lossless.
    pub fn history_trash(
        &self,
        ids: &[String],
        deadline: DateTime<Utc>,
    ) -> crate::nzb_core::Result<Vec<HistorySummary>> {
        let trashed = {
            let db = self.db.lock();
            db.history_trash(ids, deadline)?
        };
        if !trashed.is_empty() {
            self.history_changed();
        }
        Ok(trashed)
    }

    /// Bring trashed entries back before their deadline.
    pub fn history_restore(
        &self,
        ids: &[String],
        now: DateTime<Utc>,
    ) -> crate::nzb_core::Result<Vec<String>> {
        let restored = {
            let db = self.db.lock();
            db.history_restore(ids, now)?
        };
        if !restored.is_empty() {
            self.history_changed();
        }
        Ok(restored)
    }

    /// Purge every trashed entry whose undo window has closed, then reclaim
    /// the work directories only those rows still referenced.
    pub fn history_purge_expired(&self, now: DateTime<Utc>) -> crate::nzb_core::Result<usize> {
        let (purged, retained) = {
            let db = self.db.lock();
            let purged = db.history_purge_expired(now)?;
            let retained: Vec<_> = purged
                .iter()
                .flat_map(|entry| self.history_work_dirs(&db, &entry.id, entry.status))
                .collect();
            (purged, retained)
        };
        if purged.is_empty() {
            return Ok(0);
        }
        self.history_changed();
        for work_dir in retained {
            self.remove_unreferenced_work_dir(&work_dir, "trashed history entry expired");
        }
        Ok(purged.len())
    }

    /// Clear all history.
    pub fn history_clear(&self) -> crate::nzb_core::Result<()> {
        let retained = {
            let db = self.db.lock();
            let had_entries = db.history_count()? != 0;
            let retained: Vec<_> = db
                .history_summaries(&HistoryFilter::default())?
                .iter()
                .flat_map(|entry| self.history_work_dirs(&db, &entry.id, entry.status))
                .collect();
            db.history_clear()?;
            if had_entries {
                self.history_changed();
            }
            retained
        };
        for work_dir in retained {
            self.remove_unreferenced_work_dir(&work_dir, "history cleared");
        }
        Ok(())
    }

    /// Incomplete work directories a history row may have retained: its own
    /// `incomplete/<id>` and, for a failed row, the directory its retry
    /// checkpoint names (a retry reuses an earlier attempt's directory).
    fn history_work_dirs(
        &self,
        db: &Database,
        id: &str,
        status: JobStatus,
    ) -> Vec<std::path::PathBuf> {
        let mut dirs = vec![self.incomplete_dir().join(id)];
        if status == JobStatus::Failed
            && let Ok(Some(data)) = db.history_get_retry_data(id)
            && let Some(work_dir) = retry_checkpoint_work_dir(&data)
            && !dirs.contains(&work_dir)
        {
            dirs.push(work_dir);
        }
        dirs
    }

    /// Canonical incomplete directories still referenced by a queue job or a
    /// history row. With `only_named`, retry checkpoints are parsed only when
    /// they mention that directory name, which keeps a single delete cheap.
    ///
    /// Fails closed: any error reading the queue, the history, or one row's
    /// retry checkpoint is returned rather than treated as "nothing references
    /// these directories", because callers use the result to decide deletion.
    fn referenced_work_dirs(
        &self,
        only_named: Option<&std::ffi::OsStr>,
    ) -> crate::nzb_core::Result<HashSet<std::path::PathBuf>> {
        let incomplete = self.incomplete_dir();
        // Terminal jobs linger in the queue view briefly after their history
        // row is written; their directory belongs to that row, not the queue.
        let mut paths: Vec<std::path::PathBuf> = self
            .jobs
            .lock()
            .values()
            .filter(|state| !matches!(state.job.status, JobStatus::Completed | JobStatus::Failed))
            .map(|state| state.job.work_dir.clone())
            .collect();
        let needle = only_named.map(|name| name.to_string_lossy().into_owned().into_bytes());
        {
            let db = self.db.lock();
            paths.extend(db.queue_list()?.into_iter().map(|job| job.work_dir));
            let visible = db.history_summaries(&HistoryFilter::default())?;
            let trashed = db.history_trashed_summaries()?;
            // A trashed row still owns its directory until its undo window
            // closes, so it counts as a reference exactly like a visible one.
            for entry in visible.iter().chain(trashed.iter()) {
                paths.push(incomplete.join(&entry.id));
                if entry.status != JobStatus::Failed {
                    continue;
                }
                let Some(data) = db.history_get_retry_data(&entry.id)? else {
                    continue;
                };
                let mentioned = needle.as_ref().is_none_or(|needle| {
                    !needle.is_empty() && data.windows(needle.len()).any(|window| window == needle)
                });
                if mentioned && let Some(work_dir) = retry_checkpoint_work_dir(&data) {
                    paths.push(work_dir);
                }
            }
        }
        Ok(paths
            .into_iter()
            .filter_map(|path| std::fs::canonicalize(path).ok())
            .collect())
    }

    /// Remove one retained incomplete work directory once nothing references
    /// it. Removal is confined to direct child directories of the incomplete
    /// root and never follows a symlink out of it.
    fn remove_unreferenced_work_dir(&self, candidate: &std::path::Path, reason: &str) {
        let Ok(root) = std::fs::canonicalize(self.incomplete_dir()) else {
            return;
        };
        let Some(work_dir) = incomplete_child_dir(&root, candidate) else {
            return;
        };
        let Ok(referenced) = self.referenced_work_dirs(work_dir.file_name()) else {
            warn!(
                work_dir = %work_dir.display(),
                "Keeping work directory: its references could not be read"
            );
            return;
        };
        if referenced.contains(&work_dir) {
            debug!(work_dir = %work_dir.display(), "Keeping work directory still in use");
            return;
        }
        let size_bytes = tree_size(&work_dir);
        match std::fs::remove_dir_all(&work_dir) {
            Ok(()) => info!(
                work_dir = %work_dir.display(),
                size_bytes,
                reason,
                "Removed retained work directory"
            ),
            Err(e) => warn!(
                work_dir = %work_dir.display(),
                "Failed to remove retained work directory: {e}"
            ),
        }
    }

    /// Remove incomplete work directories that no queue job or history row
    /// references, such as partial downloads whose history was deleted while
    /// the process was down. Only direct child directories of the incomplete
    /// root whose names are job ids are considered; files, symlinks, other
    /// directories, and the root itself are kept. Runs at startup, before any
    /// job can create a new work directory.
    fn sweep_orphaned_work_dirs(&self) {
        let incomplete = self.incomplete_dir();
        let Ok(root) = std::fs::canonicalize(&incomplete) else {
            return;
        };
        let Ok(entries) = std::fs::read_dir(&root) else {
            return;
        };
        let Ok(referenced) = self.referenced_work_dirs(None) else {
            warn!("Skipping orphan sweep: queue or history could not be read");
            return;
        };
        for entry in entries.flatten() {
            if !is_job_id_dir_name(&entry.file_name()) {
                continue;
            }
            let Some(work_dir) = incomplete_child_dir(&root, &entry.path()) else {
                continue;
            };
            if referenced.contains(&work_dir) {
                continue;
            }
            let size_bytes = tree_size(&work_dir);
            match std::fs::remove_dir_all(&work_dir) {
                Ok(()) => info!(
                    work_dir = %work_dir.display(),
                    size_bytes,
                    "Removed orphaned incomplete work directory at startup"
                ),
                Err(e) => warn!(
                    work_dir = %work_dir.display(),
                    "Failed to remove orphaned incomplete work directory: {e}"
                ),
            }
        }
    }

    /// Get live logs for an active job from the in-memory log buffer.
    pub fn get_job_logs(&self, job_id: &str, limit: usize) -> Vec<crate::log_buffer::LogEntry> {
        if let Some(ref lb) = self.log_buffer {
            lb.get_entries(Some(job_id), None, None, limit)
        } else {
            Vec::new()
        }
    }

    /// Get persisted logs for a history entry.
    pub fn history_get_logs(&self, id: &str) -> crate::nzb_core::Result<Option<String>> {
        let db = self.db.lock();
        db.history_get_logs(id)
    }

    // -----------------------------------------------------------------------
    // RSS item/rule query methods (delegate to DB)
    // -----------------------------------------------------------------------

    /// List RSS feed items.
    pub fn rss_items_list(
        &self,
        feed_name: Option<&str>,
        limit: usize,
    ) -> crate::nzb_core::Result<Vec<RssItem>> {
        let db = self.db.lock();
        db.rss_items_list(feed_name, limit)
    }

    /// Get a single RSS item by ID.
    pub fn rss_item_get(&self, id: &str) -> crate::nzb_core::Result<Option<RssItem>> {
        let db = self.db.lock();
        db.rss_item_get(id)
    }

    /// Mark an RSS item as downloaded.
    pub fn rss_item_mark_downloaded(
        &self,
        id: &str,
        category: Option<&str>,
    ) -> crate::nzb_core::Result<()> {
        let db = self.db.lock();
        db.rss_item_mark_downloaded(id, category)
    }

    /// Upsert an RSS feed item.
    pub fn rss_item_upsert(&self, item: &RssItem) -> crate::nzb_core::Result<()> {
        let db = self.db.lock();
        db.rss_item_upsert(item)
    }

    /// Batch upsert RSS feed items (single DB lock + transaction).
    pub fn rss_items_batch_upsert(&self, items: &[RssItem]) -> crate::nzb_core::Result<usize> {
        let db = self.db.lock();
        db.rss_items_batch_upsert(items)
    }

    /// Check if an RSS item exists.
    pub fn rss_item_exists(&self, id: &str) -> crate::nzb_core::Result<bool> {
        let db = self.db.lock();
        db.rss_item_exists(id)
    }

    /// Count total RSS items.
    pub fn rss_item_count(&self) -> crate::nzb_core::Result<usize> {
        let db = self.db.lock();
        db.rss_item_count()
    }

    /// Prune RSS items to keep only N most recent.
    pub fn rss_items_prune(&self, keep: usize) -> crate::nzb_core::Result<usize> {
        let db = self.db.lock();
        db.rss_items_prune(keep)
    }

    /// Expire downloaded RSS records older than the supplied RFC3339 cutoff.
    pub fn rss_items_expire_downloaded(&self, cutoff: &str) -> crate::nzb_core::Result<usize> {
        let db = self.db.lock();
        db.rss_items_expire_downloaded(cutoff)
    }

    /// List all RSS download rules.
    pub fn rss_rule_list(&self) -> crate::nzb_core::Result<Vec<RssRule>> {
        let db = self.db.lock();
        db.rss_rule_list()
    }

    /// Insert a new RSS download rule.
    pub fn rss_rule_insert(&self, rule: &RssRule) -> crate::nzb_core::Result<()> {
        let db = self.db.lock();
        db.rss_rule_insert(rule)
    }

    /// Update an RSS download rule.
    pub fn rss_rule_update(&self, rule: &RssRule) -> crate::nzb_core::Result<()> {
        let db = self.db.lock();
        db.rss_rule_update(rule)
    }

    /// Delete an RSS download rule.
    pub fn rss_rule_delete(&self, id: &str) -> crate::nzb_core::Result<()> {
        let db = self.db.lock();
        db.rss_rule_delete(id)
    }

    // -----------------------------------------------------------------------
    // Startup: restore jobs from DB
    // -----------------------------------------------------------------------

    /// Restore in-progress jobs from the database on startup.
    ///
    /// Re-parses NZB data for each job and applies any saved checkpoint to
    /// mark already-downloaded articles, so downloads resume where they left off.
    pub fn restore_from_db(self: &Arc<Self>) -> crate::nzb_core::Result<()> {
        prune_damage_ledger(&self.db.lock());
        // Restore globally_paused from persisted state. Every hold that was
        // persisted comes back as its own flag, so a restart cannot turn a
        // schedule pause into a user pause the schedule can never lift (or
        // the other way around).
        let restored = {
            let db = self.db.lock();
            let paused = db
                .get_setting("globally_paused")
                .is_some_and(|v| v == "true");
            let ids = db
                .get_setting(Self::GLOBAL_PAUSED_JOBS_SETTING)
                .and_then(|value| serde_json::from_str::<HashSet<String>>(&value).ok());
            let flag = |key: &str| db.get_setting(key).is_some_and(|value| value == "true");
            let list = |key: &str| {
                db.get_setting(key)
                    .and_then(|value| serde_json::from_str::<Vec<String>>(&value).ok())
                    .unwrap_or_default()
            };
            let disk = db
                .get_setting(Self::DISK_SPACE_HOLD_SETTING)
                .and_then(|value| DateTime::parse_from_rfc3339(&value).ok())
                .map(|at| at.with_timezone(&Utc));
            let owner = db
                .get_setting(Self::PAUSE_OWNER_SETTING)
                .unwrap_or_default();
            // Records written before each hold had its own flag describe one
            // reason only, so they fall back to the owner.
            let legacy = db.get_setting(Self::USER_HOLD_SETTING).is_none()
                && db.get_setting(Self::SCHEDULE_HOLD_SETTING).is_none();
            let holds = PauseHolds {
                user: if legacy {
                    owner == Self::PAUSE_OWNER_USER || (owner.is_empty() && disk.is_none())
                } else {
                    flag(Self::USER_HOLD_SETTING)
                },
                disk,
                schedule: if legacy {
                    owner == Self::PAUSE_OWNER_SCHEDULE
                } else {
                    flag(Self::SCHEDULE_HOLD_SETTING)
                },
                resumed_windows: list(Self::SCHEDULE_RESUMED_SETTING),
                open_windows: list(Self::SCHEDULE_WINDOWS_SETTING),
                triggered_windows: list(Self::SCHEDULE_TRIGGERED_SETTING),
                user_paused_over: list(Self::SCHEDULE_USER_PAUSED_SETTING),
            };
            (paused, ids, holds)
        };
        let (was_paused, persisted_global_ids, persisted_holds) = restored;
        // The holds themselves only come back when the queue was paused: a
        // record that says it was not paused has no live hold to restore.
        //
        // The window bookkeeping is different, and is restored either way. A
        // manual resume inside a window leaves `globally_paused` false while
        // the occurrence it overrode is still open; dropping `triggered_windows`
        // and `resumed_windows` with it would make the first tick after a
        // restart read that window as newly opened and pause the queue again.
        let holds = PauseHolds {
            user: was_paused && persisted_holds.user,
            disk: if was_paused {
                persisted_holds.disk
            } else {
                None
            },
            schedule: was_paused && persisted_holds.schedule,
            open_windows: persisted_holds.open_windows,
            triggered_windows: persisted_holds.triggered_windows,
            resumed_windows: persisted_holds.resumed_windows,
            user_paused_over: persisted_holds.user_paused_over,
        };
        {
            let mut current = self.pause_holds.lock();
            *current = holds.clone();
        }
        // Load, do not save: restoring must not materialise settings a
        // database never held, or a first boot would look like a chosen pause.
        self.store_holds(&holds);
        info!(
            was_paused,
            disk_space_hold = holds.disk.is_some(),
            schedule_hold = holds.schedule,
            user_hold = holds.user,
            open_windows = ?holds.open_windows,
            "Restored global pause state from database"
        );

        let complete_action = self
            .db
            .lock()
            .get_setting(Self::QUEUE_COMPLETE_ACTION_SETTING)
            .filter(|value| !value.is_empty());
        *self.complete_action.lock() = complete_action;

        // Reclaim partial downloads nothing can reach any more before any
        // job starts writing into the incomplete directory.
        self.sweep_orphaned_work_dirs();

        let jobs = {
            let db = self.db.lock();
            db.queue_list()?
        };

        if jobs.is_empty() {
            return Ok(());
        }

        info!(count = jobs.len(), "Restoring jobs from database");

        let mut postproc_recovery = Vec::new();
        for mut job in jobs {
            let job_id = job.id.clone();

            // A process can stop after the final article closed but before the
            // pipeline committed history. Resume from the idempotent stage
            // boundary instead of leaving the job permanently stranded.
            let was_post_processing = matches!(
                job.status,
                JobStatus::PostProcessing
                    | JobStatus::Verifying
                    | JobStatus::Repairing
                    | JobStatus::Extracting
            );
            if was_post_processing {
                job.status = JobStatus::PostProcessing;
                postproc_recovery.push((job_id.clone(), job.articles_failed));
            }

            // Only load full NZB data + checkpoints for jobs that were actively
            // downloading. Queued/paused jobs just need metadata — their NZB data
            // is loaded lazily in launch_download() when they reach the front of
            // the queue. This keeps memory low with large queues (hundreds of jobs).
            let was_active = job.status == JobStatus::Downloading;
            let mut output_dir_claimed = false;
            let mut prior_download_secs = 0.0;
            let mut carried_bytes = 0;
            let mut download_time_secs = None;

            let nzb_data = if was_active {
                let db = self.db.lock();
                db.queue_get_nzb_data(&job_id).unwrap_or(None)
            } else {
                None
            };

            if was_active {
                // Re-parse NZB to populate files and articles
                if let Some(ref data) = nzb_data {
                    match nzb_parser::parse_nzb(&job.name, data) {
                        Ok(parsed) => {
                            job.files = parsed.files;
                        }
                        Err(e) => {
                            warn!(job_id = %job_id, "Failed to re-parse NZB data: {e}");
                        }
                    }
                }

                // Load and apply checkpoint to mark downloaded articles
                let checkpoint_data = {
                    let db = self.db.lock();
                    db.queue_load_job_data(&job_id).unwrap_or(None)
                };

                if let Some(ref data) = checkpoint_data {
                    match serde_json::from_slice::<JobCheckpoint>(data) {
                        Ok(checkpoint) => {
                            apply_checkpoint(&mut job, &checkpoint);
                            output_dir_claimed = restore_claimed_output_dir(&mut job, &checkpoint);
                            (prior_download_secs, carried_bytes) =
                                restore_checkpoint_accounting(&mut job, &checkpoint);

                            let remaining = job
                                .article_count
                                .saturating_sub(job.articles_downloaded + job.articles_failed);
                            info!(
                                job_id = %job_id,
                                name = %job.name,
                                articles_downloaded = job.articles_downloaded,
                                articles_failed = job.articles_failed,
                                remaining,
                                "Restored job checkpoint — resuming from previous progress"
                            );
                        }
                        Err(e) => {
                            warn!(
                                job_id = %job_id,
                                "Failed to deserialize checkpoint, starting from scratch: {e}"
                            );
                        }
                    }
                }
            } else if was_post_processing {
                // Interrupted post-processing resumes into the folder it had
                // already reserved rather than reserving a second one.
                let checkpoint_data = {
                    let db = self.db.lock();
                    db.queue_load_job_data(&job_id).unwrap_or(None)
                };
                if let Some(checkpoint) = checkpoint_data
                    .as_deref()
                    .and_then(|data| serde_json::from_slice::<JobCheckpoint>(data).ok())
                {
                    output_dir_claimed = restore_claimed_output_dir(&mut job, &checkpoint);
                    (prior_download_secs, carried_bytes) =
                        restore_checkpoint_accounting(&mut job, &checkpoint);
                    // The download already resolved; its checkpoint holds the
                    // final active time. Older checkpoints have none.
                    download_time_secs = (prior_download_secs > 0.0).then_some(prior_download_secs);
                }
            }

            let paused_by_global = was_paused
                && persisted_global_ids
                    .as_ref()
                    .map_or(job.status == JobStatus::Paused, |ids| ids.contains(&job_id));
            if paused_by_global {
                job.status = JobStatus::Paused;
                self.globally_paused_jobs.lock().insert(job_id.clone());
            } else if job.status == JobStatus::Downloading {
                job.status = JobStatus::Queued;
            }

            let state = JobState {
                job,
                progress_handle: None,
                speed: Arc::new(SpeedTracker::new()),
                nzb_data,
                direct_unpacker: None,
                hopeless_tracker: None,
                repair_fold: None,
                download_time_secs,
                prior_download_secs,
                carried_bytes,
                failure_code: None,
                par2_volumes: None,
                output_dir_claimed,
                persisted_status: None,
            };
            self.jobs.lock().insert(job_id.clone(), state);
            self.job_order.lock().push(job_id);
        }

        // Start queued jobs up to the concurrency limit
        self.start_next_queued();

        for (job_id, articles_failed) in postproc_recovery {
            let manager = Arc::clone(self);
            tokio::spawn(async move {
                info!(job_id, "Resuming interrupted post-processing pipeline");
                manager
                    .on_job_finished(&job_id, articles_failed == 0, articles_failed)
                    .await;
            });
        }

        Ok(())
    }

    // -----------------------------------------------------------------------
    // Background task: speed calculation
    // -----------------------------------------------------------------------

    /// Spawn the background task that periodically updates the speed counter.
    /// Gracefully shut down the queue manager.
    ///
    /// Cancels all in-flight downloads, waits for tasks to stop, and persists
    /// final job state to the database so progress is not lost.
    pub async fn shutdown(&self) {
        info!("Shutting down queue manager...");

        // 1. Set globally paused to prevent new downloads from starting
        self.globally_paused.store(true, Ordering::Relaxed);

        // 2. Mark downloading jobs as Queued for next restart, and collect
        //    progress-handler task handles so they can be aborted after the
        //    pool drains.
        let mut handles = Vec::new();
        let mut interrupted: HashSet<String> = HashSet::new();
        {
            let mut jobs = self.jobs.lock();
            for (id, state) in jobs.iter_mut() {
                if state.job.status == JobStatus::Downloading {
                    info!(job_id = %id, "Marking download for shutdown");
                    state.job.status = JobStatus::Queued; // Will resume on restart
                    interrupted.insert(id.clone());
                }
                if let Some(handle) = state.progress_handle.take() {
                    handles.push(handle);
                }
            }
        }

        // 3. Shut down the worker pool gracefully. In-flight articles finish
        //    first (finish-in-flight), then workers exit.
        self.dispatch.shutdown().await;

        // 4. Let the per-job progress listeners record the events the pool
        //    queued before it stopped. Their senders live on in the pool's
        //    job contexts, so signal them to drain and exit; abort any that
        //    miss the deadline.
        self.progress_shutdown.send_replace(true);
        let deadline = tokio::time::Instant::now() + PROGRESS_DRAIN_TIMEOUT;
        for mut handle in handles {
            if tokio::time::timeout_at(deadline, &mut handle)
                .await
                .is_err()
            {
                warn!("Progress listener did not drain before the shutdown deadline; aborting it");
                handle.abort();
            }
        }

        // 5. Persist final state for all jobs to DB
        {
            let jobs = self.jobs.lock();
            let db = self.db.lock();
            for (id, state) in jobs.iter() {
                if let Err(e) = db.queue_update_progress(
                    id,
                    state.job.status,
                    state.job.downloaded_bytes,
                    state.job.articles_downloaded,
                    state.job.articles_failed,
                    state.job.files_completed,
                ) {
                    error!(job_id = %id, error = %e, "Failed to persist job state on shutdown");
                }
            }
        }

        // 6. Flush per-file article checkpoints for downloads that were
        //    mid-flight, so a restart resumes from the last completed
        //    article instead of re-downloading whole files.
        if !interrupted.is_empty() {
            let jobs = self.jobs.lock();
            let db = self.db.lock();
            for id in &interrupted {
                let Some(state) = jobs.get(id) else {
                    continue;
                };
                let checkpoint = checkpoint_for_job(&state.job, state.output_dir_claimed);
                match serde_json::to_vec(&checkpoint) {
                    Ok(data) => {
                        if let Err(e) = db.queue_store_job_data(id, &data) {
                            error!(job_id = %id, error = %e, "Failed to flush article checkpoint on shutdown");
                        }
                    }
                    Err(e) => {
                        error!(job_id = %id, error = %e, "Failed to serialize article checkpoint on shutdown");
                    }
                }
            }
        }

        info!("Queue manager shutdown complete");
    }

    pub fn spawn_speed_tracker(self: &Arc<Self>) {
        let qm = Arc::clone(self);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(1));
            let mut tick_count: u64 = 0;
            loop {
                interval.tick().await;
                qm.speed.tick(1.0);
                // Tick per-job speed trackers
                {
                    let jobs = qm.jobs.lock();
                    for state in jobs.values() {
                        state.speed.tick(1.0);
                    }
                }

                // Phase 6: time-based hopeless scan. Catches downloads
                // that have stopped emitting article progress for too
                // long, including late-stage stalls after partial success.
                qm.scan_for_no_progress_jobs();

                // Observability: every 10s, dump the state of every
                // active download so operators can see at a glance what
                // each job is doing — progress, failure ratio, hopeless
                // tracker state, time since job start. This is the line
                // to grep when users report "stuck at 0 KB/s" — it shows
                // whether the engine is genuinely stuck, making slow
                // progress, or burning through failed articles.
                if tick_count.is_multiple_of(10) {
                    let snapshots: Vec<_> = {
                        let jobs = qm.jobs.lock();
                        jobs.iter()
                            .filter(|(_, s)| {
                                matches!(
                                    s.job.status,
                                    JobStatus::Downloading
                                        | JobStatus::Queued
                                        | JobStatus::PostProcessing
                                )
                            })
                            .map(|(id, s)| {
                                let (
                                    tracker_checked,
                                    tracker_failed,
                                    tracker_content_total,
                                    tracker_elapsed_secs,
                                    tracker_idle_secs,
                                    tracker_content_bytes,
                                    tracker_content_missing,
                                    tracker_usable_recovery,
                                ) = s
                                    .hopeless_tracker
                                    .as_ref()
                                    .map(|t| {
                                        (
                                            t.content_articles_checked,
                                            t.content_articles_failed,
                                            t.content_articles_total,
                                            t.created_at.elapsed().as_secs(),
                                            t.last_progress_at.elapsed().as_secs(),
                                            t.content_bytes,
                                            t.content_bytes_missing,
                                            t.recovery_capacity_bytes
                                                .saturating_sub(t.recovery_bytes_unavailable),
                                        )
                                    })
                                    .unwrap_or((0, 0, 0, 0, 0, 0, 0, 0));
                                (
                                    id.clone(),
                                    s.job.name.clone(),
                                    s.job.status,
                                    s.job.articles_downloaded,
                                    s.job.articles_failed,
                                    s.job.article_count,
                                    s.job.downloaded_bytes,
                                    s.job.total_bytes,
                                    s.speed.bps(),
                                    tracker_checked,
                                    tracker_failed,
                                    tracker_content_total,
                                    tracker_elapsed_secs,
                                    tracker_idle_secs,
                                    tracker_content_bytes,
                                    tracker_content_missing,
                                    tracker_usable_recovery,
                                )
                            })
                            .collect()
                    };
                    for (
                        job_id,
                        name,
                        status,
                        dl,
                        failed,
                        total_art,
                        dl_bytes,
                        total_bytes,
                        bps,
                        t_checked,
                        t_failed,
                        t_total,
                        t_elapsed,
                        t_idle,
                        t_bytes_total,
                        t_bytes_missing,
                        t_usable_recovery,
                    ) in snapshots
                    {
                        let pct_bytes = if total_bytes > 0 {
                            (dl_bytes as f64 / total_bytes as f64) * 100.0
                        } else {
                            0.0
                        };
                        let avail_pct = if t_bytes_total > 0 {
                            let avail: u64 = t_bytes_total
                                .saturating_sub(t_bytes_missing)
                                .saturating_add(t_usable_recovery);
                            100.0 * (avail as f64 / t_bytes_total as f64)
                        } else {
                            100.0
                        };
                        info!(
                            job_id = %job_id,
                            name = %name,
                            status = %status,
                            pct = format!("{pct_bytes:.1}"),
                            dl_articles = dl,
                            failed_articles = failed,
                            total_articles = total_art,
                            dl_bytes,
                            total_bytes,
                            kbps = bps / 1024,
                            elapsed_secs = t_elapsed,
                            idle_secs = t_idle,
                            tracker_checked = t_checked,
                            tracker_failed = t_failed,
                            tracker_total = t_total,
                            effective_completion_pct = format!("{avail_pct:.3}"),
                            missing_content_bytes = t_bytes_missing,
                            usable_recovery_bytes = t_usable_recovery,
                            "Job status snapshot"
                        );
                    }
                }

                // Periodic connection count + disk space checks (every 30 seconds)
                tick_count += 1;
                if tick_count.is_multiple_of(30) {
                    // Log active NNTP connections per server
                    let snapshot = qm.dispatch.active_connection_snapshot();
                    let total: usize = snapshot.iter().map(|(_, c, _)| *c).sum();
                    if total > 0 {
                        for (server_id, count, limit) in &snapshot {
                            if *count > 0 {
                                let server_name = qm
                                    .servers
                                    .lock()
                                    .iter()
                                    .find(|s| s.id == *server_id)
                                    .map(|s| s.name.clone())
                                    .unwrap_or_else(|| server_id.clone());
                                if *limit > 0 && *count > *limit {
                                    warn!(
                                        server = %server_name,
                                        active = count,
                                        limit,
                                        "NNTP connections EXCEED limit"
                                    );
                                } else {
                                    info!(
                                        server = %server_name,
                                        active = count,
                                        limit,
                                        "NNTP connection count"
                                    );
                                }
                            }
                        }
                        info!(total_nntp_connections = total, "NNTP connection summary");
                    }
                }
                if tick_count.is_multiple_of(30) {
                    qm.enforce_disk_guard();
                    // Trashed history rows are hard-deleted once their undo
                    // window closes; that is also when their retained work
                    // directories become reclaimable.
                    match qm.history_purge_expired(Utc::now()) {
                        Ok(0) => {}
                        Ok(count) => info!(count, "Purged trashed history entries"),
                        Err(e) => warn!(error = %e, "Failed to purge trashed history entries"),
                    }
                }
            }
        });
    }
}

#[cfg(test)]
mod global_pause_tests {
    use super::*;

    #[test]
    fn terminal_notification_maps_outcomes() {
        let root = std::path::Path::new("/unused");
        let mut done = job("done", JobStatus::Completed, root);
        done.category = "tv".into();

        let complete = terminal_notification(&done, JobStatus::Completed).unwrap();
        assert_eq!(complete.kind, NotificationKind::Complete);
        assert_eq!(complete.job_name.as_deref(), Some("done"));
        assert_eq!(complete.category.as_deref(), Some("tv"));

        let mut broken = job("broken", JobStatus::Failed, root);
        broken.error_message = Some("archive is corrupt".into());
        let failed = terminal_notification(&broken, JobStatus::Failed).unwrap();
        assert_eq!(failed.kind, NotificationKind::Failed);
        assert_eq!(failed.message, "archive is corrupt");

        let mut full = job("full", JobStatus::Failed, root);
        full.error_message = Some(POSTPROC_DISK_FULL_MESSAGE.into());
        let disk = terminal_notification(&full, JobStatus::Failed).unwrap();
        assert_eq!(disk.kind, NotificationKind::DiskFull);

        let paused = job("paused", JobStatus::Paused, root);
        assert!(terminal_notification(&paused, JobStatus::Paused).is_none());
    }

    fn job(id: &str, status: JobStatus, root: &std::path::Path) -> NzbJob {
        NzbJob {
            slice_map: None,
            held_par2_bytes: 0,
            id: id.to_string(),
            name: id.to_string(),
            category: "Default".to_string(),
            status,
            priority: Priority::Normal,
            total_bytes: 1,
            downloaded_bytes: 0,
            file_count: 0,
            files_completed: 0,
            article_count: 0,
            articles_downloaded: 0,
            articles_failed: 0,
            added_at: Utc::now(),
            completed_at: None,
            work_dir: root.join(id),
            output_dir: root.join("complete").join(id),
            password: None,
            error_message: None,
            speed_bps: 0,
            pp_override: None,
            delete_archives: None,
            server_stats: Vec::new(),
            files: Vec::new(),
        }
    }

    #[tokio::test]
    async fn post_processing_level_prefers_the_job_override() {
        let (manager, tempdir) = manager();
        manager.set_categories(vec![CategoryConfig {
            post_processing: 2,
            ..CategoryConfig::default()
        }]);
        let mut job = job("pp-level", JobStatus::Queued, tempdir.path());
        assert_eq!(manager.post_processing_level(&job), 2);

        job.pp_override = Some(0);
        assert_eq!(manager.post_processing_level(&job), 0);

        job.pp_override = None;
        job.category = "unconfigured".into();
        assert_eq!(manager.post_processing_level(&job), 3);
    }

    const DUPLICATE_NZB: &[u8] = br#"<?xml version="1.0" encoding="UTF-8"?>
<nzb xmlns="http://www.newzbin.com/DTD/2003/nzb">
  <file poster="poster@example.com" date="1234567890" subject="show.s01e01.rar (1/1)">
    <groups><group>alt.binaries.test</group></groups>
    <segments><segment bytes="100" number="1">show-s01e01-1@example.com</segment></segments>
  </file>
</nzb>"#;

    fn duplicate_payload_variant() -> Vec<u8> {
        String::from_utf8(DUPLICATE_NZB.to_vec())
            .unwrap()
            .replace("show-s01e01-1@", "show-s01e01-2@")
            .into_bytes()
    }

    fn admit_duplicate_candidate(
        manager: &Arc<QueueManager>,
        tempdir: &tempfile::TempDir,
        name: &str,
        payload: &[u8],
    ) -> crate::nzb_core::Result<String> {
        let mut job = nzb_parser::parse_nzb(name, payload).expect("parse");
        job.work_dir = tempdir.path().join("incomplete").join(&job.id);
        job.output_dir = tempdir.path().join("complete").join(&job.name);
        let id = job.id.clone();
        manager.add_job(job, Some(payload.to_vec()))?;
        Ok(id)
    }

    /// The default `warn` policy admits a repeat, as before, and records the match.
    #[tokio::test]
    async fn same_nzb_is_admitted_twice_under_default_warn_policy() {
        let (manager, tempdir) = manager();
        assert_eq!(manager.duplicate_policy(), DuplicatePolicy::Warn);
        for _ in 0..2 {
            admit_duplicate_candidate(&manager, &tempdir, "Show.S01E01", DUPLICATE_NZB)
                .expect("warn admits");
        }
        assert_eq!(manager.queue_size(), 2);
    }

    #[tokio::test]
    async fn skip_policy_refuses_a_repeat_and_leaves_no_directory() {
        let (manager, tempdir) = manager();
        manager.set_duplicate_policy(DuplicatePolicy::Skip);
        admit_duplicate_candidate(&manager, &tempdir, "Show.S01E01", DUPLICATE_NZB)
            .expect("first copy admitted");

        let refused = admit_duplicate_candidate(&manager, &tempdir, "Show.S01E01", DUPLICATE_NZB);
        assert!(
            matches!(refused, Err(crate::nzb_core::NzbError::DuplicateNzb(ref message)) if message.contains("queue job")),
            "{refused:?}"
        );
        assert_eq!(manager.queue_size(), 1);
        let refused_job = nzb_parser::parse_nzb("Show.S01E01", DUPLICATE_NZB).unwrap();
        assert!(
            !tempdir
                .path()
                .join("incomplete")
                .join(&refused_job.id)
                .exists()
        );
    }

    #[tokio::test]
    async fn skip_policy_matches_by_name_when_the_payload_differs() {
        let (manager, tempdir) = manager();
        manager.set_duplicate_policy(DuplicatePolicy::Skip);
        admit_duplicate_candidate(&manager, &tempdir, "Show.S01E01", DUPLICATE_NZB)
            .expect("first copy admitted");

        let renamed_payload = duplicate_payload_variant();
        let refused =
            admit_duplicate_candidate(&manager, &tempdir, "show.s01e01", &renamed_payload);
        assert!(matches!(
            refused,
            Err(crate::nzb_core::NzbError::DuplicateNzb(_))
        ));
        assert_eq!(manager.queue_size(), 1);
    }

    #[tokio::test]
    async fn allow_policy_admits_repeats_without_checking() {
        let (manager, tempdir) = manager();
        manager.set_duplicate_policy(DuplicatePolicy::Allow);
        for _ in 0..2 {
            admit_duplicate_candidate(&manager, &tempdir, "Show.S01E01", DUPLICATE_NZB)
                .expect("allow admits");
        }
        assert_eq!(manager.queue_size(), 2);
    }

    #[tokio::test]
    async fn skip_policy_refuses_a_repeat_of_a_finished_job() {
        let (manager, tempdir) = manager();
        manager.set_duplicate_policy(DuplicatePolicy::Skip);
        let finished = HistoryEntry {
            id: "finished-show".into(),
            name: "Show.S01E01".into(),
            category: "Default".into(),
            status: JobStatus::Completed,
            total_bytes: 100,
            downloaded_bytes: 100,
            added_at: Utc::now(),
            completed_at: Utc::now(),
            download_time_secs: None,
            output_dir: tempdir.path().join("complete").join("Show.S01E01"),
            stages: Vec::new(),
            error_message: None,
            failure_code: None,
            post_processing: None,
            delete_archives: None,
            server_stats: Vec::new(),
            nzb_data: Some(DUPLICATE_NZB.to_vec()),
            retry_data: None,
            par2_volumes_fetched: None,
            par2_volumes_available: None,
        };
        manager.db.lock().history_insert(&finished).unwrap();

        let refused = admit_duplicate_candidate(&manager, &tempdir, "Show.S01E01", DUPLICATE_NZB);
        assert!(matches!(
            refused,
            Err(crate::nzb_core::NzbError::DuplicateNzb(ref message)) if message.contains("history job")
        ));
    }

    #[tokio::test]
    async fn history_retry_is_not_refused_by_its_own_history_row() {
        let (manager, tempdir) = manager();
        manager.set_duplicate_policy(DuplicatePolicy::Skip);
        let failed = HistoryEntry {
            id: "failed-show".into(),
            name: "Show.S01E01".into(),
            category: "Default".into(),
            status: JobStatus::Failed,
            total_bytes: 100,
            downloaded_bytes: 10,
            added_at: Utc::now(),
            completed_at: Utc::now(),
            download_time_secs: None,
            output_dir: tempdir.path().join("complete").join("Show.S01E01"),
            stages: Vec::new(),
            error_message: Some("incomplete".into()),
            failure_code: Some(JobFailureCode::ArticlesUnavailable),
            post_processing: None,
            delete_archives: None,
            server_stats: Vec::new(),
            nzb_data: Some(DUPLICATE_NZB.to_vec()),
            retry_data: None,
            par2_volumes_fetched: None,
            par2_volumes_available: None,
        };
        manager.db.lock().history_insert(&failed).unwrap();

        let outcome = manager
            .retry_history_entry(&failed)
            .expect("retry is not a duplicate of itself");
        assert!(matches!(outcome, HistoryRetryOutcome::Started(_)));
        assert_eq!(manager.queue_size(), 1);
    }

    fn manager() -> (Arc<QueueManager>, tempfile::TempDir) {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let db = Database::open_memory().expect("database");
        let manager = build_manager(db, tempdir.path());
        (manager, tempdir)
    }

    /// A manager over a database file in `dir`, so a second one built over the
    /// same path sees what the first persisted. In-memory handles are shared
    /// by clone, which cannot model a restart: restoring into one still reads
    /// the same live state it already had.
    fn file_manager(dir: &std::path::Path) -> Arc<QueueManager> {
        let db = Database::open(&dir.join("restart").with_extension("db")).expect("database");
        build_manager(db, dir)
    }

    fn build_manager(db: Database, dir: &std::path::Path) -> Arc<QueueManager> {
        QueueManager::new(
            Vec::new(),
            db,
            dir.join("incomplete"),
            dir.join("complete"),
            LogBuffer::default(),
            1,
            Vec::new(),
            0,
            0,
            false,
            5,
            false,
            false,
            100.0,
            30,
        )
    }

    fn insert_job(manager: &QueueManager, job: NzbJob) {
        let id = job.id.clone();
        manager
            .db
            .lock()
            .queue_insert(&job)
            .expect("insert queue row");
        manager.jobs.lock().insert(
            id.clone(),
            JobState {
                job,
                progress_handle: None,
                speed: Arc::new(SpeedTracker::new()),
                nzb_data: None,
                direct_unpacker: None,
                hopeless_tracker: None,
                repair_fold: None,
                download_time_secs: None,
                prior_download_secs: 0.0,
                carried_bytes: 0,
                failure_code: None,
                par2_volumes: None,
                output_dir_claimed: false,
                persisted_status: None,
            },
        );
        manager.job_order.lock().push(id);
    }

    #[tokio::test]
    async fn lowering_max_active_downloads_requeues_the_lowest_priority_job() {
        let (manager, tempdir) = manager();
        manager.set_max_active_downloads(2);

        let mut first = job("keep", JobStatus::Downloading, tempdir.path());
        first.priority = Priority::High;
        first.downloaded_bytes = 40;
        first.articles_downloaded = 4;
        let mut second = job("drop", JobStatus::Downloading, tempdir.path());
        second.priority = Priority::Low;
        second.downloaded_bytes = 70;
        second.articles_downloaded = 7;
        insert_job(&manager, first);
        insert_job(&manager, second);

        manager.set_max_active_downloads(1);

        let kept = manager.get_job("keep").expect("kept job");
        let dropped = manager.get_job("drop").expect("requeued job");
        assert_eq!(kept.status, JobStatus::Downloading);
        assert_eq!(dropped.status, JobStatus::Queued);
        // Requeue must not discard progress already on disk.
        assert_eq!(dropped.downloaded_bytes, 70);
        assert_eq!(dropped.articles_downloaded, 7);
        assert_eq!(
            manager
                .jobs
                .lock()
                .values()
                .filter(|state| state.job.status == JobStatus::Downloading)
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn lowering_max_active_downloads_requeues_the_newest_when_priority_ties() {
        let (manager, tempdir) = manager();
        let mut older = job("older", JobStatus::Downloading, tempdir.path());
        older.downloaded_bytes = 10;
        let mut newer = job("newer", JobStatus::Downloading, tempdir.path());
        newer.downloaded_bytes = 20;
        insert_job(&manager, older);
        insert_job(&manager, newer);

        manager.set_max_active_downloads(1);

        assert_eq!(
            manager.get_job("older").unwrap().status,
            JobStatus::Downloading
        );
        let requeued = manager.get_job("newer").unwrap();
        assert_eq!(requeued.status, JobStatus::Queued);
        assert_eq!(requeued.downloaded_bytes, 20);
    }

    #[tokio::test]
    async fn rename_job_sanitizes_instead_of_rejecting() {
        let (manager, tempdir) = manager();
        insert_job(&manager, job("renamed", JobStatus::Queued, tempdir.path()));

        for (input, expected) in [
            ("Star Trek: Discovery", "Star Trek - Discovery"),
            ("a:b*c?d", "a-b_c_d"),
            ("..", "unnamed"),
            ("../escape", "_escape"),
            ("Trailing.Dot.", "Trailing.Dot"),
            ("CON", "_CON"),
        ] {
            manager.rename_job("renamed", input).expect(input);
            let job = manager.get_job("renamed").unwrap();
            assert_eq!(job.name, expected, "{input}");
            assert_eq!(
                job.output_dir,
                tempdir
                    .path()
                    .join("complete")
                    .join("Default")
                    .join(expected),
                "{input}"
            );
        }
    }

    #[tokio::test]
    async fn remaining_percentage_sort_is_stable_and_does_not_change_status() {
        let (manager, tempdir) = manager();
        let mut first = job("first", JobStatus::Downloading, tempdir.path());
        first.total_bytes = 100;
        first.downloaded_bytes = 50;
        let mut second = job("second", JobStatus::Queued, tempdir.path());
        second.total_bytes = 200;
        second.downloaded_bytes = 100;
        let mut third = job("third", JobStatus::Queued, tempdir.path());
        third.total_bytes = 100;
        third.downloaded_bytes = 10;
        insert_job(&manager, first);
        insert_job(&manager, second);
        insert_job(&manager, third);

        manager.sort_by_remaining_percentage(true);
        assert_eq!(
            manager
                .job_order
                .lock()
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            vec!["first", "second", "third"]
        );
        assert_eq!(
            manager.get_job("first").unwrap().status,
            JobStatus::Downloading
        );

        manager.sort_by_remaining_percentage(false);
        assert_eq!(
            manager
                .job_order
                .lock()
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            vec!["third", "first", "second"]
        );
    }

    fn order_of(manager: &QueueManager) -> Vec<String> {
        manager.job_order.lock().clone()
    }

    fn sort_fixture(manager: &QueueManager, root: &std::path::Path) {
        let base = Utc::now();
        // (id, name, total, downloaded, priority, age in minutes, status)
        for (id, name, total, downloaded, priority, age_min, status) in [
            (
                "a",
                "charlie",
                300,
                0,
                Priority::Normal,
                10,
                JobStatus::Downloading,
            ),
            ("b", "Alpha", 100, 0, Priority::High, 30, JobStatus::Queued),
            ("c", "bravo", 200, 0, Priority::Low, 20, JobStatus::Queued),
        ] {
            let mut j = job(id, status, root);
            j.name = name.to_string();
            j.total_bytes = total;
            j.downloaded_bytes = downloaded;
            j.priority = priority;
            j.added_at = base - chrono::Duration::minutes(age_min);
            insert_job(manager, j);
        }
    }

    #[tokio::test]
    async fn queue_sort_by_name_is_case_insensitive() {
        let (manager, tempdir) = manager();
        sort_fixture(&manager, tempdir.path());
        manager.sort_queue(QueueSortField::Name, true);
        assert_eq!(order_of(&manager), vec!["b", "c", "a"]);
        manager.sort_queue(QueueSortField::Name, false);
        assert_eq!(order_of(&manager), vec!["a", "c", "b"]);
        // Sorting reorders only: the active job keeps downloading.
        assert_eq!(manager.get_job("a").unwrap().status, JobStatus::Downloading);
    }

    #[tokio::test]
    async fn queue_sort_by_size_age_and_priority() {
        let (manager, tempdir) = manager();
        sort_fixture(&manager, tempdir.path());
        manager.sort_queue(QueueSortField::Size, true);
        assert_eq!(order_of(&manager), vec!["b", "c", "a"]);
        manager.sort_queue(QueueSortField::Size, false);
        assert_eq!(order_of(&manager), vec!["a", "c", "b"]);
        // Age ascending is youngest first, like SABnzbd's avg_age.
        manager.sort_queue(QueueSortField::Age, true);
        assert_eq!(order_of(&manager), vec!["a", "c", "b"]);
        manager.sort_queue(QueueSortField::Age, false);
        assert_eq!(order_of(&manager), vec!["b", "c", "a"]);
        manager.sort_queue(QueueSortField::Priority, true);
        assert_eq!(order_of(&manager), vec!["c", "a", "b"]);
        manager.sort_queue(QueueSortField::Priority, false);
        assert_eq!(order_of(&manager), vec!["b", "a", "c"]);
    }

    #[tokio::test]
    async fn queue_sort_is_stable_for_equal_keys() {
        let (manager, tempdir) = manager();
        for id in ["x", "y", "z"] {
            let mut j = job(id, JobStatus::Queued, tempdir.path());
            j.name = "Same".into();
            insert_job(&manager, j);
        }
        manager.sort_queue(QueueSortField::Name, true);
        assert_eq!(order_of(&manager), vec!["x", "y", "z"]);
        manager.sort_queue(QueueSortField::Name, false);
        assert_eq!(order_of(&manager), vec!["x", "y", "z"]);
    }

    #[test]
    fn queue_sort_field_parses_known_names_only() {
        for (input, expected) in [
            ("remaining", QueueSortField::Remaining),
            ("NAME", QueueSortField::Name),
            ("size", QueueSortField::Size),
            ("bytes", QueueSortField::Size),
            ("age", QueueSortField::Age),
            ("avg_age", QueueSortField::Age),
            ("priority", QueueSortField::Priority),
        ] {
            assert_eq!(input.parse::<QueueSortField>(), Ok(expected), "{input}");
        }
        assert!("bogus".parse::<QueueSortField>().is_err());
        assert!("".parse::<QueueSortField>().is_err());
    }

    #[tokio::test]
    async fn speed_limit_above_u32_saturates_instead_of_wrapping() {
        let (manager, _tempdir) = manager();
        // 4 GiB + 1 B/s used to wrap to 1 B/s via `as u32`.
        manager.set_speed_limit(u32::MAX as u64 + 2);
        assert_eq!(manager.get_speed_limit(), u32::MAX as u64);
        manager.set_speed_limit(1_000);
        assert_eq!(manager.get_speed_limit(), 1_000);
        manager.set_speed_limit(0);
        assert_eq!(manager.get_speed_limit(), 0);
    }

    #[tokio::test]
    async fn retry_forgets_servers_that_failed_missing_articles() {
        let (manager, _tempdir) = manager();
        let nzb = br#"<?xml version="1.0" encoding="utf-8"?>
<nzb xmlns="http://www.newzbin.com/DTD/2003/nzb">
  <file poster="p" date="0" subject="&quot;retry.bin&quot; yEnc (1/2)">
    <groups><group>alt.binaries.test</group></groups>
    <segments>
      <segment bytes="5" number="1">retry-1@test</segment>
      <segment bytes="6" number="2">retry-2@test</segment>
    </segments>
  </file>
</nzb>"#;
        let work_dir = manager.incomplete_dir().join("retry-partial");
        std::fs::create_dir_all(&work_dir).unwrap();
        let parsed = nzb_parser::parse_nzb("retry", nzb).unwrap();
        let filename = parsed.files[0].filename.clone();
        let checkpoint = serde_json::json!({
            "files": {},
            "downloaded_bytes": 5,
            "articles_downloaded": 1,
            "articles_failed": 1,
            "files_completed": 0,
            "articles": {filename: [
                {"message_id": "retry-1@test", "segment_number": 1, "bytes": 5,
                 "downloaded": true, "data_begin": null, "data_size": null,
                 "crc32": null, "tried_servers": [], "tries": 0},
                {"message_id": "retry-2@test", "segment_number": 2, "bytes": 6,
                 "downloaded": false, "data_begin": null, "data_size": null,
                 "crc32": null, "tried_servers": ["srv"], "tries": 1}
            ]},
            "work_dir": work_dir,
        });
        let entry = HistoryEntry {
            id: "retry".into(),
            name: "retry".into(),
            category: "Default".into(),
            status: JobStatus::Failed,
            total_bytes: 11,
            downloaded_bytes: 5,
            added_at: Utc::now(),
            completed_at: Utc::now(),
            download_time_secs: None,
            output_dir: manager.complete_dir().join("retry"),
            stages: Vec::new(),
            error_message: None,
            failure_code: None,
            post_processing: None,
            delete_archives: None,
            server_stats: Vec::new(),
            nzb_data: None,
            retry_data: None,
            par2_volumes_fetched: None,
            par2_volumes_available: None,
        };
        let retry_data = serde_json::to_vec(&checkpoint).unwrap();

        let job = manager
            .prepare_retry_job(&entry, nzb, Some(&retry_data))
            .unwrap();

        let articles = &job.files[0].articles;
        assert!(articles[0].downloaded);
        assert!(!articles[1].downloaded);
        // A missing article keeping its old `tried_servers` would be treated
        // as an already-resolved failure and never re-attempted.
        assert!(articles[1].tried_servers.is_empty());
        assert_eq!(job.articles_failed, 0);
    }

    #[test]
    fn script_paths_are_confined_to_the_script_directory() {
        let dir = tempfile::tempdir().unwrap();
        let scripts = dir.path().join("scripts");
        std::fs::create_dir_all(&scripts).unwrap();
        let script = scripts.join("success.sh");
        std::fs::write(&script, b"#!/bin/sh\nexit 0\n").unwrap();

        let resolved =
            resolve_script_path(Some(&scripts), std::path::Path::new("success.sh")).unwrap();
        assert_eq!(resolved, std::fs::canonicalize(script).unwrap());
        assert!(resolve_script_path(Some(&scripts), std::path::Path::new("../escape.sh")).is_err());
        assert!(resolve_script_path(None, std::path::Path::new("success.sh")).is_err());
    }

    #[tokio::test]
    async fn script_output_is_bounded_and_captured() {
        let (manager, tempdir) = manager();
        let script = tempdir.path().join("script.sh");
        std::fs::write(&script, b"#!/bin/sh\nprintf '1234567890'\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        manager.set_postproc_scripts(None, Some(script), None, 2, 4);
        let mut completed = job("script-job", JobStatus::Completed, tempdir.path());
        completed.output_dir = tempdir.path().join("output");
        std::fs::create_dir_all(&completed.output_dir).unwrap();
        insert_job(&manager, completed);

        let stage = manager
            .run_postproc_script("script-job", JobStatus::Completed, &[])
            .await
            .unwrap();
        assert_eq!(stage.status, StageStatus::Success);
        assert!(stage.message.unwrap().contains("output truncated"));
    }

    /// The script gets SABnzbd's positional arguments and the `NZBPP_*`
    /// environment next to the `SAB_*` names it always had.
    #[tokio::test]
    async fn postproc_script_receives_sab_arguments_and_nzbpp_env() {
        let (manager, tempdir) = manager();
        let record = tempdir.path().join("record.txt");
        let script = tempdir.path().join("record.sh");
        let body = r#"#!/bin/sh
{ for a in "$@"; do printf 'arg:%s\n' "$a"; done; env | grep -E '^(NZBPP|SAB)_' | sort; } > RECORD_PATH
"#
        .replace("RECORD_PATH", &record.to_string_lossy());
        std::fs::write(&script, body).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        manager.set_postproc_scripts(None, Some(script), None, 5, 4096);
        let mut completed = job("script-job", JobStatus::Completed, tempdir.path());
        completed.category = "tv".into();
        completed.output_dir = tempdir.path().join("output");
        std::fs::create_dir_all(&completed.output_dir).unwrap();
        insert_job(&manager, completed.clone());

        let stage = manager
            .run_postproc_script("script-job", JobStatus::Completed, &[])
            .await
            .unwrap();
        assert_eq!(stage.status, StageStatus::Success, "{stage:?}");

        let recorded = std::fs::read_to_string(&record).unwrap();
        let lines: Vec<&str> = recorded.lines().collect();
        let output = completed.output_dir.to_string_lossy().into_owned();
        assert_eq!(
            lines[..8],
            [
                format!("arg:{output}"),
                "arg:script-job".to_string(),
                "arg:script-job".to_string(),
                "arg:".to_string(),
                "arg:tv".to_string(),
                "arg:".to_string(),
                "arg:0".to_string(),
                "arg:".to_string(),
            ],
            "{recorded}"
        );
        for expected in [
            "NZBPP_CATEGORY=tv".to_string(),
            format!("NZBPP_DIRECTORY={output}"),
            "NZBPP_NZBID=script-job".to_string(),
            "NZBPP_NZBNAME=script-job".to_string(),
            "NZBPP_PARSTATUS=0".to_string(),
            "NZBPP_STATUS=SUCCESS/ALL".to_string(),
            "NZBPP_UNPACKSTATUS=0".to_string(),
            "SAB_CAT=tv".to_string(),
            "SAB_STATUS=Completed".to_string(),
        ] {
            assert!(
                lines.contains(&expected.as_str()),
                "missing {expected}: {recorded}"
            );
        }
    }

    /// A failed job's script sees the failure in both contracts: SABnzbd's
    /// `SAB_FAIL_MSG`/`SAB_STATUS` and NZBGet's `NZBPP_TOTALSTATUS` and
    /// `<TOTAL>/<detail>` status.
    #[tokio::test]
    async fn postproc_script_env_reports_a_failure_with_a_repair_detail() {
        let (manager, tempdir) = manager();
        let record = tempdir.path().join("record.txt");
        let script = tempdir.path().join("record.sh");
        let body = format!(
            "#!/bin/sh\nenv | grep -E '^(NZBPP|SAB)_' | sort > {}\n",
            record.display()
        );
        std::fs::write(&script, body).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        manager.set_postproc_scripts(None, None, Some(script), 5, 4096);
        let mut broken = job("broken-job", JobStatus::Failed, tempdir.path());
        broken.error_message = Some("3 blocks could not be repaired".into());
        broken.output_dir = tempdir.path().join("output");
        std::fs::create_dir_all(&broken.output_dir).unwrap();
        insert_job(&manager, broken);

        let stages = [StageResult {
            name: "Repair".into(),
            status: StageStatus::Failed,
            message: Some("not enough recovery blocks".into()),
            duration_secs: 0.0,
        }];
        let stage = manager
            .run_postproc_script("broken-job", JobStatus::Failed, &stages)
            .await
            .unwrap();
        assert_eq!(stage.status, StageStatus::Success, "{stage:?}");

        let recorded = std::fs::read_to_string(&record).unwrap();
        for expected in [
            "SAB_STATUS=Failed",
            "SAB_FAIL_MSG=3 blocks could not be repaired",
            "SAB_PP_STATUS=1",
            "NZBPP_TOTALSTATUS=FAILURE",
            "NZBPP_STATUS=FAILURE/PAR",
            "NZBPP_PARSTATUS=1",
            "NZBPP_UNPACKSTATUS=0",
        ] {
            assert!(
                recorded.lines().any(|line| line == expected),
                "missing {expected}: {recorded}"
            );
        }
    }

    /// A successful unpack reports success in NZBGet's numeric codes, and a
    /// failed extraction is `FAILURE/UNPACK` rather than a repair failure.
    #[test]
    fn nzbpp_codes_describe_the_stage_that_ran() {
        let stage = |name: &str, status: StageStatus| StageResult {
            name: name.into(),
            status,
            message: None,
            duration_secs: 0.0,
        };
        let unpacked = [
            stage("Verify", StageStatus::Success),
            stage("Extract", StageStatus::Success),
        ];
        assert_eq!(nzbpp_par_status(&unpacked), 2);
        assert_eq!(nzbpp_unpack_status(&unpacked), 2);
        assert_eq!(nzbpp_status("SUCCESS", &unpacked, true), "SUCCESS/ALL");

        let failed_unpack = [
            stage("Verify", StageStatus::Success),
            stage("Extract", StageStatus::Failed),
        ];
        assert_eq!(nzbpp_par_status(&failed_unpack), 2);
        assert_eq!(nzbpp_unpack_status(&failed_unpack), 1);
        assert_eq!(
            nzbpp_status("FAILURE", &failed_unpack, false),
            "FAILURE/UNPACK"
        );

        // No stages means nothing ran, not a successful check.
        assert_eq!(nzbpp_par_status(&[]), 0);
        assert_eq!(nzbpp_unpack_status(&[]), 0);
        // A skipped verify is not a check; the job reported no PAR2 work.
        let skipped = [stage("Verify", StageStatus::Skipped)];
        assert_eq!(nzbpp_par_status(&skipped), 0);
        assert_eq!(nzbpp_unpack_status(&skipped), 0);
    }

    /// `SAB_FILES` is SABnzbd's JSON array, relative to the complete
    /// directory, and the priority and job level use their own numbering.
    #[test]
    fn sab_files_and_priority_follow_sabnzbd() {
        let tempdir = tempfile::tempdir().unwrap();
        let output = tempdir.path().join("output");
        std::fs::create_dir_all(output.join("nested")).unwrap();
        std::fs::write(output.join("a.mkv"), b"a").unwrap();
        std::fs::write(output.join("nested/b.srt"), b"b").unwrap();

        let mut completed = job("files-job", JobStatus::Completed, tempdir.path());
        completed.output_dir = output;
        completed.priority = Priority::High;
        completed.pp_override = Some(2);

        let env = postproc_script_env(
            &completed,
            JobStatus::Completed,
            &[],
            std::path::Path::new("/scripts/post.sh"),
            2,
        );
        let value = |name: &str| {
            env.iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| value.as_str())
                .unwrap_or_else(|| panic!("missing {name}"))
        };

        let files: Vec<String> = serde_json::from_str(value("SAB_FILES")).unwrap();
        assert_eq!(files, ["a.mkv", "nested/b.srt"]);
        assert_eq!(value("SAB_PRIORITY"), "1");
        assert_eq!(value("SAB_PP"), "2");
        assert_eq!(value("SAB_REPAIR"), "0");
        assert_eq!(value("SAB_UNPACK"), "1");
        assert_eq!(value("SAB_SCRIPT"), "post.sh");

        assert_eq!(sab_priority(Priority::Low), -1);
        assert_eq!(sab_priority(Priority::Normal), 0);
        assert_eq!(sab_priority(Priority::Force), 2);
    }

    #[test]
    fn postproc_status_code_matches_sabnzbd_codes() {
        let stage = |name: &str, status: StageStatus| StageResult {
            name: name.into(),
            status,
            message: None,
            duration_secs: 0.0,
        };
        let ok = [
            stage("Verify", StageStatus::Success),
            stage("Extract", StageStatus::Success),
        ];
        assert_eq!(postproc_status_code(&ok), 0);
        let verify_failed = [stage("Repair", StageStatus::Failed)];
        assert_eq!(postproc_status_code(&verify_failed), 1);
        let unpack_failed = [stage("Extract", StageStatus::Failed)];
        assert_eq!(postproc_status_code(&unpack_failed), 2);
        let both = [
            stage("Verify", StageStatus::Failed),
            stage("Extract", StageStatus::Failed),
        ];
        assert_eq!(postproc_status_code(&both), 3);
    }

    /// A `done.sh` script that appends `ran` to a marker file, installed as
    /// the scripts directory. Returns the marker path.
    fn install_marker_script(manager: &QueueManager, root: &std::path::Path) -> std::path::PathBuf {
        let scripts = root.join("scripts");
        std::fs::create_dir_all(&scripts).unwrap();
        // The script runs from the complete directory, which startup creates.
        std::fs::create_dir_all(root.join("complete")).unwrap();
        let marker = root.join("drained.txt");
        let script = scripts.join("done.sh");
        std::fs::write(
            &script,
            format!("#!/bin/sh\necho ran >> '{}'\n", marker.display()),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        manager.set_postproc_scripts(Some(scripts), None, None, 5, 4096);
        manager
            .set_queue_complete_action(Some("script_done.sh"))
            .unwrap();
        marker
    }

    /// `change_complete_action` runs its script when the last active job
    /// finishes, not while another job is still active, and then resets.
    #[tokio::test]
    async fn queue_complete_action_runs_script_once_queue_drains() {
        let (manager, tempdir) = manager();
        let marker = install_marker_script(&manager, tempdir.path());
        assert!(
            manager
                .set_queue_complete_action(Some("script_missing.sh"))
                .is_err()
        );
        assert!(
            manager
                .set_queue_complete_action(Some("shutdown_pc"))
                .is_err()
        );
        assert_eq!(
            manager.queue_complete_action().as_deref(),
            Some("script_done.sh")
        );

        insert_job(&manager, job("first", JobStatus::Completed, tempdir.path()));
        insert_job(
            &manager,
            job("second", JobStatus::Downloading, tempdir.path()),
        );
        assert!(!queue_drained_by(&manager.jobs.lock(), "first"));
        manager.jobs.lock().get_mut("second").unwrap().job.status = JobStatus::Completed;
        assert!(queue_drained_by(&manager.jobs.lock(), "second"));

        manager.run_queue_complete_action().await;
        assert_eq!(std::fs::read_to_string(&marker).unwrap(), "ran\n");
        // Not persistent (`queue_complete_pers` off), so the action is reset.
        assert_eq!(manager.queue_complete_action(), None);
        assert_eq!(
            manager
                .db
                .lock()
                .get_setting(QueueManager::QUEUE_COMPLETE_ACTION_SETTING),
            Some(String::new())
        );
    }

    /// `par2.trust_inflight_verify` is on by default and the setter turns it off.
    #[tokio::test]
    async fn trust_inflight_verify_defaults_on_and_follows_config() {
        let (manager, _tempdir) = manager();
        assert!(manager.trust_inflight_verify.load(Ordering::SeqCst));
        manager.set_trust_inflight_verify(false);
        assert!(!manager.trust_inflight_verify.load(Ordering::SeqCst));
    }

    /// With `queue_complete_pers` the action survives its run.
    #[tokio::test]
    async fn queue_complete_action_persists_when_pers_is_set() {
        let (manager, tempdir) = manager();
        let marker = install_marker_script(&manager, tempdir.path());
        manager.set_queue_complete_persist(true);

        manager.run_queue_complete_action().await;
        manager.run_queue_complete_action().await;
        assert_eq!(std::fs::read_to_string(&marker).unwrap(), "ran\nran\n");
        assert_eq!(
            manager.queue_complete_action().as_deref(),
            Some("script_done.sh")
        );
    }

    /// Removing the last active job drains the queue and runs the action.
    #[tokio::test]
    async fn removing_last_active_job_runs_queue_complete_action() {
        let (manager, tempdir) = manager();
        let marker = install_marker_script(&manager, tempdir.path());
        insert_job(
            &manager,
            job("only", JobStatus::Downloading, tempdir.path()),
        );

        manager.remove_job("only").unwrap();
        // The removal spawns the action; wait for its script to finish.
        for _ in 0..100 {
            if marker.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(std::fs::read_to_string(&marker).unwrap(), "ran\n");
    }

    /// A job that hopelessly aborts and leaves the queue empty runs the action.
    #[tokio::test]
    async fn hopeless_abort_of_last_job_runs_queue_complete_action() {
        let (manager, tempdir) = manager();
        let marker = install_marker_script(&manager, tempdir.path());
        insert_job(
            &manager,
            job("doomed", JobStatus::Downloading, tempdir.path()),
        );
        let (progress_tx, progress_rx) = mpsc::channel(4);
        let handler = tokio::spawn(Arc::clone(&manager).handle_progress(
            "doomed".to_string(),
            progress_rx,
            Arc::new(SpeedTracker::new()),
        ));

        progress_tx
            .send(ProgressUpdate::JobAborted {
                job_id: "doomed".to_string(),
                reason: "all articles unavailable".to_string(),
                articles_failed: 1,
                download_time_secs: 0.5,
            })
            .await
            .unwrap();
        drop(progress_tx);
        handler.await.unwrap();

        assert_eq!(manager.get_job("doomed").unwrap().status, JobStatus::Failed);
        assert_eq!(std::fs::read_to_string(&marker).unwrap(), "ran\n");
    }

    /// A job that fails the post-processing disk-space guard runs the action
    /// once it is the last job in the queue.
    #[tokio::test]
    async fn disk_space_failure_of_last_job_runs_queue_complete_action() {
        let (manager, tempdir) = manager();
        let marker = install_marker_script(&manager, tempdir.path());
        insert_job(
            &manager,
            job("full-disk", JobStatus::Downloading, tempdir.path()),
        );
        manager.set_min_free_space(u64::MAX);

        manager.on_job_finished("full-disk", true, 0).await;

        assert_eq!(std::fs::read_to_string(&marker).unwrap(), "ran\n");
    }

    #[tokio::test]
    async fn active_queue_view_excludes_terminal_jobs() {
        let (manager, tempdir) = manager();
        insert_job(
            &manager,
            job("downloading", JobStatus::Downloading, tempdir.path()),
        );
        insert_job(
            &manager,
            job("completed", JobStatus::Completed, tempdir.path()),
        );
        insert_job(&manager, job("failed", JobStatus::Failed, tempdir.path()));

        assert_eq!(manager.get_jobs().len(), 3);
        assert_eq!(manager.get_active_jobs().len(), 1);
        assert_eq!(manager.get_active_jobs()[0].id, "downloading");
        assert_eq!(manager.queue_size(), 1);
    }

    #[tokio::test]
    async fn shutdown_flushes_article_checkpoint_for_active_downloads() {
        let (manager, tempdir) = manager();
        let mut jb = job("drain-checkpoint", JobStatus::Downloading, tempdir.path());
        jb.downloaded_bytes = 5;
        jb.articles_downloaded = 1;
        jb.file_count = 1;
        jb.article_count = 1;
        jb.files.push(NzbFile {
            id: "f1".to_string(),
            filename: "file.bin".to_string(),
            bytes: 5,
            bytes_downloaded: 5,
            is_par2: false,
            par2_setname: None,
            par2_vol: None,
            par2_blocks: None,
            assembled: false,
            groups: Vec::new(),
            articles: vec![Article {
                message_id: "<t@x>".to_string(),
                segment_number: 1,
                bytes: 5,
                downloaded: true,
                data_begin: None,
                data_size: None,
                crc32: None,
                tried_servers: Vec::new(),
                tries: 0,
            }],
        });
        insert_job(&manager, jb);

        manager.shutdown().await;

        assert_eq!(
            manager.get_job("drain-checkpoint").unwrap().status,
            JobStatus::Queued
        );
        let data = manager
            .db
            .lock()
            .queue_load_job_data("drain-checkpoint")
            .expect("checkpoint persisted");
        let checkpoint: JobCheckpoint =
            serde_json::from_slice(data.as_deref().expect("checkpoint bytes"))
                .expect("valid checkpoint");
        assert_eq!(checkpoint.articles_downloaded, 1);
        assert_eq!(checkpoint.downloaded_bytes, 5);
        assert_eq!(checkpoint.files.get("file.bin"), Some(&vec![1u32]));
    }

    #[tokio::test]
    async fn global_pause_blocks_job_resume_and_preserves_manual_pause() {
        let (manager, tempdir) = manager();
        insert_job(
            &manager,
            job("active", JobStatus::Downloading, tempdir.path()),
        );
        insert_job(&manager, job("queued", JobStatus::Queued, tempdir.path()));
        insert_job(&manager, job("manual", JobStatus::Paused, tempdir.path()));

        manager.pause_all();

        assert!(manager.is_paused());
        assert_eq!(manager.get_job("active").unwrap().status, JobStatus::Paused);
        assert_eq!(manager.get_job("queued").unwrap().status, JobStatus::Paused);
        assert_eq!(manager.get_job("manual").unwrap().status, JobStatus::Paused);
        assert!(manager.globally_paused_jobs.lock().contains("active"));
        assert!(manager.globally_paused_jobs.lock().contains("queued"));
        assert!(!manager.globally_paused_jobs.lock().contains("manual"));

        let error = manager.resume_job("active").unwrap_err();
        assert!(error.to_string().contains("globally paused"));
        assert_eq!(manager.get_job("active").unwrap().status, JobStatus::Paused);

        manager
            .jobs
            .lock()
            .get_mut("active")
            .unwrap()
            .job
            .error_message = Some("server unavailable".to_string());
        manager.resume_server_paused_jobs();
        assert_eq!(manager.get_job("active").unwrap().status, JobStatus::Paused);

        manager.resume_all();

        assert!(!manager.is_paused());
        assert_eq!(manager.get_job("manual").unwrap().status, JobStatus::Paused);
    }

    #[tokio::test]
    async fn low_disk_guard_reports_disk_space_reason_and_resumes_when_space_frees() {
        let (manager, tempdir) = manager();
        insert_job(
            &manager,
            job("active", JobStatus::Downloading, tempdir.path()),
        );
        insert_job(&manager, job("manual", JobStatus::Paused, tempdir.path()));
        assert_eq!(manager.pause_reason("active"), None);
        assert_eq!(manager.pause_reason("manual"), Some(PauseReason::Manual));

        // No volume can ever have u64::MAX bytes free.
        manager.set_min_free_space(u64::MAX);
        manager.enforce_disk_guard();

        assert!(manager.is_paused());
        assert_eq!(manager.global_pause_reason(), Some(PauseReason::DiskSpace));
        assert!(manager.disk_space_hold_since().is_some());
        assert_eq!(manager.pause_reason("active"), Some(PauseReason::DiskSpace));
        assert_eq!(manager.pause_reason("manual"), Some(PauseReason::Manual));
        assert!(
            manager
                .db
                .lock()
                .get_setting(QueueManager::DISK_SPACE_HOLD_SETTING)
                .is_some_and(|value| !value.is_empty())
        );

        // Space frees up: the guard lifts its own hold on the next check.
        manager.set_min_free_space(1);
        manager.enforce_disk_guard();

        assert!(!manager.is_paused());
        assert_eq!(manager.global_pause_reason(), None);
        assert!(manager.disk_space_hold_since().is_none());
        assert_ne!(manager.get_job("active").unwrap().status, JobStatus::Paused);
        assert_eq!(manager.get_job("manual").unwrap().status, JobStatus::Paused);
        assert_eq!(manager.pause_reason("manual"), Some(PauseReason::Manual));
    }

    #[tokio::test]
    async fn disk_guard_never_lifts_a_user_global_pause() {
        let (manager, tempdir) = manager();
        insert_job(&manager, job("queued", JobStatus::Queued, tempdir.path()));

        manager.pause_all();
        assert_eq!(manager.global_pause_reason(), Some(PauseReason::Global));
        assert_eq!(manager.pause_reason("queued"), Some(PauseReason::Global));
        manager.set_min_free_space(1);
        manager.enforce_disk_guard();
        assert!(manager.is_paused());

        // A user Pause All during a disk hold takes ownership of the pause.
        manager.resume_all();
        manager.set_min_free_space(u64::MAX);
        manager.enforce_disk_guard();
        assert_eq!(manager.global_pause_reason(), Some(PauseReason::DiskSpace));
        manager.pause_all();
        assert_eq!(manager.global_pause_reason(), Some(PauseReason::Global));
        manager.set_min_free_space(1);
        manager.enforce_disk_guard();
        assert!(manager.is_paused());
    }

    #[tokio::test]
    async fn schedule_pause_ownership_survives_a_restart() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let manager = file_manager(tempdir.path());
        insert_job(&manager, job("queued", JobStatus::Queued, tempdir.path()));

        let decision = crate::nzb_core::schedule::ScheduleDecision {
            active_rule_ids: vec!["quiet".into()],
            pause: true,
            ..Default::default()
        };
        manager.apply_schedule(&decision, true);
        assert!(manager.is_paused());
        assert!(manager.schedule_pause_active());
        assert_eq!(manager.global_pause_reason(), Some(PauseReason::Schedule));
        assert_eq!(
            manager
                .db
                .lock()
                .get_setting("global_pause_owner")
                .as_deref(),
            Some("schedule")
        );

        // A restart rebuilds the pause from the database. The schedule must
        // still own it, or the window ending could never resume the queue.
        drop(manager);
        let restarted = file_manager(tempdir.path());
        restarted.restore_from_db().unwrap();
        assert!(restarted.is_paused());
        assert!(restarted.schedule_pause_active());
        assert_eq!(restarted.global_pause_reason(), Some(PauseReason::Schedule));

        // A user Pause All during the window takes ownership, so the window
        // ending leaves their pause alone.
        restarted.pause_all();
        assert!(!restarted.schedule_pause_active());
        assert_eq!(restarted.global_pause_reason(), Some(PauseReason::Global));
        // Close and reopen the window: a user pause is not the schedule's to
        // lift, so reopening must not replace the user's ownership with its
        // own.
        restarted.apply_schedule(
            &crate::nzb_core::schedule::ScheduleDecision::default(),
            true,
        );
        restarted.apply_schedule(&decision, true);
        assert!(
            restarted.is_paused(),
            "a user pause is not the schedule's to lift"
        );
        assert_eq!(
            restarted
                .db
                .lock()
                .get_setting("global_pause_owner")
                .as_deref(),
            Some("user")
        );
    }

    #[tokio::test]
    async fn a_manual_resume_inside_a_window_holds_until_the_window_closes() {
        let (manager, tempdir) = manager();
        insert_job(&manager, job("queued", JobStatus::Queued, tempdir.path()));

        let open = crate::nzb_core::schedule::ScheduleDecision {
            active_rule_ids: vec!["quiet".into()],
            pause: true,
            ..Default::default()
        };
        manager.apply_schedule(&open, true);
        assert!(manager.is_paused());
        assert_eq!(manager.global_pause_reason(), Some(PauseReason::Schedule));

        // The user overrides the schedule for this window.
        manager.resume_all();
        assert!(!manager.is_paused());
        assert!(!manager.schedule_pause_active());

        // Every subsequent tick inside the window sees the same decision. The
        // pause is edge-triggered, so none of them may re-pause the queue.
        for _ in 0..5 {
            manager.apply_schedule(&open, true);
            assert!(
                !manager.is_paused(),
                "a manual resume must survive the rest of the window"
            );
        }

        // When the window closes the schedule has nothing to lift, and the
        // next window opening pauses again.
        manager.apply_schedule(
            &crate::nzb_core::schedule::ScheduleDecision::default(),
            true,
        );
        assert!(!manager.is_paused());
        manager.apply_schedule(&open, true);
        assert!(manager.is_paused());
        assert_eq!(manager.global_pause_reason(), Some(PauseReason::Schedule));
    }

    #[tokio::test]
    async fn a_window_opening_while_the_user_is_paused_leaves_the_user_owner() {
        let (manager, tempdir) = manager();
        insert_job(&manager, job("queued", JobStatus::Queued, tempdir.path()));

        manager.pause_all();
        let open = crate::nzb_core::schedule::ScheduleDecision {
            pause: true,
            ..Default::default()
        };
        manager.apply_schedule(&open, true);
        assert_eq!(manager.global_pause_reason(), Some(PauseReason::Global));
        assert!(!manager.schedule_pause_active());

        // The window ending must not resume a pause the user owns.
        manager.apply_schedule(
            &crate::nzb_core::schedule::ScheduleDecision::default(),
            true,
        );
        assert!(
            manager.is_paused(),
            "a user pause outlives the window that opened during it"
        );
        assert_eq!(manager.global_pause_reason(), Some(PauseReason::Global));
    }

    #[tokio::test]
    async fn a_restart_inside_a_window_still_lifts_the_pause_when_it_ends() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let manager = file_manager(tempdir.path());
        let open = crate::nzb_core::schedule::ScheduleDecision {
            pause: true,
            ..Default::default()
        };
        manager.apply_schedule(&open, true);
        assert!(manager.schedule_pause_active());

        // A restart restores the schedule-owned pause from the database. The
        // window must be rebuilt with it, or the end of the window would be
        // read as "no change" and the queue would stay paused forever.
        drop(manager);
        let restarted = file_manager(tempdir.path());
        restarted.restore_from_db().unwrap();
        assert!(restarted.schedule_pause_active());

        restarted.apply_schedule(
            &crate::nzb_core::schedule::ScheduleDecision::default(),
            true,
        );
        assert!(
            !restarted.is_paused(),
            "the window ending after a restart still resumes the queue"
        );
        assert!(!restarted.schedule_pause_active());
    }

    #[tokio::test]
    async fn a_window_opening_during_a_disk_hold_keeps_pausing_after_the_hold_lifts() {
        let (manager, tempdir) = manager();
        insert_job(&manager, job("queued", JobStatus::Queued, tempdir.path()));

        // The guard trips first, so its hold is the only one there is.
        manager.set_min_free_space(u64::MAX);
        manager.enforce_disk_guard();
        assert_eq!(manager.global_pause_reason(), Some(PauseReason::DiskSpace));

        // A pause window then opens. It must be recorded even though the
        // queue is already paused, or it would never take effect.
        let open = crate::nzb_core::schedule::ScheduleDecision {
            pause: true,
            pause_windows: vec!["quiet@2026-10-20".into()],
            ..Default::default()
        };
        manager.apply_schedule(&open, true);
        assert_eq!(
            manager.global_pause_reason(),
            Some(PauseReason::DiskSpace),
            "the disk hold is still the reason while the window overlaps it"
        );

        // Space frees up. The guard lifts only its own hold; the window is
        // still open, so downloads must not run straight through it.
        manager.set_min_free_space(1);
        manager.enforce_disk_guard();
        assert!(
            manager.is_paused(),
            "the queue ran through a pause window that opened during a disk hold"
        );
        assert!(manager.schedule_pause_active());
        assert_eq!(manager.global_pause_reason(), Some(PauseReason::Schedule));

        // And the window ending resumes, as usual.
        manager.apply_schedule(
            &crate::nzb_core::schedule::ScheduleDecision::default(),
            true,
        );
        assert!(!manager.is_paused());
        assert!(!manager.schedule_pause_active());
    }

    #[tokio::test]
    async fn a_manual_resume_inside_a_window_survives_a_restart() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let manager = file_manager(tempdir.path());
        insert_job(&manager, job("queued", JobStatus::Queued, tempdir.path()));

        let open = crate::nzb_core::schedule::ScheduleDecision {
            pause: true,
            pause_windows: vec!["quiet@2026-10-20".into()],
            ..Default::default()
        };
        manager.apply_schedule(&open, true);
        assert!(manager.is_paused());

        // The user overrides the schedule for this window, then the process
        // restarts while the window is still open.
        manager.resume_all();
        assert!(!manager.is_paused());
        drop(manager);
        let restarted = file_manager(tempdir.path());
        restarted.restore_from_db().unwrap();
        assert!(!restarted.is_paused());

        // The monitor's first tick after the restart re-applies the same open
        // window. It must not re-pause what the user just resumed.
        restarted.apply_schedule(&open, true);
        assert!(
            !restarted.is_paused(),
            "a restart inside a window undid a manual resume"
        );

        // Closing the window changes nothing, and the next window pauses.
        restarted.apply_schedule(
            &crate::nzb_core::schedule::ScheduleDecision::default(),
            true,
        );
        assert!(!restarted.is_paused());
        let next = crate::nzb_core::schedule::ScheduleDecision {
            pause_windows: vec!["quiet@2026-10-21".into()],
            ..open.clone()
        };
        restarted.apply_schedule(&next, true);
        assert!(
            restarted.is_paused(),
            "the next window must pause again after a resume"
        );
    }

    #[tokio::test]
    async fn a_restart_during_a_disk_hold_keeps_the_overlapping_window() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let manager = file_manager(tempdir.path());
        insert_job(&manager, job("queued", JobStatus::Queued, tempdir.path()));

        // The disk guard holds the queue, then a pause window opens on top of
        // it. Both holds are live at once, so both must be persisted.
        manager.set_min_free_space(u64::MAX);
        manager.enforce_disk_guard();
        assert_eq!(manager.global_pause_reason(), Some(PauseReason::DiskSpace));
        let open = crate::nzb_core::schedule::ScheduleDecision {
            active_rule_ids: vec!["quiet".into()],
            pause: true,
            pause_windows: vec!["quiet@2026-10-20".into()],
            ..Default::default()
        };
        manager.apply_schedule(&open, true);
        assert!(manager.is_paused());

        // A restart rebuilds both holds. Recording a single ranked owner lost
        // the schedule hold here, so the guard lifting its own hold later
        // resumed downloads straight through the rest of the window.
        drop(manager);
        let restarted = file_manager(tempdir.path());
        restarted.restore_from_db().unwrap();
        assert!(restarted.is_paused());
        assert_eq!(
            restarted.global_pause_reason(),
            Some(PauseReason::DiskSpace)
        );

        restarted.set_min_free_space(1);
        restarted.enforce_disk_guard();
        assert_eq!(
            restarted.global_pause_reason(),
            Some(PauseReason::Schedule),
            "a restart dropped the schedule hold overlapping the disk hold"
        );

        // The window closing is what resumes the queue.
        restarted.apply_schedule(
            &crate::nzb_core::schedule::ScheduleDecision::default(),
            true,
        );
        assert!(!restarted.is_paused());
    }

    #[tokio::test]
    async fn an_expiring_timed_pause_leaves_a_disk_hold_in_place() {
        let (manager, tempdir) = manager();
        insert_job(&manager, job("queued", JobStatus::Queued, tempdir.path()));

        manager.pause_all();
        manager.set_min_free_space(u64::MAX);
        manager.enforce_disk_guard();
        assert_eq!(manager.global_pause_reason(), Some(PauseReason::Global));

        manager.expire_user_hold();
        assert!(
            manager.is_paused(),
            "a timed pause expiring resumed with the disk still full"
        );
        assert_eq!(manager.global_pause_reason(), Some(PauseReason::DiskSpace));
    }

    #[tokio::test]
    async fn a_user_pause_outlives_the_disk_hold_it_overlapped() {
        let (manager, tempdir) = manager();
        insert_job(&manager, job("queued", JobStatus::Queued, tempdir.path()));

        // "Pause for 2h" covers the low-disk warning; space frees while the
        // user pause still runs.
        manager.set_min_free_space(u64::MAX);
        manager.enforce_disk_guard();
        manager.pause_all();
        manager.pause_for(60);
        // The guard notices space is back and lifts only its own hold.
        manager.set_min_free_space(1);
        manager.enforce_disk_guard();
        assert_eq!(manager.global_pause_reason(), Some(PauseReason::Global));

        // The user pause expiring must not bring the disk hold back or, worse,
        // resume while it should still be holding.
        manager.expire_user_hold();
        assert!(!manager.is_paused());
    }

    #[tokio::test]
    async fn a_manual_resume_is_recorded_so_the_next_tick_does_not_re_pause() {
        let (manager, tempdir) = manager();
        insert_job(&manager, job("queued", JobStatus::Queued, tempdir.path()));
        let open = decision(&["quiet@2026-10-20"]);

        manager.apply_schedule(&open, true);
        assert_eq!(manager.global_pause_reason(), Some(PauseReason::Schedule));
        manager.resume_all();
        assert!(!manager.is_paused());

        // The same occurrence on the next tick stays resumed...
        manager.apply_schedule(&open, true);
        assert!(!manager.is_paused());
        // ...while the next window pauses again.
        manager.apply_schedule(&decision(&["quiet@2026-10-21"]), true);
        assert!(manager.is_paused());
    }

    fn decision(windows: &[&str]) -> crate::nzb_core::schedule::ScheduleDecision {
        crate::nzb_core::schedule::ScheduleDecision {
            active_rule_ids: windows.iter().map(|window| window.to_string()).collect(),
            pause: !windows.is_empty(),
            pause_windows: windows.iter().map(|window| window.to_string()).collect(),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn one_of_two_overlapping_windows_closing_does_not_pause_again() {
        let (manager, tempdir) = manager();
        insert_job(&manager, job("queued", JobStatus::Queued, tempdir.path()));

        // A (20:00-24:00) and B (22:00-02:00) are open together.
        let both = decision(&["a@2026-10-19", "b@2026-10-19"]);
        manager.apply_schedule(&both, true);
        assert_eq!(manager.global_pause_reason(), Some(PauseReason::Schedule));

        // The user resumes inside the overlap.
        manager.resume_all();
        assert!(!manager.is_paused());

        // Still inside both windows: no rising edge, so no re-pause.
        manager.apply_schedule(&both, true);
        assert!(!manager.is_paused());

        // A closes at midnight while B is still open. The old key changed, so
        // a joined key looked like a new window; the occurrence that is still
        // open is the same one, so the resume must hold.
        let just_b = decision(&["b@2026-10-19"]);
        manager.apply_schedule(&just_b, true);
        assert!(
            !manager.is_paused(),
            "a window closing out of an overlap re-paused a manual resume"
        );

        // The next occurrence of B does pause again.
        manager.apply_schedule(&decision(&["b@2026-10-20"]), true);
        assert!(manager.is_paused(), "the next window must pause");
    }

    #[tokio::test(start_paused = true)]
    async fn a_second_pause_cancels_the_first_timers_auto_resume() {
        let (manager, tempdir) = manager();
        insert_job(&manager, job("queued", JobStatus::Queued, tempdir.path()));

        // The first timer expires at +60s, then a plain Pause All replaces it:
        // the queue must stay paused when that timer wakes up.
        manager.pause_for(60);
        // Let the spawned timer register its sleep before the clock moves; a
        // paused clock does not wake a task that has not been polled yet.
        tokio::task::yield_now().await;
        manager.pause_all();
        tokio::time::advance(Duration::from_secs(61)).await;
        // Let the timer task run to its comparison.
        tokio::task::yield_now().await;
        assert!(
            manager.is_paused(),
            "an earlier timed pause resumed a later plain pause"
        );
        assert_eq!(manager.pause_remaining_secs(), None);

        // A later timed pause is not cancelled by an earlier timer either: at
        // +61s a 120s pause is still running, and the first timer must not
        // resume it.
        manager.pause_for(120);
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(61)).await;
        tokio::task::yield_now().await;
        assert!(
            manager.is_paused(),
            "the earlier timer resumed a newer timed pause"
        );
        assert!(manager.pause_remaining_secs().is_some_and(|secs| secs > 0));
    }

    #[tokio::test(start_paused = true)]
    async fn a_timed_pause_expiring_pauses_into_an_open_window() {
        let (manager, tempdir) = manager();
        insert_job(&manager, job("queued", JobStatus::Queued, tempdir.path()));
        let open = decision(&["quiet@2026-10-20"]);

        manager.pause_for(60);
        tokio::task::yield_now().await;
        // The window opens while the user's timed pause still covers it.
        manager.apply_schedule(&open, true);
        assert_eq!(manager.global_pause_reason(), Some(PauseReason::Global));

        tokio::time::advance(Duration::from_secs(61)).await;
        tokio::task::yield_now().await;
        assert!(
            manager.is_paused(),
            "the timed pause resumed straight into an open window"
        );
        assert_eq!(manager.global_pause_reason(), Some(PauseReason::Schedule));
    }

    #[tokio::test]
    async fn disabling_the_schedule_inside_a_window_lifts_its_pause() {
        let (manager, tempdir) = manager();
        insert_job(&manager, job("queued", JobStatus::Queued, tempdir.path()));

        let open = crate::nzb_core::schedule::ScheduleDecision {
            pause: true,
            ..Default::default()
        };
        manager.apply_schedule(&open, true);
        assert!(manager.is_paused());

        // Turning the schedule off substitutes an idle decision, which is the
        // window closing from the queue's point of view.
        manager.apply_schedule(&open, false);
        assert!(!manager.is_paused());
        assert!(!manager.schedule_pause_active());

        // And re-enabling it opens a fresh window.
        manager.apply_schedule(&open, true);
        assert!(manager.is_paused());
        assert_eq!(manager.global_pause_reason(), Some(PauseReason::Schedule));
    }

    #[tokio::test]
    async fn schedule_speed_cap_overlays_the_configured_limit_without_rewriting_it() {
        let (manager, _tempdir) = manager();
        manager.set_speed_limit(5_000_000);
        assert_eq!(manager.configured_speed_limit(), 5_000_000);

        let cap = crate::nzb_core::schedule::ScheduleDecision {
            speed_limit_bps: Some(1_000_000),
            ..Default::default()
        };
        manager.apply_schedule(&cap, true);
        assert_eq!(manager.scheduled_speed_cap(), Some(1_000_000));
        assert_eq!(manager.configured_speed_limit(), 5_000_000);
        // The limiter the downloader reads is the cap, not the user's value.
        assert_eq!(manager.get_speed_limit(), 1_000_000);

        manager.apply_schedule(
            &crate::nzb_core::schedule::ScheduleDecision::default(),
            true,
        );
        assert_eq!(manager.scheduled_speed_cap(), None);
        assert_eq!(manager.configured_speed_limit(), 5_000_000);
        // ...and ending the window puts the user's limit back on the wire.
        assert_eq!(manager.get_speed_limit(), 5_000_000);
    }

    #[tokio::test]
    async fn a_schedule_cap_of_zero_means_unlimited_even_over_a_configured_limit() {
        let (manager, _tempdir) = manager();
        manager.set_speed_limit(5_000_000);

        let cap = crate::nzb_core::schedule::ScheduleDecision {
            speed_limit_bps: Some(0),
            ..Default::default()
        };
        manager.apply_schedule(&cap, true);
        assert_eq!(manager.scheduled_speed_cap(), Some(0));
        assert_eq!(
            manager.get_speed_limit(),
            0,
            "an explicit 0 cap lifts the user's limit for the window"
        );

        manager.apply_schedule(
            &crate::nzb_core::schedule::ScheduleDecision::default(),
            true,
        );
        assert_eq!(manager.get_speed_limit(), 5_000_000);
    }

    #[tokio::test]
    async fn a_disabled_schedule_applies_nothing_and_lifts_an_active_cap() {
        let (manager, _tempdir) = manager();
        manager.set_speed_limit(5_000_000);

        let cap = crate::nzb_core::schedule::ScheduleDecision {
            speed_limit_bps: Some(1_000_000),
            disabled_servers: vec!["backup".into()],
            pause: true,
            ..Default::default()
        };
        manager.apply_schedule(&cap, true);
        assert_eq!(manager.get_speed_limit(), 1_000_000);
        assert!(manager.schedule_pause_active());

        // Toggling the schedule off must release everything it applied, even
        // though the decision it is handed still says otherwise.
        manager.apply_schedule(&cap, false);
        assert_eq!(manager.scheduled_speed_cap(), None);
        assert_eq!(manager.get_speed_limit(), 5_000_000);
        assert!(manager.schedule_disabled_servers().is_empty());
        assert!(!manager.schedule_pause_active());
        assert!(!manager.is_paused());
    }

    #[tokio::test]
    async fn schedule_server_exclusions_leave_the_configured_list_alone() {
        let (manager, _tempdir) = manager();
        let server = |id: &str| {
            let mut config = ServerConfig::new(id, format!("{id}.example.com"));
            config.enabled = true;
            config
        };
        manager.update_servers(vec![server("primary"), server("backup")]);
        assert!(manager.get_servers().iter().all(|s| s.enabled));

        let decision = crate::nzb_core::schedule::ScheduleDecision {
            disabled_servers: vec!["backup".into()],
            ..Default::default()
        };
        manager.apply_schedule(&decision, true);
        assert_eq!(manager.schedule_disabled_servers(), vec!["backup"]);
        assert!(
            manager
                .get_servers()
                .iter()
                .find(|s| s.id == "backup")
                .is_some_and(|s| s.enabled),
            "the configured list keeps its own enabled flags"
        );
        assert!(
            manager
                .live_servers()
                .iter()
                .find(|s| s.id == "backup")
                .is_some_and(|s| !s.enabled),
            "the dispatcher sees the exclusion"
        );

        manager.apply_schedule(
            &crate::nzb_core::schedule::ScheduleDecision::default(),
            true,
        );
        assert!(manager.schedule_disabled_servers().is_empty());
        assert!(manager.live_servers().iter().all(|s| s.enabled));
    }

    #[tokio::test]
    async fn preflight_storage_hold_reports_disk_space_and_resumes_when_space_frees() {
        let (manager, tempdir) = manager();
        insert_job(&manager, job("held", JobStatus::Paused, tempdir.path()));
        {
            let mut jobs = manager.jobs.lock();
            let state = jobs.get_mut("held").unwrap();
            state.job.error_message = Some("Paused: low disk space".to_string());
            state.failure_code = Some(JobFailureCode::StorageUnavailable);
        }
        assert_eq!(manager.pause_reason("held"), Some(PauseReason::DiskSpace));

        manager.set_min_free_space(1);
        manager.enforce_disk_guard();

        let held = manager.get_job("held").unwrap();
        assert_ne!(held.status, JobStatus::Paused);
        assert_eq!(held.error_message, None);
        assert_eq!(manager.pause_reason("held"), None);
    }

    #[tokio::test]
    async fn manual_pause_takes_over_a_preflight_storage_hold() {
        let (manager, tempdir) = manager();
        insert_job(&manager, job("held", JobStatus::Paused, tempdir.path()));
        {
            let mut jobs = manager.jobs.lock();
            let state = jobs.get_mut("held").unwrap();
            state.job.error_message = Some("Paused: low disk space".to_string());
            state.failure_code = Some(JobFailureCode::StorageUnavailable);
        }

        manager.pause_job("held").unwrap();
        assert_eq!(manager.pause_reason("held"), Some(PauseReason::Manual));

        manager.set_min_free_space(1);
        manager.enforce_disk_guard();
        assert_eq!(manager.get_job("held").unwrap().status, JobStatus::Paused);
    }

    #[tokio::test]
    async fn server_error_pause_reports_server_reason() {
        let (manager, tempdir) = manager();
        insert_job(&manager, job("srv", JobStatus::Paused, tempdir.path()));
        manager
            .jobs
            .lock()
            .get_mut("srv")
            .unwrap()
            .job
            .error_message = Some("server unavailable".to_string());
        assert_eq!(manager.pause_reason("srv"), Some(PauseReason::Server));
    }

    /// Neither loading nor an idle monitor tick may write pause settings.
    ///
    /// The e2e seed inserts its own `globally_paused` row after the backend's
    /// first start; if boot materialises the defaults the seed collides with
    /// them.
    #[tokio::test]
    async fn a_fresh_database_gets_no_pause_settings_from_boot() {
        let (manager, _tempdir) = manager();
        manager.restore_from_db().unwrap();
        manager.apply_schedule(
            &crate::nzb_core::schedule::ScheduleDecision::default(),
            true,
        );

        for key in [
            "globally_paused",
            QueueManager::USER_HOLD_SETTING,
            QueueManager::SCHEDULE_HOLD_SETTING,
            QueueManager::DISK_SPACE_HOLD_SETTING,
            QueueManager::PAUSE_OWNER_SETTING,
            QueueManager::SCHEDULE_WINDOWS_SETTING,
            QueueManager::SCHEDULE_TRIGGERED_SETTING,
            QueueManager::SCHEDULE_RESUMED_SETTING,
            QueueManager::SCHEDULE_USER_PAUSED_SETTING,
        ] {
            assert_eq!(
                manager.db.lock().get_setting(key),
                None,
                "{key} was written on load"
            );
        }
        assert!(!manager.is_paused());
    }

    #[tokio::test]
    async fn restored_disk_space_hold_keeps_its_reason() {
        let (manager, _tempdir) = manager();
        {
            let db = manager.db.lock();
            db.set_setting("globally_paused", "true");
            db.set_setting(
                QueueManager::DISK_SPACE_HOLD_SETTING,
                &Utc::now().to_rfc3339(),
            );
        }
        manager.restore_from_db().unwrap();
        assert_eq!(manager.global_pause_reason(), Some(PauseReason::DiskSpace));

        manager.set_min_free_space(1);
        manager.enforce_disk_guard();
        assert!(!manager.is_paused());
    }

    #[tokio::test]
    async fn provider_waiting_status_keeps_job_active_and_clears_on_recovery() {
        let (manager, tempdir) = manager();
        insert_job(
            &manager,
            job("provider-wait", JobStatus::Downloading, tempdir.path()),
        );
        let (progress_tx, progress_rx) = mpsc::channel(4);
        let handler = tokio::spawn(Arc::clone(&manager).handle_progress(
            "provider-wait".to_string(),
            progress_rx,
            Arc::new(SpeedTracker::new()),
        ));
        let message = "Waiting for providers: every enabled server is temporarily unavailable. rustnzb will retry automatically.";

        progress_tx
            .send(ProgressUpdate::WaitingForProviders {
                job_id: "provider-wait".to_string(),
                message: message.to_string(),
            })
            .await
            .unwrap();
        tokio::task::yield_now().await;
        let waiting = manager.get_job("provider-wait").unwrap();
        assert_eq!(waiting.status, JobStatus::Downloading);
        assert_eq!(waiting.error_message.as_deref(), Some(message));

        progress_tx
            .send(ProgressUpdate::ProvidersAvailable {
                job_id: "provider-wait".to_string(),
            })
            .await
            .unwrap();
        tokio::task::yield_now().await;
        assert!(
            manager
                .get_job("provider-wait")
                .unwrap()
                .error_message
                .is_none()
        );

        drop(progress_tx);
        handler.await.unwrap();
    }

    #[tokio::test]
    async fn job_added_during_global_pause_is_owned_by_global_pause() {
        let (manager, tempdir) = manager();
        manager.pause_all();

        manager
            .add_job(job("new", JobStatus::Queued, tempdir.path()), None)
            .unwrap();

        assert_eq!(manager.get_job("new").unwrap().status, JobStatus::Paused);
        assert!(manager.globally_paused_jobs.lock().contains("new"));
    }

    #[tokio::test]
    async fn removing_terminal_queue_view_does_not_insert_history_twice() {
        let (manager, tempdir) = manager();
        let mut terminal = job("terminal", JobStatus::Failed, tempdir.path());
        terminal.error_message = Some("original failure".into());
        insert_job(&manager, terminal);
        {
            let mut jobs = manager.jobs.lock();
            let state = jobs.get_mut("terminal").unwrap();
            manager.move_to_history(state, Vec::new());
        }

        manager.remove_job("terminal").unwrap();
        let db = manager.db.lock();
        let persisted = db.history_get("terminal").unwrap().unwrap();
        assert_eq!(persisted.error_message.as_deref(), Some("original failure"));
        assert_eq!(
            db.history_list(100)
                .unwrap()
                .iter()
                .filter(|entry| entry.id == "terminal")
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn removing_terminal_queue_view_keeps_work_dir_owned_by_history() {
        let (manager, tempdir) = manager();
        let mut terminal = job("terminal-retained", JobStatus::Failed, tempdir.path());
        terminal.error_message = Some("articles missing".into());
        let work_dir = terminal.work_dir.clone();
        insert_job(&manager, terminal);
        {
            let mut jobs = manager.jobs.lock();
            let state = jobs.get_mut("terminal-retained").unwrap();
            manager.move_to_history(state, Vec::new());
        }
        // Stand-in for a partial download retained for history retry.
        std::fs::create_dir_all(&work_dir).unwrap();
        std::fs::write(work_dir.join("partial.bin"), b"partial").unwrap();

        manager.remove_job("terminal-retained").unwrap();

        assert!(
            work_dir.join("partial.bin").exists(),
            "the history row owns a retained work dir once it is persisted"
        );
    }

    #[tokio::test]
    async fn repeated_terminal_persistence_keeps_one_history_row() {
        let (manager, tempdir) = manager();
        let mut terminal = job("idempotent-terminal", JobStatus::Failed, tempdir.path());
        terminal.error_message = Some("original failure".into());
        insert_job(&manager, terminal);

        let mut jobs = manager.jobs.lock();
        let state = jobs.get_mut("idempotent-terminal").unwrap();
        manager.move_to_history(state, Vec::new());
        manager.move_to_history(state, Vec::new());
        drop(jobs);

        let db = manager.db.lock();
        assert_eq!(
            db.history_list(100)
                .unwrap()
                .iter()
                .filter(|entry| entry.id == "idempotent-terminal")
                .count(),
            1
        );
        assert_eq!(
            db.history_get("idempotent-terminal")
                .unwrap()
                .unwrap()
                .error_message
                .as_deref(),
            Some("original failure")
        );
        assert_eq!(manager.history_update(), 2);
    }

    #[tokio::test]
    async fn history_generation_changes_only_for_real_mutations() {
        let (manager, tempdir) = manager();
        assert_eq!(manager.history_update(), 1);

        manager.history_clear().unwrap();
        manager.history_remove("missing").unwrap();
        assert_eq!(manager.history_update(), 1);

        insert_job(
            &manager,
            job("counter-terminal", JobStatus::Completed, tempdir.path()),
        );
        {
            let mut jobs = manager.jobs.lock();
            manager.move_to_history(jobs.get_mut("counter-terminal").unwrap(), Vec::new());
        }
        assert_eq!(manager.history_update(), 2);

        manager.history_remove("counter-terminal").unwrap();
        assert_eq!(manager.history_update(), 3);

        manager.history_remove("counter-terminal").unwrap();
        manager.history_clear().unwrap();
        assert_eq!(manager.history_update(), 3);
    }

    /// GH #136: a configured retention of 0 used to run `LIMIT 0` retention
    /// right after the insert and silently delete the row just persisted.
    #[tokio::test]
    async fn zero_history_retention_keeps_completed_jobs() {
        let (manager, tempdir) = manager();
        manager.set_history_retention(Some(0));
        assert_eq!(manager.get_history_retention(), None);

        insert_job(
            &manager,
            job("zero-retention", JobStatus::Completed, tempdir.path()),
        );
        {
            let mut jobs = manager.jobs.lock();
            manager.move_to_history(jobs.get_mut("zero-retention").unwrap(), Vec::new());
        }

        let db = manager.db.lock();
        let entry = db
            .history_get("zero-retention")
            .unwrap()
            .expect("completed job must remain in history with retention 0");
        assert_eq!(entry.status, JobStatus::Completed);
        assert_eq!(db.history_count().unwrap(), 1);
    }

    /// A positive retention limit still prunes, oldest first.
    #[tokio::test]
    async fn positive_history_retention_prunes_after_completion() {
        let (manager, tempdir) = manager();
        manager.set_history_retention(Some(1));
        assert_eq!(manager.get_history_retention(), Some(1));

        for id in ["ret-first", "ret-second"] {
            insert_job(&manager, job(id, JobStatus::Completed, tempdir.path()));
            let mut jobs = manager.jobs.lock();
            manager.move_to_history(jobs.get_mut(id).unwrap(), Vec::new());
        }

        let db = manager.db.lock();
        assert_eq!(db.history_count().unwrap(), 1);
        assert!(db.history_get("ret-second").unwrap().is_some());
    }

    #[tokio::test]
    async fn failed_history_cleanup_removes_raw_work_directory_after_persistence() {
        let (manager, tempdir) = manager();
        let failed = job("failed-cleanup", JobStatus::Failed, tempdir.path());
        std::fs::create_dir_all(&failed.work_dir).unwrap();
        std::fs::write(failed.work_dir.join("raw-volume.rar"), b"raw articles").unwrap();
        let work_dir = failed.work_dir.clone();
        insert_job(&manager, failed);

        let mut jobs = manager.jobs.lock();
        manager.move_to_history(jobs.get_mut("failed-cleanup").unwrap(), Vec::new());
        drop(jobs);

        assert!(
            manager
                .db
                .lock()
                .history_get("failed-cleanup")
                .unwrap()
                .is_some()
        );
        assert!(!work_dir.exists());
        assert_eq!(
            manager
                .db
                .lock()
                .history_get("failed-cleanup")
                .unwrap()
                .unwrap()
                .failure_code,
            Some(JobFailureCode::DownloadFailed)
        );
    }

    #[tokio::test]
    async fn completed_history_cleanup_retains_source_when_output_move_is_unsafe() {
        let (manager, tempdir) = manager();
        let completed = job("completed-retain", JobStatus::Completed, tempdir.path());
        std::fs::create_dir_all(&completed.work_dir).unwrap();
        std::fs::write(completed.work_dir.join("unmoved.bin"), b"payload").unwrap();
        // A regular file in place of the complete root makes reserving the
        // output folder, rename and copy all fail, so the work directory must
        // be retained instead of silently losing data. (A file at output_dir
        // itself is now a name collision and resolves to `<name>.1`.)
        std::fs::write(completed.output_dir.parent().unwrap(), b"not a directory").unwrap();
        let work_dir = completed.work_dir.clone();
        insert_job(&manager, completed);

        let mut jobs = manager.jobs.lock();
        manager.move_to_history(jobs.get_mut("completed-retain").unwrap(), Vec::new());
        drop(jobs);

        assert!(
            manager
                .db
                .lock()
                .history_get("completed-retain")
                .unwrap()
                .is_some()
        );
        assert!(work_dir.join("unmoved.bin").exists());
    }

    #[tokio::test]
    async fn post_processing_with_only_raw_artifacts_is_failed_not_completed() {
        let (manager, tempdir) = manager();
        let raw = job("raw-artifacts", JobStatus::PostProcessing, tempdir.path());
        std::fs::create_dir_all(&raw.work_dir).unwrap();
        std::fs::write(raw.work_dir.join("release.part001.rar"), b"raw").unwrap();
        std::fs::write(raw.work_dir.join("release.part002.rar"), b"raw").unwrap();
        std::fs::write(raw.work_dir.join("release.par2"), b"par2").unwrap();
        let work_dir = raw.work_dir.clone();
        insert_job(&manager, raw);

        let stages = vec![StageResult {
            name: "Extract".to_string(),
            status: StageStatus::Skipped,
            message: Some("No archives found".to_string()),
            duration_secs: 0.0,
        }];
        let mut jobs = manager.jobs.lock();
        manager.move_to_history(jobs.get_mut("raw-artifacts").unwrap(), stages);
        drop(jobs);

        let entry = manager
            .db
            .lock()
            .history_get("raw-artifacts")
            .unwrap()
            .unwrap();
        assert_eq!(entry.status, JobStatus::Failed);
        assert_eq!(entry.failure_code, Some(JobFailureCode::ArchiveInvalid));
        assert_eq!(
            entry.error_message.as_deref(),
            Some("No usable output produced; only archive or PAR2 artifacts remain")
        );
        assert!(
            entry
                .stages
                .iter()
                .any(|stage| { stage.name == "Output" && stage.status == StageStatus::Failed })
        );
        assert!(
            !work_dir.exists(),
            "failed raw artifacts are cleaned after history persists"
        );
    }

    fn set_default_pp(manager: &QueueManager, level: u8) {
        manager.set_categories(vec![CategoryConfig {
            post_processing: level,
            ..CategoryConfig::default()
        }]);
    }

    fn write_raw_artifacts(work_dir: &std::path::Path) {
        std::fs::create_dir_all(work_dir).unwrap();
        std::fs::write(work_dir.join("release.part001.rar"), b"raw").unwrap();
        std::fs::write(work_dir.join("release.part002.rar"), b"raw").unwrap();
        std::fs::write(work_dir.join("release.par2"), b"par2").unwrap();
    }

    /// SABnzbd `pp=0` is download only: the raw archives are the output, so
    /// the "no usable output" rule must not fail the job (even when a
    /// post-processing script contributed a stage).
    #[tokio::test]
    async fn download_only_job_with_raw_archives_is_completed() {
        let (manager, tempdir) = manager();
        set_default_pp(&manager, 0);
        let raw = job("download-only", JobStatus::PostProcessing, tempdir.path());
        write_raw_artifacts(&raw.work_dir);
        let output = raw.output_dir.clone();
        insert_job(&manager, raw);

        let stages = vec![StageResult {
            name: "Script".to_string(),
            status: StageStatus::Success,
            message: Some("ok".to_string()),
            duration_secs: 0.0,
        }];
        let mut jobs = manager.jobs.lock();
        manager.move_to_history(jobs.get_mut("download-only").unwrap(), stages);
        drop(jobs);

        let entry = manager
            .db
            .lock()
            .history_get("download-only")
            .unwrap()
            .unwrap();
        assert_eq!(
            entry.status,
            JobStatus::Completed,
            "{:?}",
            entry.error_message
        );
        assert_eq!(entry.post_processing, Some(0));
        assert!(output.join("release.part001.rar").exists());
        assert!(output.join("release.par2").exists());
    }

    /// SABnzbd `pp=1` is repair only: archives are the expected output.
    #[tokio::test]
    async fn repair_only_job_with_raw_archives_is_completed() {
        let (manager, tempdir) = manager();
        set_default_pp(&manager, 1);
        let raw = job("repair-only", JobStatus::PostProcessing, tempdir.path());
        write_raw_artifacts(&raw.work_dir);
        let output = raw.output_dir.clone();
        insert_job(&manager, raw);

        let stages = vec![StageResult {
            name: "Verify".to_string(),
            status: StageStatus::Skipped,
            message: Some("Skipped".to_string()),
            duration_secs: 0.0,
        }];
        let mut jobs = manager.jobs.lock();
        manager.move_to_history(jobs.get_mut("repair-only").unwrap(), stages);
        drop(jobs);

        let entry = manager
            .db
            .lock()
            .history_get("repair-only")
            .unwrap()
            .unwrap();
        assert_eq!(
            entry.status,
            JobStatus::Completed,
            "{:?}",
            entry.error_message
        );
        assert_eq!(entry.post_processing, Some(1));
        assert!(output.join("release.part002.rar").exists());
    }

    /// The per-job override decides the level recorded in history.
    #[tokio::test]
    async fn history_records_the_job_override_level() {
        let (manager, tempdir) = manager();
        set_default_pp(&manager, 3);
        let mut raw = job("override-level", JobStatus::PostProcessing, tempdir.path());
        raw.pp_override = Some(1);
        write_raw_artifacts(&raw.work_dir);
        insert_job(&manager, raw);

        let mut jobs = manager.jobs.lock();
        manager.move_to_history(jobs.get_mut("override-level").unwrap(), Vec::new());
        drop(jobs);

        let entry = manager
            .db
            .lock()
            .history_get("override-level")
            .unwrap()
            .unwrap();
        assert_eq!(entry.post_processing, Some(1));
        assert_eq!(entry.status, JobStatus::Completed);
    }

    /// `pp=1` must not unpack: a (bogus) archive is delivered as-is instead
    /// of failing extraction.
    #[tokio::test]
    async fn repair_only_pipeline_does_not_extract() {
        let (manager, tempdir) = manager();
        set_default_pp(&manager, 1);
        let raw = job(
            "repair-no-unpack",
            JobStatus::PostProcessing,
            tempdir.path(),
        );
        std::fs::create_dir_all(&raw.work_dir).unwrap();
        std::fs::write(raw.work_dir.join("release.zip"), b"not really a zip").unwrap();
        let output = raw.output_dir.clone();
        insert_job(&manager, raw);

        manager.on_job_finished("repair-no-unpack", true, 0).await;

        let entry = manager
            .db
            .lock()
            .history_get("repair-no-unpack")
            .unwrap()
            .unwrap();
        assert_eq!(
            entry.status,
            JobStatus::Completed,
            "{:?}",
            entry.error_message
        );
        assert!(entry.stages.iter().all(|stage| stage.name != "Extract"));
        assert!(output.join("release.zip").exists());
    }

    /// `pp=0` does not verify or repair, so missing articles do not fail
    /// the job; the raw files are delivered like SABnzbd does.
    #[tokio::test]
    async fn download_only_job_with_failed_articles_is_completed() {
        let (manager, tempdir) = manager();
        set_default_pp(&manager, 0);
        let raw = job(
            "download-only-damaged",
            JobStatus::PostProcessing,
            tempdir.path(),
        );
        write_raw_artifacts(&raw.work_dir);
        let output = raw.output_dir.clone();
        insert_job(&manager, raw);

        manager
            .on_job_finished("download-only-damaged", false, 2)
            .await;

        let entry = manager
            .db
            .lock()
            .history_get("download-only-damaged")
            .unwrap()
            .unwrap();
        assert_eq!(
            entry.status,
            JobStatus::Completed,
            "{:?}",
            entry.error_message
        );
        assert!(output.join("release.part001.rar").exists());
    }

    #[tokio::test]
    async fn post_processing_with_payload_is_completed() {
        let (manager, tempdir) = manager();
        let payload = job("payload", JobStatus::PostProcessing, tempdir.path());
        std::fs::create_dir_all(&payload.work_dir).unwrap();
        std::fs::write(payload.work_dir.join("Movie.2024.mkv"), b"media").unwrap();
        let output = payload.output_dir.clone();
        insert_job(&manager, payload);

        let stages = vec![StageResult {
            name: "Extract".to_string(),
            status: StageStatus::Skipped,
            message: Some("No archives found".to_string()),
            duration_secs: 0.0,
        }];
        let mut jobs = manager.jobs.lock();
        manager.move_to_history(jobs.get_mut("payload").unwrap(), stages);
        drop(jobs);

        let entry = manager.db.lock().history_get("payload").unwrap().unwrap();
        assert_eq!(entry.status, JobStatus::Completed);
        assert!(output.join("Movie.2024.mkv").exists());
    }

    #[tokio::test]
    async fn removing_active_failed_job_creates_one_history_row() {
        let (manager, tempdir) = manager();
        let mut failed = job("active-failed", JobStatus::Paused, tempdir.path());
        failed.error_message = Some("providers unavailable".into());
        insert_job(&manager, failed);

        manager.remove_job("active-failed").unwrap();
        assert!(
            manager
                .db
                .lock()
                .history_get("active-failed")
                .unwrap()
                .is_some()
        );
    }

    #[tokio::test]
    async fn removal_during_post_processing_is_deferred() {
        let (manager, tempdir) = manager();
        insert_job(
            &manager,
            job("post-processing", JobStatus::PostProcessing, tempdir.path()),
        );

        manager.remove_job("post-processing").unwrap();
        assert!(manager.get_job("post-processing").is_some());
        assert!(
            manager
                .db
                .lock()
                .history_get("post-processing")
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn interrupted_post_processing_resumes_into_terminal_history() {
        let (manager, tempdir) = manager();
        let mut interrupted = job(
            "restart-postproc",
            JobStatus::PostProcessing,
            tempdir.path(),
        );
        std::fs::create_dir_all(&interrupted.work_dir).unwrap();
        std::fs::write(interrupted.work_dir.join("payload.mkv"), b"payload").unwrap();
        interrupted.total_bytes = 7;
        interrupted.downloaded_bytes = 7;
        manager.db.lock().queue_insert(&interrupted).unwrap();

        manager.restore_from_db().unwrap();

        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if manager
                    .db
                    .lock()
                    .history_get("restart-postproc")
                    .unwrap()
                    .is_some()
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("recovered post-processing should reach terminal history");

        let history = manager
            .db
            .lock()
            .history_get("restart-postproc")
            .unwrap()
            .unwrap();
        assert_eq!(history.status, JobStatus::Completed);
        assert!(interrupted.output_dir.join("payload.mkv").exists());
    }

    #[tokio::test]
    async fn interrupted_post_processing_resumes_into_its_reserved_output_dir() {
        let (manager, tempdir) = manager();
        let mut interrupted = job("SameName", JobStatus::PostProcessing, tempdir.path());
        interrupted.id = "restart-claimed".to_string();
        std::fs::create_dir_all(&interrupted.work_dir).unwrap();
        std::fs::write(interrupted.work_dir.join("second.mkv"), b"second").unwrap();
        // Another job already completed into the plain name; this job had
        // reserved `SameName.1` (and extracted into it) before the restart.
        let foreign = interrupted.output_dir.clone();
        std::fs::create_dir_all(&foreign).unwrap();
        std::fs::write(foreign.join("first.mkv"), b"first").unwrap();
        let reserved = foreign.with_file_name("SameName.1");
        std::fs::create_dir_all(&reserved).unwrap();
        std::fs::write(reserved.join("extracted.mkv"), b"extracted").unwrap();
        interrupted.output_dir = reserved.clone();
        {
            let db = manager.db.lock();
            db.queue_insert(&interrupted).unwrap();
            let checkpoint = checkpoint_for_job(&interrupted, true);
            db.queue_store_job_data(&interrupted.id, &serde_json::to_vec(&checkpoint).unwrap())
                .unwrap();
        }

        manager.restore_from_db().unwrap();

        tokio::time::timeout(Duration::from_secs(2), async {
            while manager
                .db
                .lock()
                .history_get("restart-claimed")
                .unwrap()
                .is_none()
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("recovered post-processing should reach terminal history");

        let history = manager
            .db
            .lock()
            .history_get("restart-claimed")
            .unwrap()
            .unwrap();
        assert_eq!(history.status, JobStatus::Completed);
        assert_eq!(history.output_dir, reserved);
        assert!(reserved.join("extracted.mkv").exists());
        assert!(reserved.join("second.mkv").exists());
        assert!(!foreign.join("second.mkv").exists());
        assert!(!foreign.with_file_name("SameName.2").exists());
    }

    /// A progress save waiting on the database (a slow history query, say)
    /// must not hold the job list: queue and status polls read `get_jobs()`
    /// and have to answer while the save is still pending.
    #[tokio::test]
    async fn queue_reads_are_not_blocked_behind_a_pending_progress_save() {
        let (manager, tempdir) = manager();
        insert_job(
            &manager,
            job("busy", JobStatus::Downloading, tempdir.path()),
        );

        let db_guard = manager.db.lock();
        let saver = {
            let manager = Arc::clone(&manager);
            std::thread::spawn(move || manager.persist_job_progress("busy"))
        };
        // Let the save take its snapshot and block on the database lock.
        std::thread::sleep(Duration::from_millis(100));
        assert!(
            !saver.is_finished(),
            "save should be waiting on the DB lock"
        );

        let (tx, rx) = std::sync::mpsc::channel();
        let reader = {
            let manager = Arc::clone(&manager);
            std::thread::spawn(move || {
                let _ = tx.send(manager.get_jobs().len());
            })
        };
        let seen = rx.recv_timeout(Duration::from_secs(2));
        drop(db_guard);
        saver.join().unwrap();
        reader.join().unwrap();
        assert_eq!(
            seen.expect("get_jobs() blocked behind a pending progress save"),
            1
        );
        assert!(
            manager
                .db
                .lock()
                .queue_load_job_data("busy")
                .unwrap()
                .is_some(),
            "the save completes once the database is free"
        );
    }

    fn db_status(manager: &QueueManager, id: &str) -> JobStatus {
        manager
            .db
            .lock()
            .queue_list()
            .unwrap()
            .into_iter()
            .find(|j| j.id == id)
            .expect("queue row")
            .status
    }

    /// Run a progress save that snapshots `from`, then, while it waits on the
    /// database, apply a pause or resume the way the control paths do (job
    /// list, then database) and let the save finish. The database must keep
    /// the newer status.
    fn status_change_during_pending_save(from: JobStatus, to: JobStatus) {
        let (manager, tempdir) = manager();
        insert_job(&manager, job("raced", from, tempdir.path()));
        manager.persist_job_progress("raced");
        assert_eq!(db_status(&manager, "raced"), from);

        let db_guard = manager.db.lock();
        let saver = {
            let manager = Arc::clone(&manager);
            std::thread::spawn(move || manager.persist_job_progress("raced"))
        };
        // Let the save take its snapshot and block on the database lock.
        std::thread::sleep(Duration::from_millis(100));
        assert!(
            !saver.is_finished(),
            "save should be waiting on the DB lock"
        );
        {
            let mut jobs = manager.jobs.lock();
            let state = jobs.get_mut("raced").unwrap();
            state.job.status = to;
            state.persisted_status = Some(to);
            db_guard
                .queue_update_progress("raced", to, 0, 0, 0, 0)
                .unwrap();
        }
        drop(db_guard);
        saver.join().unwrap();

        assert_eq!(
            db_status(&manager, "raced"),
            to,
            "a stale progress save overwrote the newer status"
        );
    }

    #[tokio::test]
    async fn pending_progress_save_does_not_undo_a_pause() {
        status_change_during_pending_save(JobStatus::Downloading, JobStatus::Paused);
    }

    #[tokio::test]
    async fn pending_progress_save_does_not_undo_a_resume() {
        status_change_during_pending_save(JobStatus::Paused, JobStatus::Downloading);
    }

    /// A status set only in memory (pause all, say) still reaches the
    /// database through the next progress save.
    #[tokio::test]
    async fn progress_save_persists_an_in_memory_status_change() {
        let (manager, tempdir) = manager();
        insert_job(&manager, job("mem", JobStatus::Downloading, tempdir.path()));
        manager.persist_job_progress("mem");
        manager.jobs.lock().get_mut("mem").unwrap().job.status = JobStatus::Paused;
        manager.persist_job_progress("mem");
        assert_eq!(db_status(&manager, "mem"), JobStatus::Paused);
    }

    #[tokio::test]
    async fn checkpoint_carries_download_accounting_across_a_restart() {
        let (manager, tempdir) = manager();
        let mut downloading = job("accounting", JobStatus::Downloading, tempdir.path());
        downloading.downloaded_bytes = 4_000;
        downloading.server_stats = vec![ServerArticleStats {
            server_id: "srv".into(),
            server_name: "Provider".into(),
            articles_downloaded: 4,
            articles_failed: 1,
            bytes_downloaded: 4_000,
        }];
        insert_job(&manager, downloading);
        {
            let mut jobs = manager.jobs.lock();
            let state = jobs.get_mut("accounting").unwrap();
            state.prior_download_secs = 40.0;
            state.carried_bytes = 1_000;
        }

        manager.persist_job_progress("accounting");

        let data = manager
            .db
            .lock()
            .queue_load_job_data("accounting")
            .unwrap()
            .unwrap();
        let checkpoint: JobCheckpoint = serde_json::from_slice(&data).unwrap();
        // The job is not registered with the dispatcher, so no live run adds
        // to the time accumulated by earlier runs.
        assert_eq!(checkpoint.download_time_secs, 40.0);
        assert_eq!(checkpoint.carried_bytes, 1_000);
        let mut restored = job("accounting", JobStatus::Downloading, tempdir.path());
        assert_eq!(
            restore_checkpoint_accounting(&mut restored, &checkpoint),
            (40.0, 1_000)
        );
        assert_eq!(restored.server_stats.len(), 1);
        assert_eq!(restored.server_stats[0].articles_downloaded, 4);
        assert_eq!(restored.server_stats[0].articles_failed, 1);
    }

    fn served(bytes: u64) -> Vec<ServerArticleStats> {
        vec![ServerArticleStats {
            server_id: "srv".into(),
            server_name: "Provider".into(),
            articles_downloaded: 2,
            articles_failed: 1,
            bytes_downloaded: bytes,
        }]
    }

    #[tokio::test]
    async fn server_stats_count_a_job_kept_after_history_once() {
        let (manager, tempdir) = manager();
        let mut finished = job("lingering", JobStatus::Completed, tempdir.path());
        finished.server_stats = served(1_000);
        finished.completed_at = Some(Utc::now());
        // move_to_history wrote the history and statistics rows, but the job
        // stays in the in-memory map for a few seconds afterwards.
        manager
            .db
            .lock()
            .history_insert(&HistoryEntry {
                id: finished.id.clone(),
                name: finished.name.clone(),
                category: finished.category.clone(),
                status: JobStatus::Completed,
                total_bytes: 1_000,
                downloaded_bytes: 1_000,
                added_at: finished.added_at,
                completed_at: Utc::now(),
                download_time_secs: Some(1.0),
                output_dir: finished.output_dir.clone(),
                stages: Vec::new(),
                error_message: None,
                failure_code: None,
                server_stats: finished.server_stats.clone(),
                nzb_data: None,
                retry_data: None,
                delete_archives: None,
                post_processing: None,
                par2_volumes_fetched: None,
                par2_volumes_available: None,
            })
            .unwrap();
        insert_job(&manager, finished);

        let stats = manager.server_stats_get_all(&[]);
        assert_eq!(stats.len(), 1);
        assert_eq!(stats[0].total_bytes, 1_000);
        assert_eq!(stats[0].total_ok, 2);
        assert_eq!(stats[0].total_fail, 1);
        assert_eq!(stats[0].today_bytes, 1_000);
    }

    #[tokio::test]
    async fn server_stats_count_live_progress_of_a_long_queued_job_today() {
        let (manager, tempdir) = manager();
        let mut downloading = job("old-queued", JobStatus::Downloading, tempdir.path());
        // Queued two days ago, downloading now.
        downloading.added_at = Utc::now() - chrono::Duration::days(2);
        downloading.server_stats = served(500);
        insert_job(&manager, downloading);

        let stats = manager.server_stats_get_all(&[]);
        assert_eq!(stats[0].total_bytes, 500);
        assert_eq!(stats[0].today_bytes, 500);
        assert_eq!(stats[0].today_ok, 2);
        assert_eq!(stats[0].week_bytes, 500);
    }

    #[tokio::test]
    async fn shutdown_drains_pending_progress_before_flushing_checkpoint() {
        let (manager, _tempdir) = manager();
        let nzb = br#"<?xml version="1.0" encoding="utf-8"?>
<nzb xmlns="http://www.newzbin.com/DTD/2003/nzb">
  <file poster="p" date="0" subject="&quot;drain.bin&quot; yEnc (1/2)">
    <groups><group>alt.binaries.test</group></groups>
    <segments>
      <segment bytes="5" number="1">drain-1@test</segment>
      <segment bytes="6" number="2">drain-2@test</segment>
    </segments>
  </file>
</nzb>"#;
        let mut job = nzb_parser::parse_nzb("drain", nzb).unwrap();
        job.id = "drain".into();
        job.status = JobStatus::Downloading;
        let filename = job.files[0].filename.clone();
        let file_id = job.files[0].id.clone();
        insert_job(&manager, job);

        // Wire up the per-job listener exactly as start_job does. The sender
        // stays alive for the whole test, as it does inside the worker pool's
        // job context, so the listener never sees its channel close.
        let (progress_tx, progress_rx) = mpsc::channel(16);
        let qm = Arc::clone(&manager);
        let handle = tokio::spawn(async move {
            qm.handle_progress("drain".into(), progress_rx, Arc::new(SpeedTracker::new()))
                .await;
        });
        manager
            .jobs
            .lock()
            .get_mut("drain")
            .unwrap()
            .progress_handle = Some(handle);

        // Article 1 finished in the last instant before shutdown: its event
        // is queued, but the listener has not processed it yet.
        progress_tx
            .try_send(ProgressUpdate::ArticleComplete {
                job_id: "drain".into(),
                file_id,
                segment_number: 1,
                decoded_bytes: 5,
                file_complete: false,
                server_id: None,
                yenc_filename: None,
            })
            .unwrap();

        tokio::time::timeout(Duration::from_secs(30), manager.shutdown())
            .await
            .expect("shutdown must not wait on a listener whose sender is still alive");

        let data = manager
            .db
            .lock()
            .queue_load_job_data("drain")
            .unwrap()
            .expect("shutdown should persist an article checkpoint");
        let checkpoint: JobCheckpoint = serde_json::from_slice(&data).unwrap();
        assert_eq!(
            checkpoint.files.get(&filename),
            Some(&vec![1]),
            "the queued ArticleComplete must be drained into the checkpoint"
        );
        assert_eq!(checkpoint.articles_downloaded, 1);
        assert_eq!(checkpoint.downloaded_bytes, 5);
        drop(progress_tx);
    }

    #[test]
    fn stored_output_claim_outside_the_job_target_is_ignored() {
        let tempdir = tempfile::tempdir().unwrap();
        let mut target = job("Name", JobStatus::PostProcessing, tempdir.path());
        let elsewhere = tempdir.path().join("elsewhere").join("Name.1");
        let unrelated = target.output_dir.with_file_name("Other");
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::fs::create_dir_all(&unrelated).unwrap();
        for claimed in [elsewhere, unrelated] {
            let mut stored = target.clone();
            stored.output_dir = claimed;
            let checkpoint = checkpoint_for_job(&stored, true);
            assert!(!restore_claimed_output_dir(&mut target, &checkpoint));
            assert_eq!(
                target.output_dir,
                tempdir.path().join("complete").join("Name")
            );
        }
    }
}

#[cfg(test)]
mod hopeless_tests {
    use super::*;

    /// Helper: build a tracker with N content articles of ~1MB each and M par2 articles.
    fn make_tracker(content_articles: usize, par2_articles: usize) -> HopelessTracker {
        let article_bytes: u64 = 750_000; // ~750KB per article
        HopelessTracker {
            created_at: Instant::now(),
            last_progress_at: Instant::now(),
            content_bytes: content_articles as u64 * article_bytes,
            recovery_capacity_bytes: par2_articles as u64 * article_bytes,
            recovery_blocks_total: par2_articles as u64,
            recovery_bytes_unavailable: 0,
            recovery_blocks_unavailable: 0,
            recovery_capacity_by_set: HashMap::new(),
            recovery_unavailable_by_set: HashMap::new(),
            missing_content_by_set: HashMap::new(),
            unassociated_missing_bytes: 0,
            content_file_sets: HashMap::new(),
            pending_file_classifications: std::collections::HashSet::new(),
            content_bytes_missing: 0,
            content_articles_checked: 0,
            content_articles_failed: 0,
            content_articles_total: content_articles,
            has_par2_set: par2_articles > 0,
            recovery_blocks_known: true,
            confirmed_missing_files: std::collections::HashSet::new(),
            confirmed_missing_unattributed: 0,
        }
    }

    /// A tracker for a job with a PAR2 set carrying `recovery_blocks` slices.
    fn par2_tracker(recovery_blocks: u64) -> HopelessTracker {
        let mut t = make_tracker(1000, 0);
        t.has_par2_set = true;
        t.recovery_blocks_total = recovery_blocks;
        t.recovery_capacity_bytes = recovery_blocks * 750_000;
        t
    }

    fn fail(t: &mut HopelessTracker, file: &str, kind: nzb_dispatch::ArticleFailureKind) {
        t.record_file_failure(Some(file), false, 750_000, kind);
    }

    #[test]
    fn parity_aborts_when_damage_exceeds_zero_recovery() {
        let mut t = par2_tracker(0);
        fail(&mut t, "mkv", nzb_dispatch::ArticleFailureKind::NotFound);
        let abort = t.check(true, true, 100.2).expect("no recovery: hopeless");
        assert_eq!(abort.tier, "parity");
        assert!(
            abort
                .reason
                .contains("slice(s) missing but the release only carries 0 recovery slice(s)"),
            "{}",
            abort.reason
        );
    }

    #[test]
    fn parity_does_not_abort_within_recovery() {
        let mut t = par2_tracker(20);
        for _ in 0..6 {
            fail(&mut t, "mkv", nzb_dispatch::ArticleFailureKind::NotFound);
        }
        assert!(t.check(true, true, 100.2).is_none());
    }

    #[test]
    fn parity_counts_each_damaged_file_once_at_least() {
        let mut t = par2_tracker(2);
        for file in ["a", "b", "c"] {
            fail(&mut t, file, nzb_dispatch::ArticleFailureKind::NotFound);
        }
        assert_eq!(t.damaged_slices_lower_bound(), 3);
        assert_eq!(t.check(true, true, 100.2).unwrap().tier, "parity");
    }

    #[test]
    fn parity_ignores_decode_errors_and_transient_failures() {
        let mut t = par2_tracker(0);
        fail(&mut t, "mkv", nzb_dispatch::ArticleFailureKind::DecodeError);
        fail(&mut t, "mkv", nzb_dispatch::ArticleFailureKind::Timeout);
        fail(&mut t, "mkv", nzb_dispatch::ArticleFailureKind::ServerDown);
        assert_eq!(t.damaged_slices_lower_bound(), 0);
        assert!(t.parity_check().is_none());
    }

    #[test]
    fn parity_stands_aside_when_a_volume_block_count_is_unknown() {
        let mut t = par2_tracker(0);
        t.recovery_blocks_known = false;
        fail(&mut t, "mkv", nzb_dispatch::ArticleFailureKind::NotFound);
        assert!(t.parity_check().is_none());
    }

    #[test]
    fn parity_waits_for_obfuscated_files_to_be_classified() {
        let mut t = par2_tracker(0);
        t.pending_file_classifications.insert("x".into());
        fail(&mut t, "mkv", nzb_dispatch::ArticleFailureKind::NotFound);
        assert!(t.check(true, true, 100.2).is_none());
    }

    #[test]
    fn job_without_par2_keeps_the_article_grace() {
        let mut t = make_tracker(1000, 0);
        for _ in 0..3 {
            fail(&mut t, "mkv", nzb_dispatch::ArticleFailureKind::NotFound);
        }
        assert!(t.check(true, true, 100.2).is_none());
    }

    fn map(damaged: usize, unknown: usize, recovery: u32) -> SliceMapSummary {
        let mut states = vec![SliceVerdict::Damaged; damaged];
        states.extend(vec![SliceVerdict::Unknown; unknown]);
        SliceMapSummary {
            states,
            recovery_block_count: recovery,
            damaged,
            unknown,
        }
    }

    #[test]
    fn parity_diagnosis_uses_confirmed_damage_only() {
        assert_eq!(
            parity_diagnosis(&map(7, 0, 0), Some(5)).as_deref(),
            Some("7 slice(s) missing but the release only carries 5 recovery slice(s)")
        );
        // Unknown slices are unestablished and never tip the balance.
        assert_eq!(parity_diagnosis(&map(2, 40, 0), Some(2)), None);
        // The index's own count can cover what filenames do not declare.
        assert_eq!(parity_diagnosis(&map(3, 0, 4), Some(0)), None);
        // Unknown declared recovery: no decision.
        assert_eq!(parity_diagnosis(&map(9, 0, 0), None), None);
    }

    #[test]
    fn grace_period_allows_small_failures() {
        let mut t = make_tracker(1000, 50);
        // Fail 5 articles (at the grace threshold)
        for _ in 0..5 {
            t.record_failure(false, 750_000, nzb_dispatch::ArticleFailureKind::NotFound);
        }
        assert!(
            t.check(true, true, 100.2).is_none(),
            "Should not abort within grace period"
        );
    }

    #[test]
    fn grace_period_disabled_when_abort_hopeless_off() {
        let mut t = make_tracker(100, 10);
        for _ in 0..100 {
            t.record_failure(false, 750_000, nzb_dispatch::ArticleFailureKind::NotFound);
        }
        assert!(
            t.check(false, true, 100.2).is_none(),
            "Should never abort when abort_hopeless is disabled"
        );
    }

    #[test]
    fn early_check_fires_at_80pct_of_first_10() {
        let mut t = make_tracker(1000, 50);
        // Simulate: 8 failures, 2 successes out of first 10
        for _ in 0..8 {
            t.record_failure(false, 750_000, nzb_dispatch::ArticleFailureKind::NotFound);
        }
        for _ in 0..2 {
            t.record_success(false);
        }
        let result = t.check(true, true, 100.2);
        assert!(result.is_some(), "Should abort: 80% failure in first 10");
        let abort = result.unwrap();
        assert_eq!(abort.tier, "early_failure");
        assert!(abort.reason.contains("80%"));
    }

    #[test]
    fn early_check_does_not_fire_below_threshold() {
        let mut t = make_tracker(1000, 50);
        // 7 failures, 3 successes = 70% failure
        for _ in 0..7 {
            t.record_failure(false, 750_000, nzb_dispatch::ArticleFailureKind::NotFound);
        }
        for _ in 0..3 {
            t.record_success(false);
        }
        // Still within grace (7 > 5) but early check at 70% < 80%
        // However, we need to check the ongoing ratio too.
        // 7 * 750KB missing out of 1000 * 750KB total = 0.7% missing
        // availability = 99.3%, which is below 100.2 — so it should still pass
        // because at 10 articles checked the ratio is still fine.
        // Actually: 7 * 750KB = 5.25MB missing out of 750MB total = 99.3% available
        // 99.3 < 100.2 → would abort via tier 3!
        // But wait, content_bytes = 1000 * 750_000 = 750MB, missing = 5.25MB
        // availability = (750MB - 5.25MB) / 750MB * 100 = 99.3%
        // 99.3 < 100.2 → yes this triggers tier 3.
        // With 1000 articles, 7 missing is easily repairable by par2.
        // The 100.2% threshold is very aggressive. Let's use a more reasonable test.
        let result = t.check(true, false, 95.0);
        assert!(
            result.is_none(),
            "Should not abort: 70% failure in first 10 with early_check disabled and low threshold"
        );
    }

    #[test]
    fn early_check_disabled_still_checks_ongoing() {
        let mut t = make_tracker(100, 10);
        // Fail all 100 content articles
        for _ in 0..100 {
            t.record_failure(false, 750_000, nzb_dispatch::ArticleFailureKind::NotFound);
        }
        let result = t.check(true, false, 100.0);
        assert!(
            result.is_some(),
            "Ongoing ratio should catch 100% failure even with early_check off"
        );
        let abort = result.unwrap();
        assert_eq!(abort.tier, "ongoing_availability");
        assert!(abort.reason.contains("effective completion 10.000%"));
    }

    #[test]
    fn par2_failures_reduce_recovery_without_counting_as_content_damage() {
        let mut t = make_tracker(100, 50);
        // Fail 20 par2 articles — should not affect content tracking
        for _ in 0..20 {
            t.record_failure(true, 750_000, nzb_dispatch::ArticleFailureKind::NotFound);
        }
        assert_eq!(t.content_articles_failed, 0);
        assert_eq!(t.content_bytes_missing, 0);
        assert_eq!(t.recovery_bytes_unavailable, 15_000_000);
        assert_eq!(t.recovery_blocks_unavailable, 20);
    }

    #[test]
    fn ongoing_ratio_triggers_when_too_many_missing() {
        let mut t = make_tracker(100, 10);
        // Fail 11 out of 100 articles with only 10 articles worth of
        // recovery. Effective completion is 99%, so this is hopeless.
        for _ in 0..11 {
            t.record_failure(false, 750_000, nzb_dispatch::ArticleFailureKind::NotFound);
        }
        for _ in 0..40 {
            t.record_success(false);
        }
        let result = t.check(true, true, 100.0);
        assert!(
            result.is_some(),
            "damage beyond recovery capacity should fail"
        );
    }

    #[test]
    fn recovery_capacity_covers_more_than_five_missing_articles() {
        let mut t = make_tracker(100, 12);
        for _ in 0..10 {
            t.record_failure(false, 750_000, nzb_dispatch::ArticleFailureKind::NotFound);
        }
        assert!(t.check(true, false, 100.2).is_none());
    }

    #[test]
    fn damage_exactly_at_capacity_still_requires_safety_reserve() {
        let mut t = make_tracker(100, 10);
        for _ in 0..10 {
            t.record_failure(false, 750_000, nzb_dispatch::ArticleFailureKind::NotFound);
        }
        let abort = t
            .check(true, false, 100.2)
            .expect("reserve must be available");
        assert_eq!(abort.tier, "ongoing_availability");
        assert!(t.check(true, false, 100.0).is_none());
    }

    #[test]
    fn recovery_from_another_par2_set_cannot_mask_damage() {
        let mut t = make_tracker(100, 22);
        t.recovery_capacity_by_set = HashMap::from([
            ("set-a".to_string(), 20 * 750_000),
            ("set-b".to_string(), 2 * 750_000),
        ]);
        t.content_file_sets
            .insert("file-b".to_string(), Some("set-b".to_string()));
        for _ in 0..6 {
            t.record_file_failure(
                Some("file-b"),
                false,
                750_000,
                nzb_dispatch::ArticleFailureKind::NotFound,
            );
        }

        let abort = t
            .check(true, false, 100.0)
            .expect("set A blocks cannot repair set B content");
        assert_eq!(abort.tier, "ongoing_availability");
    }

    #[test]
    fn obfuscated_par2_reclassification_excludes_index_capacity() {
        let mut index = make_tracker(10, 0);
        index.reclassify_as_par2("index", 750_000, 1, None, None, Some("set"));
        assert_eq!(index.content_bytes, 9 * 750_000);
        assert_eq!(index.recovery_capacity_bytes, 0);

        let mut volume = make_tracker(10, 0);
        volume.reclassify_as_par2("volume", 3 * 750_000, 3, Some(0), Some(3), Some("set"));
        assert_eq!(volume.content_bytes, 7 * 750_000);
        assert_eq!(volume.recovery_capacity_bytes, 3 * 750_000);
        assert_eq!(volume.recovery_blocks_total, 3);
    }

    #[test]
    fn early_check_fires_continuously_after_phase_6() {
        // Phase 6 removed the `<= total/4` window cap. Tier 2 now fires
        // whenever the failure rate is high enough — even past the 25%
        // mark. Previously this scenario would only trip tier 3
        // (ongoing_availability); now tier 2 wins because it's checked
        // first.
        let mut t = make_tracker(40, 5);
        for _ in 0..9 {
            t.record_failure(false, 750_000, nzb_dispatch::ArticleFailureKind::NotFound);
        }
        for _ in 0..2 {
            t.record_success(false);
        }
        // 11 checked, 9 failed = 81.8% failure rate, well above the 80%
        // threshold. Tier 2 should fire regardless of how many articles
        // remain to check.
        let result = t.check(true, true, 100.0);
        assert!(result.is_some());
        let abort = result.unwrap();
        assert_eq!(abort.tier, "early_failure");
    }

    #[test]
    fn completely_dead_nzb_aborts_fast() {
        let mut t = make_tracker(10000, 500);
        // First 10 articles all fail — exactly the scenario from the bug report
        for _ in 0..10 {
            t.record_failure(false, 750_000, nzb_dispatch::ArticleFailureKind::NotFound);
        }
        let result = t.check(true, true, 100.2);
        assert!(
            result.is_some(),
            "100% failure on first 10 should abort immediately"
        );
    }

    #[test]
    fn no_progress_timeout_fires_after_partial_success() {
        let mut t = make_tracker(100, 10);
        t.record_success(false);
        t.last_progress_at = Instant::now() - Duration::from_secs(301);

        let result = t.time_based_check(Duration::from_secs(300));
        assert!(result.is_some(), "late-stage stalls should abort");
        assert_eq!(result.unwrap().tier, "no_progress_timeout");
    }

    #[test]
    fn reset_progress_clock_prevents_abort_after_pause() {
        // Simulate a job that was paused for longer than the article timeout:
        // its progress clock is stale. Resuming must restart the clock (GH
        // #123) so the watchdog does not abort it on the next scan.
        let mut t = make_tracker(100, 10);
        t.last_progress_at = Instant::now() - Duration::from_secs(600);
        assert!(
            t.time_based_check(Duration::from_secs(300)).is_some(),
            "precondition: a stale clock should abort"
        );

        t.reset_progress_clock();

        assert!(
            t.time_based_check(Duration::from_secs(300)).is_none(),
            "resuming a paused job must not abort it: paused time must not count toward the article timeout"
        );
    }
}
