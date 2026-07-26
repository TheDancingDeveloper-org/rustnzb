//! Direct unpack: extract RAR volumes during download instead of after.
//!
//! Spawns `unrar x -vp` which pauses between volumes. As each volume finishes
//! assembly, we signal the unpacker to continue. If any article fails during
//! download, the unpacker is aborted and post-processing falls back to the
//! normal PAR2 repair + extract pipeline.

use std::collections::BTreeMap;
use std::io::{Read, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tracing::{debug, error, info, warn};

use nzb_postproc::find_unrar;

/// Error strings from unrar output that indicate an unrecoverable failure.
const UNRAR_ERROR_PATTERNS: &[&str] = &[
    "CRC failed",
    "checksum failed",
    "Cannot create",
    "Cannot open",
    "password is incorrect",
    "Incorrect password",
    "in the encrypted file",
    "not enough space on the disk",
    "Write error",
    "checksum error",
    "start extraction from a previous volume",
    "Unexpected end of archive",
];

/// A healthy extraction can wait for volumes, but a process that neither exits
/// nor emits output is stuck (for example, waiting on an unrecognised prompt).
const DIRECT_UNPACK_IDLE_TIMEOUT: Duration = Duration::from_secs(300);

/// Result of direct unpack for one RAR set.
#[derive(Debug, Clone)]
pub struct DirectUnpackResult {
    pub set_name: String,
    pub success: bool,
    pub error: Option<String>,
}

/// Tracks which volumes are available for a single RAR set.
#[derive(Debug)]
struct RarSetState {
    #[allow(dead_code)]
    set_name: String,
    /// volume_number → file path on disk (assembled and ready)
    volumes: BTreeMap<u32, PathBuf>,
}

/// Shared mutable state between the queue manager and the unpack task.
#[derive(Debug)]
struct DirectUnpackState {
    /// All RAR sets discovered so far, keyed by set_name.
    sets: BTreeMap<String, RarSetState>,
    /// Whether the download has finished (all files assembled).
    download_finished: bool,
}

/// Handle held by QueueManager to interact with the running direct unpacker.
///
/// Created lazily when the first RAR volume finishes assembly.
pub struct DirectUnpacker {
    /// Signal that a new volume has been assembled (or download finished).
    volume_ready: Arc<Notify>,
    /// Shared state: which volumes are available.
    state: Arc<Mutex<DirectUnpackState>>,
    /// Flag to signal abort.
    killed: Arc<AtomicBool>,
    /// Task handle for the background unrar process.
    task: JoinHandle<Vec<DirectUnpackResult>>,
}

impl DirectUnpacker {
    /// Create a new direct unpacker for a job.
    ///
    /// Returns `None` if `unrar` is not available on PATH.
    pub fn new(work_dir: &Path, output_dir: &Path, password: Option<String>) -> Option<Self> {
        let unrar_bin = find_unrar()?;

        let state = Arc::new(Mutex::new(DirectUnpackState {
            sets: BTreeMap::new(),
            download_finished: false,
        }));
        let volume_ready = Arc::new(Notify::new());
        let killed = Arc::new(AtomicBool::new(false));

        let task = {
            let state = Arc::clone(&state);
            let volume_ready = Arc::clone(&volume_ready);
            let killed = Arc::clone(&killed);
            let work_dir = work_dir.to_path_buf();
            let output_dir = output_dir.to_path_buf();

            tokio::task::spawn_blocking(move || {
                run_direct_unpack(
                    &unrar_bin,
                    &work_dir,
                    &output_dir,
                    password.as_deref(),
                    &state,
                    &volume_ready,
                    &killed,
                )
            })
        };

        Some(Self {
            volume_ready,
            state,
            killed,
            task,
        })
    }

    /// Register a newly assembled RAR volume. Call this from `handle_progress()`
    /// when a file completes assembly and `parse_rar_volume()` succeeds.
    pub fn add_volume(&self, set_name: &str, volume_number: u32, path: PathBuf) {
        {
            let mut state = self.state.lock();
            let set = state
                .sets
                .entry(set_name.to_string())
                .or_insert_with(|| RarSetState {
                    set_name: set_name.to_string(),
                    volumes: BTreeMap::new(),
                });
            set.volumes.insert(volume_number, path);
        }
        // Wake the unpack task — a new volume is available.
        self.volume_ready.notify_one();
    }

    /// Signal that the download phase is complete. The unpacker will finish
    /// processing any remaining queued sets or abort if volumes are missing.
    pub fn download_complete(&self) {
        {
            let mut state = self.state.lock();
            state.download_finished = true;
        }
        self.volume_ready.notify_one();
    }

    /// Abort the running unrar process and clean up.
    pub fn abort(&self) {
        self.killed.store(true, Ordering::Release);
        self.volume_ready.notify_one();
    }

    /// Wait for the direct unpack task to finish and return results.
    /// Call this from `on_job_finished()` before running the post-proc pipeline.
    pub async fn finish(self) -> Vec<DirectUnpackResult> {
        // Signal download complete in case it wasn't already.
        self.download_complete();

        match self.task.await {
            Ok(results) => results,
            Err(e) => {
                error!(error = %e, "Direct unpack task panicked");
                Vec::new()
            }
        }
    }
}

/// Main loop running in a blocking thread. Processes RAR sets one at a time.
fn run_direct_unpack(
    unrar_bin: &str,
    _work_dir: &Path,
    output_dir: &Path,
    password: Option<&str>,
    state: &Mutex<DirectUnpackState>,
    volume_ready: &Notify,
    killed: &AtomicBool,
) -> Vec<DirectUnpackResult> {
    let mut results: Vec<DirectUnpackResult> = Vec::new();
    let rt = tokio::runtime::Handle::current();

    // Wait for the first volume of any set to appear.
    loop {
        if killed.load(Ordering::Acquire) {
            return results;
        }

        let first_set = {
            let st = state.lock();
            // Find a set that has volume 0 ready and hasn't been processed yet.
            st.sets
                .iter()
                .find(|(name, set)| {
                    set.volumes.contains_key(&0) && !results.iter().any(|r| r.set_name == **name)
                })
                .map(|(name, set)| (name.clone(), set.volumes[&0].clone()))
        };

        if let Some((set_name, first_vol_path)) = first_set {
            info!(
                set = %set_name,
                first_volume = %first_vol_path.display(),
                "Starting direct unpack"
            );

            let result = unpack_set(
                unrar_bin,
                &set_name,
                &first_vol_path,
                output_dir,
                password,
                state,
                volume_ready,
                killed,
                &rt,
            );

            let success = result.success;
            results.push(result);

            if !success || killed.load(Ordering::Acquire) {
                return results;
            }

            // Check for more sets.
            continue;
        }

        // No set ready yet. Check if we should stop waiting.
        {
            let st = state.lock();
            if st.download_finished {
                // Check if there are unprocessed sets with volume 0 ready.
                let has_pending = st.sets.iter().any(|(name, set)| {
                    set.volumes.contains_key(&0) && !results.iter().any(|r| r.set_name == **name)
                });
                if !has_pending {
                    break;
                }
                continue;
            }
        }

        // Wait for a new volume to arrive.
        rt.block_on(volume_ready.notified());
    }

    results
}

/// Process a single RAR set: spawn unrar, feed volumes as they become available.
#[allow(clippy::too_many_arguments)]
fn unpack_set(
    unrar_bin: &str,
    set_name: &str,
    first_volume: &Path,
    output_dir: &Path,
    password: Option<&str>,
    state: &Mutex<DirectUnpackState>,
    volume_ready: &Notify,
    killed: &AtomicBool,
    rt: &tokio::runtime::Handle,
) -> DirectUnpackResult {
    // Create output directory.
    if let Err(e) = std::fs::create_dir_all(output_dir) {
        return DirectUnpackResult {
            set_name: set_name.to_string(),
            success: false,
            error: Some(format!("Failed to create output dir: {e}")),
        };
    }

    let pw_flag = match password {
        Some(pw) => format!("-p{pw}"),
        None => "-p-".to_string(),
    };

    // Spawn unrar with -vp (pause between volumes).
    // -o+ = overwrite, -y = assume yes, -vp = pause between volumes
    let mut child = match Command::new(unrar_bin)
        .args(["x", "-o+", "-y", "-vp"])
        .arg(&pw_flag)
        .arg(first_volume)
        .arg(format!("{}/", output_dir.display()))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            return DirectUnpackResult {
                set_name: set_name.to_string(),
                success: false,
                error: Some(format!("Failed to spawn unrar: {e}")),
            };
        }
    };

    let result = drive_unrar(&mut child, set_name, state, volume_ready, killed, rt);

    // Ensure child is cleaned up.
    let _ = child.kill();
    let _ = child.wait();

    result
}

