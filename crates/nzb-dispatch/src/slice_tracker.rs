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
//! `Unknown`.
//!
//! WI-1229 extends this to the cases the first cut left `Unknown`:
//!
//! - **Late index.** When the index lands after data files have completed, the
//!   replay reads those files' slices back from disk once, on the blocking task
//!   of the worker that completed the index, while the other workers keep the
//!   network busy.
//! - **Restart.** A resumed file's already-written segments carry their decoded
//!   offsets (`data_begin`, `data_size`), so they are seeded as writes and their
//!   slices are verified from disk once. A resumed file with a downloaded
//!   segment of unknown offset stays untracked. If the index itself finished
//!   before the restart, the verifier installs on the first write.
//! - **Obfuscated names.** An NZB file whose name matches no PAR2 file is
//!   matched by the MD5 of its first 16 KiB once those bytes have landed, then
//!   its buffered writes are replayed. A file that never matches leaves its
//!   slices `Unknown`.
//!
//! WI-148 adds the repair fold. Once damage is first seen, the tracker opens a
//! [`RepairFold`] with rows sized to the damage (bounded by the fold's memory
//! budget and the set's recovery count), folds the slices already verified
//! from disk once, in bounded batches with the fold lock released between
//! batches so workers keep feeding, and from then on folds every slice it
//! verifies straight from the bytes it already read. When damage outgrows the
//! rows, the rows are doubled and the fed slices are re-read for the new rows
//! only. Post-processing takes the fold from `JobFinished` and only has the
//! D×D solve and the writes left.
//!
//! Parsing, read-back and hashing all run on blocking tasks, and each worker
//! awaits its own verification before it resolves the article, so the map is
//! complete before `JobFinished` is emitted.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use md5::{Digest, Md5};
use nzb_core::models::{NzbJob, SliceMapSummary, SliceVerdict};
use nzb_core::{CandidateFile, PAR2_HASH_16K_LEN, Par2FileRef, match_files_by_hash16k};
use nzb_decode::repair_fold::{GrowthTicket, batch_len, max_rows};
use nzb_decode::{RepairFold, SharedFold, SliceState, SliceVerifier};
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
    /// Every layout file's `hash_16k` and size, by layout index.
    par2_files: Vec<([u8; 16], u64)>,
    /// Layout indices already mapped to an NZB file.
    claimed: HashSet<usize>,
    /// NZB data files not yet mapped to a PAR2 file, with their writes so far.
    unmatched: HashMap<String, Vec<Write>>,
    /// (NZB file id, prefix length) pairs already hashed, so each prefix is
    /// read from disk at most once.
    hashed: HashSet<(String, u64)>,
}

