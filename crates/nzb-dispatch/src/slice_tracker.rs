//! Per-job in-flight PAR2 slice verification (WI-1228).
//!
//! The download engine owns one [`JobSlices`] per job. Every successful
//! article write is reported to it with [`JobSlices::record_write`]. Until the
//! job's PAR2 index has been downloaded and parsed, writes are only buffered.
//! When the index completes, [`JobSlices::install`] parses it, maps NZB files
//! to PAR2 files, builds a [`SliceVerifier`], and replays the buffered writes,
//! so data that arrived before the index is still verified. From then on each
//! write marks its bytes, and every slice that becomes fully written is read
//! back from disk and checked against its PAR2 checksums.
//!
//! When the job ends, [`JobSlices::summary`] decides the slices that are still
//! `Unknown`. A gap in a file whose failed articles were all refused by every
//! server is confirmed missing, so its unfilled slices are `Damaged`. A gap in
//! a file with an unconfirmed failure (a timeout, a local write error) stays
//! `Unknown`. Files that already had bytes on disk when the job was submitted
//! (a resumed job) are never tracked; their slices stay `Unknown`.
//!
//! Parsing, read-back and hashing all run on blocking tasks, and each worker
//! awaits its own verification before it resolves the article, so the map is
//! complete before `JobFinished` is emitted.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use nzb_core::models::{NzbJob, SliceMapSummary, SliceVerdict};
use nzb_core::{CandidateFile, Par2FileRef, match_files_by_hash16k};
use nzb_decode::{SliceState, SliceVerifier};
use parking_lot::Mutex;
use tracing::{debug, info, warn};

use crate::slice_layout::slice_layout_from_par2;

/// One file write the verifier has to account for.
#[derive(Debug, Clone, Copy)]
struct Write {
    offset: u64,
    len: u64,
}

/// Where a slice's bytes live on disk, for read-back.
#[derive(Debug)]
pub(crate) struct PendingSlice {
    global_index: usize,
    path: PathBuf,
    offset: u64,
    len: u64,
}

struct Tracked {
    verifier: SliceVerifier,
    /// NZB file id → (layout file index, path the assembler writes).
    files: HashMap<String, (usize, PathBuf)>,
    /// Layout file indices that are not tracked (bytes already on disk at
    /// submission). Their slices are never decided here.
    untracked: HashSet<usize>,
    recovery_block_count: u32,
}

enum State {
    /// The job has no PAR2 index file, or tracking failed; nothing is done.
    Disabled,
    /// Waiting for the index. Writes are buffered per NZB file id.
    Pending {
        writes: HashMap<String, Vec<Write>>,
        installing: bool,
    },
    Ready(Box<Tracked>),
}

/// The per-job slice tracker. Cheap when disabled: one uncontended lock per
/// article.
pub(crate) struct JobSlices {
    work_dir: PathBuf,
    /// NZB file id → NZB filename (the name the assembler writes under).
    filenames: HashMap<String, String>,
    /// NZB files that already had downloaded articles at submission.
    resumed: HashSet<String>,
    state: Mutex<State>,
    /// NZB file id → whether every terminal failure in it was a confirmed
    /// absence (refused by every server). Absent: no failures.
    failures: Mutex<HashMap<String, bool>>,
}

/// True for a PAR2 index file name (`.par2` without `.vol`).
pub(crate) fn is_par2_index(filename: &str) -> bool {
    let lower = filename.to_lowercase();
    lower.ends_with(".par2") && !lower.contains(".vol")
}

impl JobSlices {
    pub(crate) fn new(job: &NzbJob) -> Self {
        let has_index = job.files.iter().any(|f| is_par2_index(&f.filename));
        let state = if has_index {
            State::Pending {
                writes: HashMap::new(),
                installing: false,
            }
        } else {
            State::Disabled
        };
        Self {
            work_dir: job.work_dir.clone(),
            filenames: job
                .files
                .iter()
                .map(|f| (f.id.clone(), f.filename.clone()))
                .collect(),
            resumed: job
                .files
                .iter()
                .filter(|f| f.articles.iter().any(|a| a.downloaded))
                .map(|f| f.id.clone())
                .collect(),
            state: Mutex::new(state),
            failures: Mutex::new(HashMap::new()),
        }
    }