/// Drive the unrar process: read output, detect prompts, feed volumes.
fn drive_unrar(
    child: &mut Child,
    set_name: &str,
    state: &Mutex<DirectUnpackState>,
    volume_ready: &Notify,
    killed: &AtomicBool,
    rt: &tokio::runtime::Handle,
) -> DirectUnpackResult {
    let mut stdin = child.stdin.take().expect("stdin was piped");
    let stdout = child.stdout.take().expect("stdout was piped");
    let stderr = child.stderr.take().expect("stderr was piped");
    let (output_tx, output_rx) = mpsc::channel();
    spawn_output_reader(stdout, output_tx.clone());
    spawn_output_reader(stderr, output_tx);

    let mut next_volume: u32 = 1; // Volume 0 is already being processed.
    let mut output_buf = String::with_capacity(1024);
    let mut last_output = Instant::now();

    loop {
        if killed.load(Ordering::Acquire) {
            return failed_result(set_name, "Aborted");
        }

        match output_rx.recv_timeout(Duration::from_secs(1)) {
            Ok(bytes) => {
                last_output = Instant::now();
                output_buf.push_str(&String::from_utf8_lossy(&bytes));

                // Prompts are not consistently newline-terminated and, in newer
                // unrar releases, can arrive on stderr. Inspect every chunk.
                if output_buf.contains("All OK") {
                    info!(set = %set_name, "Direct unpack complete — All OK");
                    return DirectUnpackResult {
                        set_name: set_name.to_string(),
                        success: true,
                        error: None,
                    };
                }

                if let Some(pattern) = UNRAR_ERROR_PATTERNS
                    .iter()
                    .find(|pattern| output_buf.contains(**pattern))
                {
                    error!(set = %set_name, error = %output_buf.trim(), "Direct unpack error detected");
                    return failed_result(set_name, &format!("unrar reported {pattern}"));
                }

                if is_volume_prompt(&output_buf) {
                    debug!(set = %set_name, next_volume, "Unrar requesting next volume");
                    match wait_for_volume(set_name, next_volume, state, volume_ready, killed, rt) {
                        Ok(()) => {
                            if let Err(e) = stdin.write_all(b"C\n") {
                                error!(error = %e, "Failed to write to unrar stdin");
                                return failed_result(set_name, &format!("stdin write error: {e}"));
                            }
                            let _ = stdin.flush();
                            next_volume += 1;
                            output_buf.clear();
                        }
                        Err(e) => {
                            let _ = stdin.write_all(b"Q\n");
                            let _ = stdin.flush();
                            return failed_result(set_name, &e);
                        }
                    }
                } else if output_buf.contains("[R]etry, [A]bort") {
                    warn!(set = %set_name, "Unrar retry prompt — aborting");
                    let _ = stdin.write_all(b"A\n");
                    let _ = stdin.flush();
                    return failed_result(set_name, "Unrar requested retry — aborted");
                } else if output_buf.len() > 8192 {
                    // Keep enough trailing output for a prompt split across reads
                    // without allowing noisy tool output to grow unbounded.
                    output_buf.drain(..4096);
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if let Ok(Some(status)) = child.try_wait() {
                    return result_from_exit_status(set_name, status);
                }
                if last_output.elapsed() >= DIRECT_UNPACK_IDLE_TIMEOUT {
                    let _ = child.kill();
                    return failed_result(
                        set_name,
                        "unrar stopped producing output and was terminated after five minutes",
                    );
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    // Process exited without "All OK". Check exit status.
    match child.wait() {
        Ok(status) => result_from_exit_status(set_name, status),
        Err(e) => failed_result(set_name, &format!("Failed to wait on unrar: {e}")),
    }
}

fn spawn_output_reader<R>(mut reader: R, sender: mpsc::Sender<Vec<u8>>)
where
    R: Read + Send + 'static,
{
    thread::spawn(move || {
        let mut buffer = [0_u8; 1024];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(count) if sender.send(buffer[..count].to_vec()).is_err() => break,
                Ok(_) => {}
            }
        }
    });
}

fn is_volume_prompt(output: &str) -> bool {
    let output = output.to_ascii_lowercase();
    (output.contains("[c]ontinue") && output.contains("[q]uit"))
        || (output.contains("insert disk")
            && (output.contains("continue") || output.contains("press c")))
        || (output.contains("next volume")
            && (output.contains("continue") || output.contains("press c")))
}

fn failed_result(set_name: &str, error: &str) -> DirectUnpackResult {
    DirectUnpackResult {
        set_name: set_name.to_string(),
        success: false,
        error: Some(error.to_string()),
    }
}

fn result_from_exit_status(set_name: &str, status: std::process::ExitStatus) -> DirectUnpackResult {
    if status.success() {
        DirectUnpackResult {
            set_name: set_name.to_string(),
            success: true,
            error: None,
        }
    } else {
        failed_result(set_name, &format!("unrar exited with status {status}"))
    }
}

/// Wait until the requested volume is available or determine it will never arrive.
fn wait_for_volume(
    set_name: &str,
    volume_number: u32,
    state: &Mutex<DirectUnpackState>,
    volume_ready: &Notify,
    killed: &AtomicBool,
    rt: &tokio::runtime::Handle,
) -> Result<(), String> {
    loop {
        if killed.load(Ordering::Acquire) {
            return Err("Aborted".to_string());
        }

        // Check if volume is available.
        {
            let st = state.lock();
            if let Some(set) = st.sets.get(set_name)
                && set.volumes.contains_key(&volume_number)
            {
                return Ok(());
            }

            // If download is finished and volume still not available, it's missing.
            if st.download_finished {
                return Err(format!(
                    "Volume {volume_number} of set '{set_name}' not available after download completed"
                ));
            }
        }

        // Wait for notification with a timeout. This handles the case where
        // the notification arrived between our check and the wait.
        let notified = volume_ready.notified();
        let timeout = std::time::Duration::from_secs(30);
        match rt.block_on(async { tokio::time::timeout(timeout, notified).await }) {
            Ok(()) => {} // Notified — re-check.
            Err(_) => {
                debug!(
                    set = %set_name,
                    volume = volume_number,
                    "Timeout waiting for volume — retrying"
                );
                // Timeout but download not finished — keep waiting.
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_error_patterns_are_non_empty() {
        for pattern in UNRAR_ERROR_PATTERNS {
            assert!(!pattern.is_empty());
        }
    }

    #[test]
    fn volume_prompts_are_recognised_across_unrar_versions() {
        assert!(is_volume_prompt(
            "Insert disk with part002.rar [C]ontinue, [Q]uit"
        ));
        assert!(is_volume_prompt(
            "Insert disk with next volume. Press C to continue or Q to quit"
        ));
        assert!(is_volume_prompt(
            "Next volume is required; press C to continue"
        ));
    }

    #[test]
    fn normal_output_is_not_mistaken_for_a_volume_prompt() {
        assert!(!is_volume_prompt("Extracting video.mkv"));
        assert!(!is_volume_prompt("Continue downloading articles"));
        assert!(!is_volume_prompt("Press C to cancel"));
    }

    #[cfg(unix)]
    fn fake_unrar(script: &str) -> (tempfile::TempDir, PathBuf) {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("unrar");
        std::fs::write(&path, script).unwrap();
        let mut permissions = std::fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&path, permissions).unwrap();
        (dir, path)
    }

    #[cfg(unix)]
    #[test]
    fn direct_unpack_continues_when_modern_prompt_is_written_to_stderr() {
        let (script_dir, unrar) = fake_unrar(
            "#!/bin/sh\nprintf 'Insert disk with next volume. Press C to continue or Q to quit' >&2\nIFS= read -r answer\n[ \"$answer\" = C ] || exit 9\nprintf 'All OK\\n'\n",
        );
        let output_dir = tempfile::tempdir().unwrap();
        let first_volume = script_dir.path().join("movie.part001.rar");
        std::fs::write(&first_volume, b"first").unwrap();
        let second_volume = script_dir.path().join("movie.part002.rar");
        std::fs::write(&second_volume, b"second").unwrap();
        let state = Mutex::new(DirectUnpackState {
            sets: BTreeMap::from([(
                "movie".to_string(),
                RarSetState {
                    set_name: "movie".to_string(),
                    volumes: BTreeMap::from([(0, first_volume.clone()), (1, second_volume)]),
                },
            )]),
            download_finished: false,
        });
        let notify = Notify::new();
        let killed = AtomicBool::new(false);
        let runtime = tokio::runtime::Runtime::new().unwrap();

        let result = unpack_set(
            unrar.to_str().unwrap(),
            "movie",
            &first_volume,
            output_dir.path(),
            None,
            &state,
            &notify,
            &killed,
            runtime.handle(),
        );

        assert!(result.success, "{result:?}");
    }

    #[cfg(unix)]
    #[test]
    fn direct_unpack_fails_on_error_written_to_stderr() {
        let (script_dir, unrar) =
            fake_unrar("#!/bin/sh\nprintf 'Cannot open archive\\n' >&2\nexit 2\n");
        let output_dir = tempfile::tempdir().unwrap();
        let first_volume = script_dir.path().join("movie.part001.rar");
        std::fs::write(&first_volume, b"first").unwrap();
        let state = Mutex::new(DirectUnpackState {
            sets: BTreeMap::new(),
            download_finished: true,
        });
        let notify = Notify::new();
        let killed = AtomicBool::new(false);
        let runtime = tokio::runtime::Runtime::new().unwrap();

        let result = unpack_set(
            unrar.to_str().unwrap(),
            "movie",
            &first_volume,
            output_dir.path(),
            None,
            &state,
            &notify,
            &killed,
            runtime.handle(),
        );

        assert!(!result.success);
        assert_eq!(result.error.as_deref(), Some("unrar reported Cannot open"));
    }

    #[test]
    fn test_direct_unpack_state_basics() {
        let state = DirectUnpackState {
            sets: BTreeMap::new(),
            download_finished: false,
        };
        assert!(state.sets.is_empty());
        assert!(!state.download_finished);
    }

    #[test]
    fn test_rar_set_state_volume_tracking() {
        let mut set = RarSetState {
            set_name: "movie".to_string(),
            volumes: BTreeMap::new(),
        };

        set.volumes
            .insert(0, PathBuf::from("/tmp/movie.part001.rar"));
        set.volumes
            .insert(1, PathBuf::from("/tmp/movie.part002.rar"));

        assert!(set.volumes.contains_key(&0));
        assert!(set.volumes.contains_key(&1));
        assert!(!set.volumes.contains_key(&2));
    }

    #[tokio::test]
    async fn test_direct_unpacker_no_unrar() {
        // If unrar is not found, new() returns None.
        // We can't easily test this without mocking PATH, but we can verify
        // the constructor doesn't panic.
        let work_dir = tempfile::tempdir().unwrap();
        let output_dir = tempfile::tempdir().unwrap();
        // This will return Some if unrar is installed, None otherwise.
        // Either way, it shouldn't panic.
        let _du = DirectUnpacker::new(work_dir.path(), output_dir.path(), None);
    }

    #[tokio::test]
    async fn test_direct_unpacker_abort() {
        let work_dir = tempfile::tempdir().unwrap();
        let output_dir = tempfile::tempdir().unwrap();

        if let Some(du) = DirectUnpacker::new(work_dir.path(), output_dir.path(), None) {
            // Abort immediately — should complete without hanging.
            du.abort();
            let results = du.finish().await;
            // Should get empty or aborted results.
            for r in &results {
                assert!(!r.success);
            }
        }
    }
}
