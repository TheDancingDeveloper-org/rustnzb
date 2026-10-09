use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use notify::{Event, EventKind, RecursiveMode, Watcher};
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use crate::nzb_archive::extract_nzbs;
use crate::queue_manager::QueueManager;

const MAX_WATCHED_NZB_BYTES: usize = 100 * 1024 * 1024;
/// Interval between the size checks that decide a dropped file is complete.
const SETTLE_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);
/// Give up waiting for a file that never stops changing and process it anyway.
const MAX_SETTLE_WAIT: std::time::Duration = std::time::Duration::from_secs(600);
/// Suffix of a file the watcher has claimed. Claimed files are never picked
/// up again, so an NZB is enqueued at most once even if the final move fails.
const CLAIM_SUFFIX: &str = ".processing";

pub struct DirWatcher {
    watch_dir: PathBuf,
    queue_manager: Arc<QueueManager>,
}

impl DirWatcher {
    pub fn new(watch_dir: PathBuf, queue_manager: Arc<QueueManager>) -> Self {
        Self {
            watch_dir,
            queue_manager,
        }
    }

    pub async fn run(self) {
        info!(dir = %self.watch_dir.display(), "Starting directory watcher");

        // Create the watch directory if it doesn't exist
        if let Err(e) = std::fs::create_dir_all(&self.watch_dir) {
            error!(error = %e, "Failed to create watch directory");
            return;
        }

        // Process any existing .nzb files first
        self.process_existing_files().await;

        // Set up file watcher
        let (tx, mut rx) = mpsc::channel(100);

        let _watcher = {
            let tx = tx.clone();
            let mut watcher =
                notify::recommended_watcher(move |res: Result<Event, notify::Error>| {
                    if let Ok(event) = res {
                        let _ = tx.blocking_send(event);
                    }
                })
                .expect("Failed to create file watcher");

            watcher
                .watch(&self.watch_dir, RecursiveMode::NonRecursive)
                .expect("Failed to watch directory");
            watcher // keep alive
        };

        // Process events
        while let Some(event) = rx.recv().await {
            match event.kind {
                EventKind::Create(_) | EventKind::Modify(_) => {
                    for path in &event.paths {
                        if Self::is_nzb_file(path) {
                            self.process_file(path).await;
                        }
                    }
                }
                _ => {}
            }
        }
    }