    /// Record a successful write of `len` bytes at `offset` into `file_id`.
    /// Returns the slices that became fully written and must be verified.
    fn mark(&self, file_id: &str, offset: u64, len: u64) -> Vec<PendingSlice> {
        let mut state = self.state.lock();
        match &mut *state {
            State::Disabled => Vec::new(),
            State::Pending { writes, .. } => {
                writes
                    .entry(file_id.to_string())
                    .or_default()
                    .push(Write { offset, len });
                Vec::new()
            }
            State::Ready(tracked) => {
                let Some((index, path)) = tracked.files.get(file_id) else {
                    return Vec::new();
                };
                if tracked.untracked.contains(index) {
                    return Vec::new();
                }
                let (index, path) = (*index, path.clone());
                let done = tracked.verifier.mark_written(index, offset, len);
                pending_for(&tracked.verifier, &path, done)
            }
        }
    }

    /// Record a write and verify any slices it completes. If the write
    /// completed the PAR2 index (`file_complete` on an index file), install the
    /// verifier first.
    pub(crate) async fn record_write(
        self: &std::sync::Arc<Self>,
        file_id: &str,
        offset: u64,
        len: u64,
        file_complete: bool,
    ) {
        let mut pending = self.mark(file_id, offset, len);
        if file_complete
            && self
                .filenames
                .get(file_id)
                .is_some_and(|name| is_par2_index(name))
            && self.claim_install()
        {
            let this = self.clone();
            let file_id = file_id.to_string();
            match tokio::task::spawn_blocking(move || this.install(&file_id)).await {
                Ok(more) => pending.extend(more),
                Err(e) => {
                    warn!("PAR2 slice map install task failed: {e}");
                    *self.state.lock() = State::Disabled;
                }
            }
        }
        if pending.is_empty() {
            return;
        }
        let this = self.clone();
        if let Err(e) = tokio::task::spawn_blocking(move || this.verify_all(pending)).await {
            warn!("PAR2 slice verify task failed: {e}");
        }
    }

    /// Note a terminal article failure in `file_id`. `confirmed` is true when
    /// every server refused the article.
    pub(crate) fn record_failure(&self, file_id: &str, confirmed: bool) {
        let mut failures = self.failures.lock();
        let entry = failures.entry(file_id.to_string()).or_insert(true);
        *entry &= confirmed;
    }

    /// Take the right to install. Only the first completed index wins.
    fn claim_install(&self) -> bool {
        match &mut *self.state.lock() {
            State::Pending { installing, .. } if !*installing => {
                *installing = true;
                true
            }
            _ => false,
        }
    }

    /// Parse the index, build the verifier, and replay buffered writes.
    /// Returns the slices the replay completed. Runs on a blocking task.
    fn install(&self, index_file_id: &str) -> Vec<PendingSlice> {
        let Some(index_name) = self.filenames.get(index_file_id) else {
            *self.state.lock() = State::Disabled;
            return Vec::new();
        };
        let index_path = self.work_dir.join(index_name);
        let set = match rust_par2::parse(&index_path) {
            Ok(set) => set,
            Err(e) => {
                warn!(path = %index_path.display(), "PAR2 index unreadable, no in-flight slice map: {e}");
                *self.state.lock() = State::Disabled;
                return Vec::new();
            }
        };
        let layout = slice_layout_from_par2(&set);

        // Layout file index for each PAR2 file id, in Main-packet order (the
        // same filter `slice_layout_from_par2` applies).
        let par2_refs: Vec<Par2FileRef<'_, usize>> = set
            .file_order
            .iter()
            .filter_map(|id| set.files.get(id))
            .enumerate()
            .map(|(i, f)| Par2FileRef {
                id: i,
                name: &f.filename,
                hash_16k: f.hash_16k,
                size: f.size,
            })
            .collect();
        let candidates: Vec<CandidateFile<'_, String>> = self
            .filenames
            .iter()
            .filter(|(_, name)| !name.to_lowercase().ends_with(".par2"))
            .map(|(id, name)| CandidateFile {
                id: id.clone(),
                name,
                first_16k_md5: None,
                size: None,
            })
            .collect();
        let matched = match_files_by_hash16k(&par2_refs, &candidates);

        let mut files = HashMap::new();
        let mut untracked = HashSet::new();
        for (file_id, index) in &matched {
            files.insert(
                file_id.clone(),
                (*index, self.work_dir.join(&self.filenames[file_id])),
            );
            if self.resumed.contains(file_id) {
                untracked.insert(*index);
            }
        }
        // A PAR2 file no NZB file maps to can never be written.
        let unmatched = (0..layout.files.len()).filter(|i| !matched.values().any(|m| m == i));
        untracked.extend(unmatched);

        let mut tracked = Box::new(Tracked {
            verifier: SliceVerifier::new(&layout),
            files,
            untracked,
            recovery_block_count: set.recovery_block_count,
        });
        info!(
            slices = tracked.verifier.total_slices(),
            matched_files = matched.len(),
            par2_files = layout.files.len(),
            "In-flight PAR2 slice map installed"
        );

        let mut state = self.state.lock();
        let buffered = match std::mem::replace(&mut *state, State::Disabled) {
            State::Pending { writes, .. } => writes,
            _ => HashMap::new(),
        };
        let mut pending = Vec::new();
        for (file_id, writes) in buffered {
            let Some((index, path)) = tracked.files.get(&file_id) else {
                continue;
            };
            if tracked.untracked.contains(index) {
                continue;
            }
            let (index, path) = (*index, path.clone());
            for w in writes {
                let done = tracked.verifier.mark_written(index, w.offset, w.len);
                pending.extend(pending_for(&tracked.verifier, &path, done));
            }
        }
        *state = State::Ready(tracked);
        pending
    }

