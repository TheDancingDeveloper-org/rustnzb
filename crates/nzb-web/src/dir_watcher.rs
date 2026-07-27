use std::path::{Path, PathBuf};
use std::sync::Arc;

use notify::{Event, EventKind, RecursiveMode, Watcher};
use tokio::sync::mpsc;
use tracing::{error, info, warn};

use crate::queue_manager::QueueManager;

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
                            // Small delay to ensure file is fully written
                            tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
                            self.process_file(path).await;
                        }
                    }
                }
                _ => {}
            }
        }
    }

    fn is_nzb_file(path: &Path) -> bool {
        path.extension().is_some_and(|ext| ext == "nzb")
            || path.to_str().is_some_and(|s| s.ends_with(".nzb.gz"))
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
            }
        }
    }

    async fn process_file(&self, path: &Path) {
        info!(file = %path.display(), "Processing NZB from watch directory");

        let data = match std::fs::read(path) {
            Ok(d) => d,
            Err(e) => {
                warn!(error = %e, file = %path.display(), "Failed to read NZB file");
                return;
            }
        };

        let name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown")
            .to_string();

        match crate::nzb_core::nzb_parser::parse_nzb(&name, &data) {
            Ok(mut job) => {
                job.work_dir = self.queue_manager.incomplete_dir().join(&job.id);
                job.output_dir = self.queue_manager.complete_dir().join(&job.name);

                if let Err(e) = std::fs::create_dir_all(&job.work_dir) {
                    error!(error = %e, "Failed to create work directory");
                    return;
                }

                info!(name = %job.name, id = %job.id, "Auto-enqueuing NZB from watch dir");

                if let Err(e) = self.queue_manager.add_job(job, Some(data)) {
                    error!(error = %e, "Failed to enqueue NZB");
                    return;
                }

                // Move processed file to avoid re-processing
                let processed_dir = self.watch_dir.join("processed");
                let _ = std::fs::create_dir_all(&processed_dir);
                let dest = processed_dir.join(path.file_name().unwrap_or_default());
                if let Err(_e) = std::fs::rename(path, &dest) {
                    // If rename fails (cross-device), try copy+delete
                    if let Err(e2) =
                        std::fs::copy(path, &dest).and_then(|_| std::fs::remove_file(path))
                    {
                        warn!(error = %e2, "Failed to move processed NZB file");
                    }
                }
            }
            Err(e) => {
                warn!(error = %e, file = %path.display(), "Failed to parse NZB from watch dir");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_plain_and_gzipped_nzb_paths_case_sensitively() {
        assert!(DirWatcher::is_nzb_file(Path::new("release.nzb")));
        assert!(DirWatcher::is_nzb_file(Path::new("release.nzb.gz")));
        assert!(!DirWatcher::is_nzb_file(Path::new("release.NZB")));
        assert!(!DirWatcher::is_nzb_file(Path::new("release.txt")));
    }
}