    /// Watched names: `.nzb`, plus `.nzb.gz`, `.nzb.bz2`, and `.zip` archives
    /// of NZBs. Matching is case-sensitive, as it always has been here.
    fn is_nzb_file(path: &Path) -> bool {
        path.file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| {
                [".nzb", ".nzb.gz", ".nzb.bz2", ".zip"]
                    .iter()
                    .any(|suffix| name.ends_with(suffix))
            })
    }

    async fn process_existing_files(&self) {
        let entries = match std::fs::read_dir(&self.watch_dir) {
            Ok(e) => e,
            Err(e) => {
                warn!(error = %e, "Failed to read watch directory");
                return;
            }
        };

        for entry in entries.flatten() {
            let path = entry.path();
            if Self::is_nzb_file(&path) {
                self.process_file(&path).await;
            } else if path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with(CLAIM_SUFFIX))
            {
                warn!(
                    file = %path.display(),
                    "Found a claimed NZB left by an interrupted run; it may already be queued, so it is not imported again. Rename it to retry."
                );
            }
        }
    }

    /// Wait until `path` stops changing: its size and modification time
    /// must be identical across two checks `interval` apart. Returns `false`
    /// if the file disappeared.
    async fn wait_until_stable(path: &Path, interval: std::time::Duration) -> bool {
        let snapshot = |path: &Path| {
            std::fs::symlink_metadata(path)
                .ok()
                .map(|m| (m.len(), m.modified().ok()))
        };
        let started = tokio::time::Instant::now();
        let Some(mut previous) = snapshot(path) else {
            return false;
        };
        loop {
            tokio::time::sleep(interval).await;
            let Some(current) = snapshot(path) else {
                return false;
            };
            if current == previous {
                return true;
            }
            if started.elapsed() >= MAX_SETTLE_WAIT {
                warn!(file = %path.display(), "Watched file is still changing; processing it anyway");
                return true;
            }
            previous = current;
        }
    }

    /// Move `from` into `<watch_dir>/<subdir>/<file_name>`.
    fn move_into(
        &self,
        from: &Path,
        subdir: &str,
        file_name: &std::ffi::OsStr,
    ) -> std::io::Result<()> {
        let dir = self.watch_dir.join(subdir);
        std::fs::create_dir_all(&dir)?;
        let dest = dir.join(file_name);
        if std::fs::rename(from, &dest).is_err() {
            // If rename fails (cross-device), try copy+delete
            std::fs::copy(from, &dest).and_then(|_| std::fs::remove_file(from))?;
        }
        Ok(())
    }

    async fn process_file(&self, path: &Path) {
        // Several events can fire for one file while it is being written; a
        // later one finds it already claimed and gone.
        if !Self::wait_until_stable(path, SETTLE_INTERVAL).await {
            debug!(file = %path.display(), "Watched file vanished before it settled");
            return;
        }
        let Some(file_name) = path.file_name().map(|name| name.to_os_string()) else {
            return;
        };

        // Claim the file before enqueueing so that, whatever happens next,
        // it is never imported a second time.
        let mut claimed_name = file_name.clone();
        claimed_name.push(CLAIM_SUFFIX);
        let claimed = path.with_file_name(&claimed_name);
        if let Err(e) = std::fs::rename(path, &claimed) {
            if e.kind() != std::io::ErrorKind::NotFound {
                warn!(error = %e, file = %path.display(), "Failed to claim watched NZB");
            }
            return;
        }

        info!(file = %path.display(), "Processing NZB from watch directory");
        let destination = match self.enqueue_claimed(&claimed, &file_name).await {
            Ok(()) => "processed",
            Err(e) => {
                warn!(error = %e, file = %path.display(), "Failed to import NZB from watch dir; moving it to failed/");
                "failed"
            }
        };
        if let Err(e) = self.move_into(&claimed, destination, &file_name) {
            warn!(
                error = %e,
                file = %claimed.display(),
                "Failed to move claimed NZB to {destination}/; it stays claimed and will not be imported again"
            );
        }
    }

    /// Read, unpack and enqueue the claimed copy. One watched file can hold
    /// several NZBs (a zip of them), so every member is enqueued and any
    /// failure moves the whole file to failed/.
    async fn enqueue_claimed(
        &self,
        claimed: &Path,
        file_name: &std::ffi::OsStr,
    ) -> Result<(), String> {
        let raw_data = Self::read_limited(claimed).map_err(|e| format!("read failed: {e}"))?;
        let file_name_str = file_name.to_str().unwrap_or("unknown.nzb");
        let nzbs =
            extract_nzbs(file_name_str, &raw_data).map_err(|e| format!("unpack failed: {e}"))?;
        if nzbs.is_empty() {
            return Err("archive contains no NZBs".into());
        }
        for (nzb_name, data) in nzbs {
            // Archive entries may carry directories; the job is named after
            // the NZB itself, without its extension.
            let base = nzb_name.rsplit(['/', '\\']).next().unwrap_or(&nzb_name);
            // SABnzbd's `name{{password}}` convention: split the password
            // off before parse_nzb sanitizes the name.
            let (base, password) = match crate::nzb_core::path::split_job_password(base) {
                Some((name, password)) => (name, Some(password).filter(|pw| !pw.is_empty())),
                None => (base.to_string(), None),
            };
            let name = base.strip_suffix(".nzb").unwrap_or(&base);
            let name = if name.is_empty() { "unknown" } else { name };
            let mut job = crate::nzb_core::nzb_parser::parse_nzb(name, &data)
                .map_err(|e| format!("parse failed for {nzb_name}: {e}"))?;
            if password.is_some() {
                job.password = password;
            }
            job.work_dir = self.queue_manager.incomplete_dir().join(&job.id);
            job.output_dir = self.queue_manager.complete_dir().join(&job.name);
            std::fs::create_dir_all(&job.work_dir)
                .map_err(|e| format!("failed to create work directory: {e}"))?;
            info!(name = %job.name, id = %job.id, "Auto-enqueuing NZB from watch dir");
            self.queue_manager
                .add_job(job, Some(data))
                .map_err(|e| format!("enqueue failed: {e}"))?;
        }
        Ok(())
    }

    fn read_limited(path: &Path) -> std::io::Result<Vec<u8>> {
        let metadata = std::fs::symlink_metadata(path)?;
        if metadata.file_type().is_symlink() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "watched NZB symlinks are not supported",
            ));
        }
        if metadata.len() > MAX_WATCHED_NZB_BYTES as u64 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::FileTooLarge,
                format!("watched NZB exceeds the {MAX_WATCHED_NZB_BYTES} byte limit"),
            ));
        }

        let file = std::fs::File::open(path)?;
        let mut data = Vec::new();
        file.take((MAX_WATCHED_NZB_BYTES as u64).saturating_add(1))
            .read_to_end(&mut data)?;
        if data.len() > MAX_WATCHED_NZB_BYTES {
            return Err(std::io::Error::new(
                std::io::ErrorKind::FileTooLarge,
                format!("watched NZB exceeds the {MAX_WATCHED_NZB_BYTES} byte limit"),
            ));
        }
        Ok(data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_plain_and_compressed_nzb_paths_case_sensitively() {
        assert!(DirWatcher::is_nzb_file(Path::new("release.nzb")));
        assert!(DirWatcher::is_nzb_file(Path::new("release.nzb.gz")));
        assert!(DirWatcher::is_nzb_file(Path::new("release.nzb.bz2")));
        assert!(DirWatcher::is_nzb_file(Path::new("bundle.zip")));
        assert!(!DirWatcher::is_nzb_file(Path::new("release.gz")));
        assert!(!DirWatcher::is_nzb_file(Path::new("release.NZB")));
        assert!(!DirWatcher::is_nzb_file(Path::new("release.txt")));
    }

    #[tokio::test]
    async fn waits_until_a_growing_file_stops_changing() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("growing.nzb");
        std::fs::write(&path, b"a").unwrap();

        let writer_path = path.clone();
        let writer = tokio::spawn(async move {
            for _ in 0..8 {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                let mut file = std::fs::OpenOptions::new()
                    .append(true)
                    .open(&writer_path)
                    .unwrap();
                std::io::Write::write_all(&mut file, b"a").unwrap();
            }
        });

        let interval = std::time::Duration::from_millis(150);
        assert!(DirWatcher::wait_until_stable(&path, interval).await);
        writer.await.unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 9);
    }

    #[tokio::test]
    async fn settle_wait_reports_a_vanished_file() {
        let temp = tempfile::tempdir().unwrap();
        let interval = std::time::Duration::from_millis(10);
        assert!(!DirWatcher::wait_until_stable(&temp.path().join("gone.nzb"), interval).await);
    }

    #[test]
    fn claimed_files_are_not_watched() {
        assert!(!DirWatcher::is_nzb_file(Path::new(
            "release.nzb.processing"
        )));
        assert!(!DirWatcher::is_nzb_file(Path::new(
            "release.nzb.gz.processing"
        )));
    }

    #[test]
    fn bounded_file_reader_rejects_oversized_input() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("oversized.nzb");
        std::fs::write(&path, vec![b'x'; MAX_WATCHED_NZB_BYTES + 1]).unwrap();
        let error = DirWatcher::read_limited(&path).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::FileTooLarge);
    }
}