    /// Read each slice back and verify it. Runs on a blocking task.
    fn verify_all(&self, pending: Vec<PendingSlice>) {
        for slice in pending {
            let bytes = match read_range(&slice.path, slice.offset, slice.len) {
                Ok(bytes) => bytes,
                Err(e) => {
                    warn!(path = %slice.path.display(), slice = slice.global_index, "Slice read-back failed, left unknown: {e}");
                    continue;
                }
            };
            if let State::Ready(tracked) = &mut *self.state.lock() {
                let verdict = tracked.verifier.verify(slice.global_index, &bytes);
                if verdict == SliceState::Damaged {
                    debug!(slice = slice.global_index, "PAR2 slice damaged");
                }
            }
        }
    }

    /// Decide the remaining slices and summarise the map. `None` when no map
    /// was ever built. Called once, when the job's articles are all resolved.
    pub(crate) fn summary(&self) -> Option<SliceMapSummary> {
        let mut state = self.state.lock();
        let State::Ready(tracked) = &mut *state else {
            return None;
        };
        let failures = self.failures.lock();
        // Files whose every gap is a confirmed absence: unfilled slices there
        // are holes. A file with no failure has no gap at all, so any slice
        // still unfilled in it would be a bookkeeping error; leave it unknown.
        let confirmed: HashSet<usize> = tracked
            .files
            .iter()
            .filter(|(id, (index, _))| {
                failures.get(*id) == Some(&true) && !tracked.untracked.contains(index)
            })
            .map(|(_, (index, _))| *index)
            .collect();
        let states = tracked.verifier.states();
        for (gi, s) in states.iter().enumerate() {
            if *s != SliceState::Unknown {
                continue;
            }
            if let Some((index, ..)) = tracked.verifier.slice_range(gi)
                && confirmed.contains(&index)
            {
                tracked.verifier.mark_damaged(gi);
            }
        }
        let states: Vec<SliceVerdict> = tracked
            .verifier
            .states()
            .into_iter()
            .map(|s| match s {
                SliceState::Unknown => SliceVerdict::Unknown,
                SliceState::Verified => SliceVerdict::Verified,
                SliceState::Damaged => SliceVerdict::Damaged,
            })
            .collect();
        let damaged = states
            .iter()
            .filter(|s| **s == SliceVerdict::Damaged)
            .count();
        let unknown = states
            .iter()
            .filter(|s| **s == SliceVerdict::Unknown)
            .count();
        Some(SliceMapSummary {
            states,
            recovery_block_count: tracked.recovery_block_count,
            damaged,
            unknown,
        })
    }
}

fn pending_for(verifier: &SliceVerifier, path: &Path, done: Vec<usize>) -> Vec<PendingSlice> {
    done.into_iter()
        .filter_map(|gi| {
            verifier
                .slice_range(gi)
                .map(|(_, offset, len)| PendingSlice {
                    global_index: gi,
                    path: path.to_path_buf(),
                    offset,
                    len,
                })
        })
        .collect()
}

fn read_range(path: &Path, offset: u64, len: u64) -> std::io::Result<Vec<u8>> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(path)?;
    file.seek(SeekFrom::Start(offset))?;
    let mut buf = vec![0u8; len as usize];
    file.read_exact(&mut buf)?;
    Ok(buf)
}