/// What one write asks of the caller beyond verifying `pending`.
struct Marked {
    pending: Vec<PendingSlice>,
    /// The write landed in the first 16 KiB of an unmatched file; try the
    /// hash matcher.
    try_match: bool,
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

/// The repair fold's lifecycle within one job.
enum FoldSlot {
    /// No damage seen yet.
    Idle,
    /// The set cannot be folded (slice size, budget); post-processing uses
    /// the disk path.
    Refused,
    Active(SharedFold),
}

/// A tracked set's shape, for sizing the fold.
struct Plan {
    slice_size: u64,
    total: usize,
    recovery: u32,
}

/// The recovery exponents a job's volumes carry, from their names.
fn recovery_exponents(job: &NzbJob) -> Vec<u32> {
    let mut exps: Vec<u32> = job
        .files
        .iter()
        .filter_map(|f| Some((f.par2_vol?, f.par2_blocks?)))
        .flat_map(|(vol, blocks)| vol..vol.saturating_add(blocks))
        .collect();
    exps.sort_unstable();
    exps.dedup();
    exps
}

/// Rows to open for `damage` known damaged slices: twice the damage, at least
/// eight, within the budget and the recovery count.
fn rows_for(damage: usize, recovery: u32, slice_size: u64) -> usize {
    (damage * 2)
        .max(8)
        .min(recovery as usize)
        .min(max_rows(slice_size))
}

/// The per-job slice tracker. Cheap when disabled: one uncontended lock per
/// article.
pub(crate) struct JobSlices {
    work_dir: PathBuf,
    /// NZB file id → NZB filename (the name the assembler writes under).
    filenames: HashMap<String, String>,
    /// NZB files resumed with a downloaded segment of unknown offset. They are
    /// never tracked.
    resumed: HashSet<String>,
    /// The index file, when it was already complete at submission.
    index_on_disk: Option<String>,
    state: Mutex<State>,
    /// NZB file id → whether every terminal failure in it was a confirmed
    /// absence (refused by every server). Absent: no failures.
    failures: Mutex<HashMap<String, bool>>,
    /// Articles confirmed missing everywhere so far.
    confirmed_missing: AtomicUsize,
    /// Slices that failed their checksum so far.
    damaged_seen: AtomicUsize,
    /// The in-flight repair fold (WI-148).
    fold: Mutex<FoldSlot>,
    /// Recovery exponents the NZB's volumes carry, ascending (from their
    /// `.volNN+MM` names). Empty when the names do not say.
    recovery_exponents: Vec<u32>,
}

/// True for a PAR2 index file name (`.par2` without `.vol`).
pub(crate) fn is_par2_index(filename: &str) -> bool {
    let lower = filename.to_lowercase();
    lower.ends_with(".par2") && !lower.contains(".vol")
}

impl JobSlices {
    pub(crate) fn new(job: &NzbJob) -> Self {
        let has_index = job.files.iter().any(|f| is_par2_index(&f.filename));
        // Seed the writes a resumed job made before the restart.
        let mut seeded: HashMap<String, Vec<Write>> = HashMap::new();
        let mut resumed = HashSet::new();
        for file in &job.files {
            let done: Vec<_> = file.articles.iter().filter(|a| a.downloaded).collect();
            if done.is_empty() {
                continue;
            }
            let writes: Option<Vec<Write>> = done
                .iter()
                .map(|a| {
                    Some(Write {
                        offset: a.data_begin?,
                        len: a.data_size?,
                    })
                })
                .collect();
            match writes {
                Some(writes) => {
                    seeded.insert(file.id.clone(), writes);
                }
                None => {
                    resumed.insert(file.id.clone());
                }
            }
        }
        let index_on_disk = job
            .files
            .iter()
            .find(|f| {
                is_par2_index(&f.filename)
                    && !f.articles.is_empty()
                    && f.articles.iter().all(|a| a.downloaded)
            })
            .map(|f| f.id.clone());
        let state = if has_index {
            State::Pending {
                writes: seeded,
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
            resumed,
            index_on_disk,
            state: Mutex::new(state),
            failures: Mutex::new(HashMap::new()),
            confirmed_missing: AtomicUsize::new(0),
            damaged_seen: AtomicUsize::new(0),
            fold: Mutex::new(FoldSlot::Idle),
            recovery_exponents: recovery_exponents(job),
        }
    }

    /// Record a successful write of `len` bytes at `offset` into `file_id`.
    /// Returns the slices that became fully written and must be verified.
    fn mark(&self, file_id: &str, offset: u64, len: u64) -> Marked {
        let none = Marked {
            pending: Vec::new(),
            try_match: false,
        };
        let mut state = self.state.lock();
        match &mut *state {
            State::Disabled => none,
            State::Pending { writes, .. } => {
                writes
                    .entry(file_id.to_string())
                    .or_default()
                    .push(Write { offset, len });
                none
            }
            State::Ready(tracked) => {
                if let Some(writes) = tracked.unmatched.get_mut(file_id) {
                    writes.push(Write { offset, len });
                    return Marked {
                        pending: Vec::new(),
                        try_match: offset < PAR2_HASH_16K_LEN as u64,
                    };
                }
                let Some((index, path)) = tracked.files.get(file_id) else {
                    return none;
                };
                if tracked.untracked.contains(index) {
                    return none;
                }
                let (index, path) = (*index, path.clone());
                let done = tracked.verifier.mark_written(index, offset, len);
                Marked {
                    pending: pending_for(&tracked.verifier, &path, done),
                    try_match: false,
                }
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
        let Marked {
            mut pending,
            try_match,
        } = self.mark(file_id, offset, len);
        let completed_index = (file_complete
            && self
                .filenames
                .get(file_id)
                .is_some_and(|name| is_par2_index(name)))
        .then(|| file_id.to_string())
        .or_else(|| self.index_on_disk.clone());
        if let Some(index_id) = completed_index
            && self.claim_install()
        {
            let this = self.clone();
            match tokio::task::spawn_blocking(move || this.install(&index_id)).await {
                Ok(more) => pending.extend(more),
                Err(e) => {
                    warn!("PAR2 slice map install task failed: {e}");
                    *self.state.lock() = State::Disabled;
                }
            }
        }
        let to_match = try_match.then(|| file_id.to_string());
        if pending.is_empty() && to_match.is_none() {
            return;
        }
        let this = self.clone();
        let task = move || {
            if let Some(id) = to_match {
                pending.extend(this.match_by_hash(&[id]));
            }
            this.verify_all(pending);
            this.spawn_grow();
        };
        if let Err(e) = tokio::task::spawn_blocking(task).await {
            warn!("PAR2 slice verify task failed: {e}");
        }
    }

    /// Note a terminal article failure in `file_id`. `confirmed` is true when
    /// every server refused the article.
    pub(crate) fn record_failure(self: &std::sync::Arc<Self>, file_id: &str, confirmed: bool) {
        if confirmed {
            self.confirmed_missing.fetch_add(1, Ordering::Relaxed);
        }
        {
            let mut failures = self.failures.lock();
            let entry = failures.entry(file_id.to_string()).or_insert(true);
            *entry &= confirmed;
        }
        // Confirmed damage opens (or grows) the fold now, off the worker, so
        // the prefix is folded while the rest of the job downloads.
        if !confirmed {
            return;
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let idle = matches!(*self.fold.lock(), FoldSlot::Idle);
        let this = self.clone();
        if idle && let Some(plan) = self.plan() {
            // Allocate here so the fold exists before the job can finish;
            // the prefix read happens on a blocking task.
            let rows = rows_for(self.damage_hint(), plan.recovery, plan.slice_size);
            if let Some(fold) = self.create_fold(rows, plan.total, plan.slice_size) {
                runtime.spawn_blocking(move || this.feed_prefix(&fold, rows, plan.slice_size));
            }
        } else if !idle {
            drop(runtime);
            this.spawn_grow();
        }
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
        // A PAR2 file no NZB file maps to cannot be written until a data file
        // is matched to it by hash.
        let claimed: HashSet<usize> = matched.values().copied().collect();
        untracked.extend((0..layout.files.len()).filter(|i| !claimed.contains(i)));
        // Data files the names did not map, still open to the hash matcher.
        let unmatched: HashMap<String, Vec<Write>> = self
            .filenames
            .iter()
            .filter(|(id, name)| {
                !name.to_lowercase().ends_with(".par2")
                    && !matched.contains_key(*id)
                    && !self.resumed.contains(*id)
            })
            .map(|(id, _)| (id.clone(), Vec::new()))
            .collect();

        let mut tracked = Box::new(Tracked {
            verifier: SliceVerifier::new(&layout),
            files,
            untracked,
            recovery_block_count: set.recovery_block_count,
            par2_files: par2_refs.iter().map(|p| (p.hash_16k, p.size)).collect(),
            claimed,
            unmatched,
            hashed: HashSet::new(),
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
        let mut to_match = Vec::new();
        for (file_id, writes) in buffered {
            if let Some(held) = tracked.unmatched.get_mut(&file_id) {
                held.extend(writes);
                to_match.push(file_id);
                continue;
            }
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
        drop(state);
        pending.extend(self.match_by_hash(&to_match));
        pending
    }

    /// Try to map each unmatched NZB file in `file_ids` to a PAR2 file by the
    /// MD5 of its first 16 KiB (or of the whole PAR2 file, when shorter), read
    /// from disk once its prefix has landed. A match replays the file's
    /// buffered writes; the slices that replay completes are returned. Runs on
    /// a blocking task.
    fn match_by_hash(&self, file_ids: &[String]) -> Vec<PendingSlice> {
        let mut pending = Vec::new();
        for file_id in file_ids {
            // Prefix lengths worth hashing: one per distinct unclaimed PAR2
            // prefix length the landed bytes now cover, not hashed before.
            let lens: Vec<u64> = {
                let mut state = self.state.lock();
                let State::Ready(tracked) = &mut *state else {
                    return pending;
                };
                let Some(writes) = tracked.unmatched.get(file_id) else {
                    continue;
                };
                let covered = covered_prefix(writes);
                let mut lens: Vec<u64> = tracked
                    .par2_files
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| !tracked.claimed.contains(i))
                    .map(|(_, (_, size))| (*size).min(PAR2_HASH_16K_LEN as u64))
                    .filter(|len| *len > 0 && *len <= covered)
                    .collect();
                lens.sort_unstable();
                lens.dedup();
                lens.retain(|len| tracked.hashed.insert((file_id.clone(), *len)));
                lens
            };
            let path = self.work_dir.join(&self.filenames[file_id]);
            for len in lens {
                let digest: [u8; 16] = match read_range(&path, 0, len) {
                    Ok(bytes) => Md5::digest(&bytes).into(),
                    Err(e) => {
                        debug!(path = %path.display(), "16 KiB prefix unreadable: {e}");
                        continue;
                    }
                };
                let mut state = self.state.lock();
                let State::Ready(tracked) = &mut *state else {
                    return pending;
                };
                let found = tracked
                    .par2_files
                    .iter()
                    .enumerate()
                    .position(|(i, (h, s))| {
                        !tracked.claimed.contains(&i)
                            && *h == digest
                            && (*s).min(PAR2_HASH_16K_LEN as u64) == len
                    });
                let Some(index) = found else {
                    continue;
                };
                let Some(writes) = tracked.unmatched.remove(file_id) else {
                    break;
                };
                tracked.claimed.insert(index);
                tracked.untracked.remove(&index);
                tracked.files.insert(file_id.clone(), (index, path.clone()));
                info!(file = %self.filenames[file_id], par2_file = index, "Obfuscated file matched to PAR2 by 16 KiB hash");
                for w in writes {
                    let done = tracked.verifier.mark_written(index, w.offset, w.len);
                    pending.extend(pending_for(&tracked.verifier, &path, done));
                }
                break;
            }
        }
        pending
    }

    /// Read each slice back and verify it, folding the verified ones into the
    /// repair fold when one is open. Runs on a blocking task.
    fn verify_all(&self, pending: Vec<PendingSlice>) {
        for slice in pending {
            let bytes = match read_range(&slice.path, slice.offset, slice.len) {
                Ok(bytes) => bytes,
                Err(e) => {
                    warn!(path = %slice.path.display(), slice = slice.global_index, "Slice read-back failed, left unknown: {e}");
                    continue;
                }
            };
            let verdict = match &mut *self.state.lock() {
                State::Ready(tracked) => tracked.verifier.verify(slice.global_index, &bytes),
                _ => continue,
            };
            let fold = self.active_fold();
            match verdict {
                SliceState::Damaged => {
                    self.damaged_seen.fetch_add(1, Ordering::Relaxed);
                    debug!(slice = slice.global_index, "PAR2 slice damaged");
                    if let Some(fold) = fold {
                        fold.lock().note_corrupt(slice.global_index);
                    }
                }
                SliceState::Verified => {
                    if let Some(fold) = fold
                        && let Err(e) = fold.lock().feed(slice.global_index, &bytes)
                    {
                        warn!("Repair fold feed failed: {e}");
                    }
                }
                SliceState::Unknown => {}
            }
        }
    }

    /// Open or grow the fold on a detached blocking task, so the worker that
    /// verified the slice is not held up by disk reads. A no-op without
    /// damage, without a runtime, or once the fold is refused.
    fn spawn_grow(self: &std::sync::Arc<Self>) {
        if self.damage_hint() == 0 || matches!(*self.fold.lock(), FoldSlot::Refused) {
            return;
        }
        if let Some(fold) = self.active_fold()
            && fold.lock().growing()
        {
            return;
        }
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let this = self.clone();
            runtime.spawn_blocking(move || this.grow_fold());
        }
    }

    fn active_fold(&self) -> Option<SharedFold> {
        match &*self.fold.lock() {
            FoldSlot::Active(fold) => Some(fold.clone()),
            _ => None,
        }
    }

    /// The fold for post-processing: open and not poisoned.
    pub(crate) fn fold(&self) -> Option<SharedFold> {
        self.active_fold().filter(|f| !f.lock().is_poisoned())
    }

    /// Where a slice lives on disk, for folding it from there.
    fn slice_source(tracked: &Tracked, gi: usize) -> Option<PendingSlice> {
        let (file_index, offset, len) = tracked.verifier.slice_range(gi)?;
        let path = tracked
            .files
            .values()
            .find(|(index, _)| *index == file_index)
            .map(|(_, path)| path.clone())?;
        Some(PendingSlice {
            global_index: gi,
            path,
            offset,
            len,
        })
    }

    /// The set's shape for sizing the fold; `None` before the index is in.
    fn plan(&self) -> Option<Plan> {
        let (slice_size, total, recovery) = match &*self.state.lock() {
            State::Ready(t) => (
                t.verifier.slice_size(),
                t.verifier.total_slices(),
                t.recovery_block_count,
            ),
            _ => return None,
        };
        // The index rarely carries recovery slices itself; the volume names
        // say how many the set has. With neither, open rows on exponents
        // 0.. and let the solve add rows for what is on disk.
        let recovery = match (self.recovery_exponents.len() as u32, recovery) {
            (0, 0) => u32::MAX,
            (0, r) => r,
            (named, _) => named,
        };
        Some(Plan {
            slice_size,
            total,
            recovery,
        })
    }

    /// Open the fold once damage is first seen, or grow its rows when the
    /// damage outruns them. Runs on a blocking task.
    fn grow_fold(&self) {
        let damage = self.damage_hint();
        if damage == 0 {
            return;
        }
        let existing = match &*self.fold.lock() {
            FoldSlot::Refused => return,
            FoldSlot::Active(fold) => Some(fold.clone()),
            FoldSlot::Idle => None,
        };
        let Some(Plan {
            slice_size,
            total,
            recovery,
        }) = self.plan()
        else {
            return;
        };
        let want = rows_for(damage, recovery, slice_size);
        match existing {
            None => self.open_fold(want, total, slice_size),
            Some(fold) => {
                let (have, ticket) = {
                    let mut f = fold.lock();
                    let have = f.rows();
                    if f.growing() || damage <= have || have >= want {
                        return;
                    }
                    let target = want.max(rows_for(have, recovery, slice_size));
                    match f.begin_growth(&self.exponents(have..target)) {
                        Ok(Some(ticket)) => (have, ticket),
                        Ok(None) => return,
                        Err(e) => {
                            // The fold stays usable with its current rows;
                            // post-processing falls back if they are too few.
                            warn!(damage, rows = have, "Repair fold could not grow: {e}");
                            return;
                        }
                    }
                };
                let state = &self.state;
                let read = |gi: usize| {
                    let source = match &*state.lock() {
                        State::Ready(t) => Self::slice_source(t, gi),
                        _ => None,
                    };
                    let source =
                        source.ok_or_else(|| std::io::Error::other("slice has no path"))?;
                    read_range(&source.path, source.offset, source.len)
                };
                if backfill_growth(&fold, ticket, slice_size, read) {
                    info!(
                        rows = fold.lock().rows(),
                        from = have,
                        damage,
                        "Repair fold rows extended"
                    );
                }
            }
        }
    }

    /// Exponents for rows `range`: the named volumes' exponents in order,
    /// else `range` itself.
    fn exponents(&self, range: std::ops::Range<usize>) -> Vec<u32> {
        if self.recovery_exponents.is_empty() {
            (range.start as u32..range.end as u32).collect()
        } else {
            let end = range.end.min(self.recovery_exponents.len());
            self.recovery_exponents[range.start.min(end)..end].to_vec()
        }
    }

    fn open_fold(&self, rows: usize, total: usize, slice_size: u64) {
        if let Some(fold) = self.create_fold(rows, total, slice_size) {
            self.feed_prefix(&fold, rows, slice_size);
        }
    }

    /// Allocate the fold and make it the job's, without any I/O: from here
    /// on every slice that verifies is folded. `None` when one already exists
    /// or the set cannot be folded.
    fn create_fold(&self, rows: usize, total: usize, slice_size: u64) -> Option<SharedFold> {
        let exps = self.exponents(0..rows);
        let mut slot = self.fold.lock();
        if !matches!(*slot, FoldSlot::Idle) {
            return None;
        }
        match RepairFold::new(total, slice_size, &exps) {
            Ok(fold) => {
                let fold = std::sync::Arc::new(Mutex::new(fold));
                *slot = FoldSlot::Active(fold.clone());
                Some(fold)
            }
            Err(e) => {
                info!(
                    slice_size,
                    rows, "In-flight repair fold refused, disk repair will be used: {e}"
                );
                *slot = FoldSlot::Refused;
                None
            }
        }
    }

    /// Fold the slices verified before the fold existed. Runs on a blocking
    /// task.
    fn feed_prefix(&self, fold: &SharedFold, rows: usize, slice_size: u64) {
        // The selection is a snapshot. A slice a worker verifies after it is
        // folded once, from the worker's own bytes, and a slice in the
        // snapshot that the worker got to first is not read here at all: the
        // feeder re-checks `is_fed` under the fold lock before each read.
        let prefix_reads = feed_from_disk(fold, &self.prefix_sources(), slice_size);
        info!(
            rows,
            slice_size, prefix_reads, "In-flight repair fold opened"
        );
    }

    /// Every slice the verifier holds `Verified`, with where it lives on disk.
    /// The prefix is the set's, not one file's, so this one pass covers every
    /// slice that verified before damage opened the fold.
    fn prefix_sources(&self) -> Vec<PendingSlice> {
        match &*self.state.lock() {
            State::Ready(t) => t
                .verifier
                .states()
                .iter()
                .enumerate()
                .filter(|(_, s)| **s == SliceState::Verified)
                .filter_map(|(gi, _)| Self::slice_source(t, gi))
                .collect(),
            _ => Vec::new(),
        }
    }

    /// A running estimate of the damage while articles are still arriving:
    /// slices that failed their checksum plus articles confirmed missing
    /// (each takes out at least one slice). [`summary`](Self::summary) gives
    /// the exact count once the data is in.
    pub(crate) fn damage_hint(&self) -> usize {
        self.damaged_seen.load(Ordering::Relaxed) + self.confirmed_missing.load(Ordering::Relaxed)
    }

    /// Decide the remaining slices and summarise the map. `None` when no map
    /// was ever built. Called when the job's data articles are all resolved
    /// (to size the recovery volumes it needs) and again at job end.
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

/// Fold slices listed on disk into the fold's rows, reading each one once.
///
/// The read bound is the WI-148 one, shared by the prefix feed and a growth's
/// backfill: [`batch_len`] slices (at most 8) and [`MAX_BATCH_BYTES`] (48 MiB)
/// are in flight at a time, and the fold lock is held only to fold a batch,
/// never across a read, so workers keep folding what they verify meanwhile.
///
/// A slice already folded — by a worker, between the snapshot and this read —
/// is skipped without reading it. Returns how many slices it read; a read
/// failure counts as an attempt on the fold's counter but skips the slice
/// (post-processing folds it at the end).
fn feed_from_disk(fold: &SharedFold, sources: &[PendingSlice], slice_size: u64) -> usize {
    let mut read = 0usize;
    for chunk in sources.chunks(batch_len(slice_size)) {
        let mut batch: Vec<(usize, Vec<u8>)> = Vec::with_capacity(chunk.len());
        for source in chunk {
            {
                // Skip before reading: the worker's bytes are already folded.
                let mut f = fold.lock();
                if f.is_fed(source.global_index) {
                    continue;
                }
                f.note_disk_read(source.global_index, source.len);
            }
            match read_range(&source.path, source.offset, source.len) {
                Ok(bytes) => {
                    read += 1;
                    batch.push((source.global_index, bytes));
                }
                Err(e) => {
                    debug!(
                        slice = source.global_index,
                        "Repair fold disk feed read failed: {e}"
                    );
                }
            }
        }
        if batch.is_empty() {
            continue;
        }
        let refs: Vec<(usize, &[u8])> = batch.iter().map(|(i, b)| (*i, b.as_slice())).collect();
        let mut fold = fold.lock();
        match fold.feed_batch(&refs) {
            Ok(n) => fold.note_disk_fed(n),
            Err(e) => warn!("Repair fold disk feed failed: {e}"),
        }
    }
    read
}

/// Fold a growth's slices into its new rows, reading them back with `read`
/// in bounded batches. The fold lock is taken only to fold each batch, never
/// across a read, so workers keep feeding the fold meanwhile.
///
/// A growth re-reads slices already folded into its existing rows: the new
/// rows are fresh zeros and need that contribution again. A slice that came
/// off disk before is therefore counted as a `disk_reread`. Returns whether
/// the growth completed; a read failure abandons it.
fn backfill_growth<R>(fold: &SharedFold, ticket: GrowthTicket, slice_size: u64, mut read: R) -> bool
where
    R: FnMut(usize) -> std::io::Result<Vec<u8>>,
{
    for chunk in ticket.slices.chunks(batch_len(slice_size)) {
        let mut batch = Vec::with_capacity(chunk.len());
        for &gi in chunk {
            match read(gi) {
                Ok(bytes) => {
                    let mut f = fold.lock();
                    f.note_disk_read(gi, bytes.len() as u64);
                    drop(f);
                    batch.push((gi, bytes));
                }
                Err(e) => {
                    warn!(slice = gi, "Repair fold growth abandoned, read failed: {e}");
                    fold.lock().abandon_growth_of(ticket.generation);
                    return false;
                }
            }
        }
        let refs: Vec<(usize, &[u8])> = batch.iter().map(|(i, b)| (*i, b.as_slice())).collect();
        if !fold.lock().backfill(ticket.generation, &refs) {
            // Post-processing took the fold and dropped the growth.
            return false;
        }
    }
    true
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

/// End of the contiguous run of written bytes starting at offset 0.
fn covered_prefix(writes: &[Write]) -> u64 {
    let mut sorted: Vec<Write> = writes.to_vec();
    sorted.sort_unstable_by_key(|w| w.offset);
    let mut end = 0;
    for w in sorted {
        if w.offset > end {
            break;
        }
        end = end.max(w.offset + w.len);
    }
    end
}

fn read_range(path: &Path, offset: u64, len: u64) -> std::io::Result<Vec<u8>> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(path)?;
    file.seek(SeekFrom::Start(offset))?;
    let mut buf = vec![0u8; len as usize];
    file.read_exact(&mut buf)?;
    Ok(buf)
}

#[cfg(test)]
mod fold_tests {
    use super::*;

    fn slice(i: usize, len: usize) -> Vec<u8> {
        (0..len).map(|b| (b * 13 + i * 7 + 1) as u8).collect()
    }

    /// The growth backfill never holds the fold while it reads: a worker can
    /// take the lock and feed a slice during every read, and the grown rows
    /// still solve.
    #[test]
    fn growth_backfill_does_not_block_workers() {
        let (n, size) = (40usize, 64usize);
        // Small batches: one slice each would hide a lock held across reads.
        let data: Vec<Vec<u8>> = (0..n).map(|i| slice(i, size)).collect();
        let fold: SharedFold =
            std::sync::Arc::new(Mutex::new(RepairFold::new(n, size as u64, &[0]).unwrap()));
        for (i, d) in data.iter().enumerate().take(20) {
            fold.lock().feed(i, d).unwrap();
        }
        let ticket = fold.lock().begin_growth(&[0, 1, 2]).unwrap().unwrap();
        assert_eq!(ticket.slices.len(), 20);
        let mut next_worker_slice = 20;
        let mut reads = 0;
        let done = backfill_growth(&fold, ticket, size as u64, |gi| {
            reads += 1;
            // A worker verifies a slice right now: the lock must be free.
            let mut f = fold.try_lock().expect("fold locked during a disk read");
            if next_worker_slice < n - 3 {
                f.feed(next_worker_slice, &data[next_worker_slice]).unwrap();
                next_worker_slice += 1;
            }
            Ok(data[gi].clone())
        });
        assert!(done);
        assert_eq!(reads, 20, "each pre-growth slice read once");
        let f = fold.lock();
        assert!(!f.growing());
        assert_eq!(f.complete_rows(), 3);
        drop(f);
        // Feed the rest except the three damaged ones, then solve.
        let damaged = [n - 3, n - 2, n - 1];
        for (i, d) in data.iter().enumerate().take(n - 3).skip(next_worker_slice) {
            fold.lock().feed(i, d).unwrap();
        }
        let constants = rust_par2::matrix::par2_input_constants(n);
        let recovery: HashMap<u32, Vec<u8>> = (0..3u32)
            .map(|e| {
                let mut out = vec![0u8; size];
                for (i, d) in data.iter().enumerate() {
                    rust_par2::gf_simd::mul_add_buffer(
                        &mut out,
                        d,
                        rust_par2::gf::pow(constants[i], e),
                    );
                }
                (e, out)
            })
            .collect();
        for (gi, bytes) in fold.lock().solve(&damaged, &recovery).unwrap() {
            assert_eq!(bytes, data[gi]);
        }
    }

    /// WI-1236: the prefix feed reads at most `batch_len` slices per batch
    /// (8 at the 256 KiB-and-under sizes, fewer for big slices) and folds each
    /// one once. A slice a worker already folded between the snapshot and the
    /// read is skipped without being read.
    #[test]
    fn prefix_feed_is_bounded_and_skips_what_is_already_fed() {
        let size = 32usize;
        let n = 40usize;
        let data: Vec<Vec<u8>> = (0..n).map(|i| slice(i, size)).collect();
        let dir = tempfile::tempdir().unwrap();
        let fold: SharedFold =
            std::sync::Arc::new(Mutex::new(RepairFold::new(n, size as u64, &[0]).unwrap()));
        // Every slice but #5 was verified before the fold existed.
        let mut sources = Vec::new();
        for (gi, bytes) in data.iter().enumerate() {
            if gi == 5 {
                continue;
            }
            let path = dir.path().join(format!("s{gi}.bin"));
            std::fs::write(&path, bytes).unwrap();
            sources.push(PendingSlice {
                global_index: gi,
                path,
                offset: 0,
                len: size as u64,
            });
        }
        // A worker folded #11 in the window between the snapshot and the feed.
        fold.lock().feed(11, &data[11]).unwrap();
        let read = feed_from_disk(&fold, &sources, size as u64);
        assert_eq!(
            read,
            n - 2,
            "skipped the worker's slice and the damaged one"
        );
        let f = fold.lock();
        assert_eq!(f.fed_count(), n - 1);
        assert_eq!(f.disk_reads(), n - 2);
        assert_eq!(f.disk_rereads(), 0);
        assert!(!f.is_fed(5), "a damaged slice is never fed");
        drop(f);
        // The bound is the WI-148 one: one batch is at most 8 slices.
        assert_eq!(batch_len(size as u64), 8);
        assert_eq!(batch_len(8 << 20), 6, "48 MiB / 8 MiB");
        assert_eq!(batch_len(32 << 20), 1, "a 32 MiB slice alone is 48 MiB-max");
        assert_eq!(sources.len(), n - 1);
    }

    /// A failed read abandons the growth and leaves the old rows usable.
    #[test]
    fn failed_backfill_read_abandons_the_growth() {
        let size = 32usize;
        let fold: SharedFold =
            std::sync::Arc::new(Mutex::new(RepairFold::new(4, size as u64, &[0]).unwrap()));
        fold.lock().feed(0, &slice(0, size)).unwrap();
        let ticket = fold.lock().begin_growth(&[1]).unwrap().unwrap();
        assert!(!backfill_growth(&fold, ticket, size as u64, |_| {
            Err(std::io::Error::other("gone"))
        }));
        let f = fold.lock();
        assert!(!f.growing());
        assert_eq!(f.rows(), 1);
    }
}
