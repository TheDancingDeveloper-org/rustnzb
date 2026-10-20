//! In-flight PAR2 repair fold (WI-148).
//!
//! PAR2 recovery slice `e` is `R_e = Σ_i c_i^e · D_i` over every input slice
//! `i`. A repair of the damaged slices `J` solves
//!
//! ```text
//! R_e ⊕ Σ_{i ∉ J} c_i^e · D_i = Σ_{j ∈ J} c_j^e · D_j
//! ```
//!
//! The left-hand sum over intact slices is the expensive part: it reads every
//! surviving byte of the set. It is linear, so it can be accumulated one slice
//! at a time, in any order, before the recovery data is even on disk. A
//! [`RepairFold`] owns a bounded set of accumulator rows (one per recovery
//! exponent) and the download engine feeds each slice into them as soon as it
//! verifies. At job end [`RepairFold::solve`] XORs in the recovery data,
//! inverts the D×D matrix and returns the repaired slices: what is left is
//! `D × slice_size` of solve plus the writes.
//!
//! The arithmetic is `rust-par2`'s own (`par2_input_constants`, `gf::pow`,
//! `mul_add_multi`, `GfMatrix::invert`), so the bytes are identical to its
//! disk-based `repair_from_verify`.
//!
//! Bounds, after zurg `internal/nzb/repair.go`:
//! - rows × slice size ≤ [`MAX_REPAIR_ROW_BYTES`]; slices over
//!   [`MAX_FOLD_SLICE_SIZE`] are refused.
//! - slices fed from disk are read in batches of at most [`MAX_REPAIR_BATCH`]
//!   slices and [`MAX_BATCH_BYTES`].
//! - a slice that was fed and is later found corrupt poisons the fold
//!   ([`RepairFold::note_corrupt`]); the caller then uses the disk path.

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

use parking_lot::Mutex;
use rust_par2::gf;
use rust_par2::gf_simd::mul_add_multi;
use rust_par2::matrix::{GfMatrix, par2_input_constants};

/// Upper bound on the accumulator rows' memory: rows × slice size.
pub const MAX_REPAIR_ROW_BYTES: u64 = 256 * 1024 * 1024;
/// Sets with larger slices are left to the disk path.
pub const MAX_FOLD_SLICE_SIZE: u64 = 32 * 1024 * 1024;
/// Most slices read from disk and folded in one batch.
pub const MAX_REPAIR_BATCH: usize = 8;
/// Most bytes read from disk and held for one batch.
pub const MAX_BATCH_BYTES: u64 = 48 * 1024 * 1024;

/// A fold shared between the download engine and post-processing.
pub type SharedFold = Arc<Mutex<RepairFold>>;

/// Why a fold could not be built, extended or solved. Every variant means
/// "use the disk repair path", never "fail the job".
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FoldError {
    #[error("slice size {0} is over the in-flight repair limit")]
    SliceTooLarge(u64),
    #[error("no recovery rows fit the memory budget")]
    NoRows,
    #[error("a slice fed into the fold later failed verification")]
    Poisoned,
    #[error("need {needed} recovery rows, the fold has {usable} with recovery data")]
    NotEnoughRows { needed: usize, usable: usize },
    #[error("slice {0} is outside the set")]
    OutOfRange(usize),
    #[error("decode matrix is singular")]
    Singular,
    #[error("reading slice {index} from disk failed: {message}")]
    Read { index: usize, message: String },
}

/// Accumulated intact-slice contributions for a fixed set of recovery
/// exponents.
pub struct RepairFold {
    slice_size: usize,
    /// PAR2 input constant per global slice.
    constants: Vec<u16>,
    exponents: Vec<u32>,
    rows: Vec<Vec<u8>>,
    fed: Vec<bool>,
    fed_count: usize,
    poisoned: bool,
    /// Rows being added without holding the caller's lock (see
    /// [`begin_growth`](RepairFold::begin_growth)).
    growth: Option<Growth>,
    /// Bumped whenever a growth ends, so stale backfills are ignored.
    generation: u64,
    /// Slices folded from disk rather than from the network path.
    disk_fed: usize,
    /// Slices read back from disk to be folded (WI-1236). A slice verified
    /// before damage was known must be read at most once; a second read is
    /// counted in `disk_rereads` so a caller can assert that.
    disk_read: Vec<bool>,
    /// Slices read back from disk to be folded, total.
    disk_reads: usize,
    /// Of those, slices that had already been read back once.
    disk_rereads: usize,
    /// Bytes read back from disk to be folded, total.
    disk_read_bytes: u64,
    /// This fold's own row limit, below the global budget. Tests use it to
    /// force a growth past the budget.
    max_rows: usize,
}

impl fmt::Debug for RepairFold {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RepairFold")
            .field("slice_size", &self.slice_size)
            .field("slices", &self.fed.len())
            .field("rows", &self.rows.len())
            .field("fed", &self.fed_count)
            .field("poisoned", &self.poisoned)
            .finish()
    }
}

/// Most rows the budget allows for `slice_size`.
pub fn max_rows(slice_size: u64) -> usize {
    if slice_size == 0 || slice_size > MAX_FOLD_SLICE_SIZE {
        0
    } else {
        (MAX_REPAIR_ROW_BYTES / slice_size) as usize
    }
}

/// Rows added to a live fold: new slices go into them at once, the slices
/// fed before the growth are backfilled in batches.
#[derive(Debug)]
struct Growth {
    /// First new row.
    start: usize,
    /// Slices fed before the growth and not yet folded into the new rows.
    pending: std::collections::HashSet<usize>,
}

/// A growth handed to the caller to backfill.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrowthTicket {
    pub generation: u64,
    /// Slices to read back and pass to [`RepairFold::backfill`].
    pub slices: Vec<usize>,
}

/// How many `slice_size` slices one disk batch may hold.
pub fn batch_len(slice_size: u64) -> usize {
    ((MAX_BATCH_BYTES / slice_size.max(1)) as usize).clamp(1, MAX_REPAIR_BATCH)
}

impl RepairFold {
    /// A fold over `total_slices` input slices with one row per exponent.
    pub fn new(total_slices: usize, slice_size: u64, exponents: &[u32]) -> Result<Self, FoldError> {
        if slice_size > MAX_FOLD_SLICE_SIZE {
            return Err(FoldError::SliceTooLarge(slice_size));
        }
        if exponents.is_empty() || exponents.len() > max_rows(slice_size) {
            return Err(FoldError::NoRows);
        }
        let slice = slice_size as usize;
        Ok(Self {
            slice_size: slice,
            constants: par2_input_constants(total_slices),
            exponents: exponents.to_vec(),
            rows: exponents.iter().map(|_| vec![0u8; slice]).collect(),
            fed: vec![false; total_slices],
            fed_count: 0,
            poisoned: false,
            disk_fed: 0,
            disk_read: vec![false; total_slices],
            disk_reads: 0,
            disk_rereads: 0,
            disk_read_bytes: 0,
            max_rows: max_rows(slice_size),
            growth: None,
            generation: 0,
        })
    }

    pub fn slice_size(&self) -> u64 {
        self.slice_size as u64
    }
    /// Cap this fold at `rows` rows, below the global budget. A growth past
    /// the cap fails with [`FoldError::NoRows`], which the caller turns into
    /// a disk repair — the same path a host with a tighter memory budget
    /// takes. Never raises the global limit.
    pub fn with_row_limit(mut self, rows: usize) -> Self {
        self.max_rows = rows.min(max_rows(self.slice_size as u64));
        self
    }
    pub fn total_slices(&self) -> usize {
        self.fed.len()
    }
    pub fn rows(&self) -> usize {
        self.rows.len()
    }
    pub fn exponents(&self) -> &[u32] {
        &self.exponents
    }
    pub fn fed_count(&self) -> usize {
        self.fed_count
    }
    pub fn disk_fed(&self) -> usize {
        self.disk_fed
    }
    /// Count slices the caller folded from disk through [`feed_batch`](Self::feed_batch).
    pub fn note_disk_fed(&mut self, count: usize) {
        self.disk_fed += count;
    }

    /// Slices read back from disk to be folded so far (WI-1236).
    pub fn disk_reads(&self) -> usize {
        self.disk_reads
    }

    /// Of [`disk_reads`](Self::disk_reads), how many were a second read of a
    /// slice already read back once.
    pub fn disk_rereads(&self) -> usize {
        self.disk_rereads
    }

    /// Bytes read back from disk to be folded so far (WI-1236).
    pub fn disk_read_bytes(&self) -> u64 {
        self.disk_read_bytes
    }

    /// Count a slice about to be read back from disk. The caller counts
    /// before the read, so a read that fails still counts as an attempt.
    pub fn note_disk_read(&mut self, index: usize, len: u64) {
        match self.disk_read.get_mut(index) {
            Some(seen) if *seen => self.disk_rereads += 1,
            Some(seen) => *seen = true,
            None => self.disk_rereads += 1,
        }
        self.disk_reads += 1;
        self.disk_read_bytes += len;
    }
    pub fn is_fed(&self, index: usize) -> bool {
        self.fed.get(index).copied().unwrap_or(false)
    }
    pub fn is_poisoned(&self) -> bool {
        self.poisoned
    }
    /// Rows this fold may hold: its own limit, never above the global budget.
    fn row_limit(&self) -> usize {
        self.max_rows.min(max_rows(self.slice_size as u64))
    }
    /// Bytes held by the rows.
    pub fn row_bytes(&self) -> u64 {
        (self.rows.len() * self.slice_size) as u64
    }

    /// Fold one verified slice. `bytes` is the slice's data (the last slice of
    /// a file may be short; it is zero-padded). A slice already fed is
    /// ignored. Returns whether it was folded.
    pub fn feed(&mut self, index: usize, bytes: &[u8]) -> Result<bool, FoldError> {
        self.feed_batch(&[(index, bytes)]).map(|n| n == 1)
    }

    /// Fold several verified slices at once (one pass over each row).
    /// Returns how many were folded.
    pub fn feed_batch(&mut self, slices: &[(usize, &[u8])]) -> Result<usize, FoldError> {
        let mut fresh: Vec<(usize, &[u8])> = Vec::with_capacity(slices.len());
        for &(index, bytes) in slices {
            if index >= self.fed.len() {
                return Err(FoldError::OutOfRange(index));
            }
            if !self.fed[index] && !fresh.iter().any(|(i, _)| *i == index) {
                fresh.push((index, bytes));
            }
        }
        if fresh.is_empty() {
            return Ok(0);
        }
        let row_range = 0..self.rows.len();
        fold_into(
            &mut self.rows[row_range.clone()],
            &self.exponents[row_range],
            &self.constants,
            self.slice_size,
            &fresh,
        );
        for (index, _) in &fresh {
            self.fed[*index] = true;
        }
        self.fed_count += fresh.len();
        Ok(fresh.len())
    }

    /// A slice failed verification. If it was already folded, its
    /// contribution is wrong for the solve and the fold is poisoned.
    pub fn note_corrupt(&mut self, index: usize) {
        if self.is_fed(index) {
            self.poisoned = true;
        }
    }

    /// Add rows for `exponents` and fold every already-fed slice into the new
    /// rows only, reading those slices back with `read`. Exponents already
    /// present are skipped. Fails when the rows would break the budget.
    pub fn extend_rows<R>(&mut self, exponents: &[u32], mut read: R) -> Result<usize, FoldError>
    where
        R: FnMut(usize) -> std::io::Result<Vec<u8>>,
    {
        self.abandon_growth();
        let new: Vec<u32> = exponents
            .iter()
            .copied()
            .filter(|e| !self.exponents.contains(e))
            .collect();
        if new.is_empty() {
            return Ok(0);
        }
        if self.rows.len() + new.len() > self.row_limit() {
            return Err(FoldError::NoRows);
        }
        let start = self.rows.len();
        self.exponents.extend(&new);
        self.rows
            .extend(new.iter().map(|_| vec![0u8; self.slice_size]));
        let fed: Vec<usize> = (0..self.fed.len()).filter(|i| self.fed[*i]).collect();
        let result = for_each_batch(&fed, self.slice_size, &mut read, |batch| {
            let refs: Vec<(usize, &[u8])> = batch.iter().map(|(i, b)| (*i, b.as_slice())).collect();
            fold_into(
                &mut self.rows[start..],
                &self.exponents[start..],
                &self.constants,
                self.slice_size,
                &refs,
            );
        });
        if let Err(e) = result {
            // Leave the fold as it was: the new rows are incomplete.
            self.rows.truncate(start);
            self.exponents.truncate(start);
            return Err(e);
        }
        Ok(new.len())
    }

    /// Start adding rows for `exponents` without holding the fold while the
    /// fed slices are re-read. The new rows are live at once (slices fed from
    /// now on go into them), and the slices fed so far are returned in the
    /// ticket, to be read back outside the lock and passed to
    /// [`backfill`](Self::backfill) a batch at a time. The rows count for
    /// [`solve`](Self::solve) only once every slice is backfilled. `None`
    /// when a growth is already running or no exponent is new.
    pub fn begin_growth(&mut self, exponents: &[u32]) -> Result<Option<GrowthTicket>, FoldError> {
        if self.growth.is_some() {
            return Ok(None);
        }
        let new: Vec<u32> = exponents
            .iter()
            .copied()
            .filter(|e| !self.exponents.contains(e))
            .collect();
        if new.is_empty() {
            return Ok(None);
        }
        if self.rows.len() + new.len() > self.row_limit() {
            return Err(FoldError::NoRows);
        }
        let start = self.rows.len();
        self.exponents.extend(&new);
        self.rows
            .extend(new.iter().map(|_| vec![0u8; self.slice_size]));
        let slices: Vec<usize> = (0..self.fed.len()).filter(|i| self.fed[*i]).collect();
        let pending = slices.iter().copied().collect();
        self.growth = Some(Growth { start, pending });
        if slices.is_empty() {
            self.growth = None;
            self.generation += 1;
        }
        Ok(Some(GrowthTicket {
            generation: self.generation,
            slices,
        }))
    }

    /// Fold a batch of a growth's slices into its new rows. Returns false
    /// when the growth is gone (finished, abandoned or superseded).
    pub fn backfill(&mut self, generation: u64, slices: &[(usize, &[u8])]) -> bool {
        if generation != self.generation {
            return false;
        }
        let Some(growth) = self.growth.as_mut() else {
            return false;
        };
        let fresh: Vec<(usize, &[u8])> = slices
            .iter()
            .copied()
            .filter(|(i, _)| growth.pending.remove(i))
            .collect();
        let start = growth.start;
        let done = growth.pending.is_empty();
        fold_into(
            &mut self.rows[start..],
            &self.exponents[start..],
            &self.constants,
            self.slice_size,
            &fresh,
        );
        if done {
            self.growth = None;
            self.generation += 1;
        }
        true
    }

    /// Whether rows are still being backfilled.
    pub fn growing(&self) -> bool {
        self.growth.is_some()
    }

    /// Drop the rows of the growth `generation`, if it is still unfinished.
    pub fn abandon_growth_of(&mut self, generation: u64) {
        if generation == self.generation {
            self.abandon_growth();
        }
    }

    /// Drop the rows of an unfinished growth.
    pub fn abandon_growth(&mut self) {
        if let Some(growth) = self.growth.take() {
            self.rows.truncate(growth.start);
            self.exponents.truncate(growth.start);
            self.generation += 1;
        }
    }

    /// Rows that count for [`solve`](Self::solve).
    pub fn complete_rows(&self) -> usize {
        self.growth.as_ref().map_or(self.rows.len(), |g| g.start)
    }

    /// Fold every slice that is neither fed nor in `damaged`, reading it with
    /// `read`. These are slices the fold missed (written before it existed,
    /// or decided outside the network path).
    pub fn feed_missing<R>(&mut self, damaged: &[usize], mut read: R) -> Result<usize, FoldError>
    where
        R: FnMut(usize) -> std::io::Result<Vec<u8>>,
    {
        let mut is_damaged = vec![false; self.fed.len()];
        for &j in damaged {
            *is_damaged.get_mut(j).ok_or(FoldError::OutOfRange(j))? = true;
        }
        let todo: Vec<usize> = (0..self.fed.len())
            .filter(|i| !self.fed[*i] && !is_damaged[*i])
            .collect();
        let slice_size = self.slice_size;
        let mut fed = 0;
        let mut fold_err = None;
        for_each_batch(&todo, slice_size, &mut read, |batch| {
            let refs: Vec<(usize, &[u8])> = batch.iter().map(|(i, b)| (*i, b.as_slice())).collect();
            match self.feed_batch(&refs) {
                Ok(n) => fed += n,
                Err(e) => fold_err = Some(e),
            }
        })?;
        if let Some(e) = fold_err {
            return Err(e);
        }
        self.disk_fed += fed;
        Ok(fed)
    }

    /// Solve for the `damaged` slices (global indices, in any order).
    ///
    /// `recovery` maps exponent → recovery slice data. Every slice outside
    /// `damaged` must be fed first (see [`feed_missing`](Self::feed_missing)).
    /// Returns `(global index, slice_size bytes)` per damaged slice, in the
    /// order given. The fold is left untouched, so a failed solve can be
    /// retried after [`extend_rows`](Self::extend_rows).
    pub fn solve(
        &self,
        damaged: &[usize],
        recovery: &HashMap<u32, Vec<u8>>,
    ) -> Result<Vec<(usize, Vec<u8>)>, FoldError> {
        if self.poisoned || damaged.iter().any(|j| self.is_fed(*j)) {
            return Err(FoldError::Poisoned);
        }
        if let Some(&j) = damaged.iter().find(|j| **j >= self.fed.len()) {
            return Err(FoldError::OutOfRange(j));
        }
        let d = damaged.len();
        if self.fed_count + d != self.fed.len() {
            // Some intact slice is not in the sums: the caller must feed it.
            return Err(FoldError::NotEnoughRows {
                needed: d,
                usable: 0,
            });
        }
        if d == 0 {
            return Ok(Vec::new());
        }
        let complete = self.growth.as_ref().map_or(self.rows.len(), |g| g.start);
        let usable: Vec<usize> = (0..complete)
            .filter(|r| {
                recovery
                    .get(&self.exponents[*r])
                    .is_some_and(|data| data.len() == self.slice_size)
            })
            .take(d)
            .collect();
        if usable.len() < d {
            return Err(FoldError::NotEnoughRows {
                needed: d,
                usable: usable.len(),
            });
        }
        let mut matrix = GfMatrix::zeros(d, d);
        for (e, &r) in usable.iter().enumerate() {
            for (j, &idx) in damaged.iter().enumerate() {
                matrix.set(e, j, gf::pow(self.constants[idx], self.exponents[r]));
            }
        }
        let inverse = matrix.invert().ok_or(FoldError::Singular)?;
        // out_j = Σ_e inv[j][e]·(row_e ⊕ R_e) = Σ_e inv[j][e]·row_e ⊕
        // Σ_e inv[j][e]·R_e: two passes, no copy of the rows.
        let rows: Vec<&[u8]> = usable.iter().map(|&r| self.rows[r].as_slice()).collect();
        let recs: Vec<&[u8]> = usable
            .iter()
            .map(|&r| recovery[&self.exponents[r]].as_slice())
            .collect();
        Ok(damaged
            .iter()
            .enumerate()
            .map(|(j, &idx)| {
                let mut out = vec![0u8; self.slice_size];
                let coeffs: Vec<u16> = (0..d).map(|e| inverse.get(j, e)).collect();
                mul_add_multi(&mut out, &rows, &coeffs);
                mul_add_multi(&mut out, &recs, &coeffs);
                (idx, out)
            })
            .collect())
    }
}

/// Fold `slices` into `rows` (row `r` has exponent `exponents[r]`).
fn fold_into(
    rows: &mut [Vec<u8>],
    exponents: &[u32],
    constants: &[u16],
    slice_size: usize,
    slices: &[(usize, &[u8])],
) {
    // Short slices are zero-padded, as rust-par2 pads them when it reads.
    let padded: Vec<Vec<u8>> = slices
        .iter()
        .filter(|(_, b)| b.len() < slice_size)
        .map(|(_, b)| {
            let mut v = b.to_vec();
            v.resize(slice_size, 0);
            v
        })
        .collect();
    let mut padded_iter = padded.iter();
    let srcs: Vec<&[u8]> = slices
        .iter()
        .map(|(_, b)| {
            if b.len() < slice_size {
                padded_iter.next().map(Vec::as_slice).unwrap_or(b)
            } else {
                &b[..slice_size]
            }
        })
        .collect();
    for (row, &exp) in rows.iter_mut().zip(exponents) {
        let coeffs: Vec<u16> = slices
            .iter()
            .map(|(i, _)| gf::pow(constants[*i], exp))
            .collect();
        mul_add_multi(row, &srcs, &coeffs);
    }
}

/// Read `indices` in batches bounded by [`MAX_REPAIR_BATCH`] slices and
/// [`MAX_BATCH_BYTES`], handing each batch to `apply`.
fn for_each_batch<R, A>(
    indices: &[usize],
    slice_size: usize,
    read: &mut R,
    mut apply: A,
) -> Result<(), FoldError>
where
    R: FnMut(usize) -> std::io::Result<Vec<u8>>,
    A: FnMut(&mut Vec<(usize, Vec<u8>)>),
{
    let per_batch = batch_len(slice_size as u64);
    for chunk in indices.chunks(per_batch) {
        let mut batch = Vec::with_capacity(chunk.len());
        for &index in chunk {
            let bytes = read(index).map_err(|e| FoldError::Read {
                index,
                message: e.to_string(),
            })?;
            batch.push((index, bytes));
        }
        apply(&mut batch);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn growth_backfills_in_batches_while_feeding_continues() {
        let (n, slice) = (12, 32);
        let d = data(n, slice);
        let rec = encode(&d, slice, &[0, 1, 2, 3]);
        let damaged = [1usize, 6, 10];
        let mut fold = RepairFold::new(n, slice as u64, &[0]).unwrap();
        for i in [0, 2, 3, 4] {
            fold.feed(i, &d[i]).unwrap();
        }
        let ticket = fold.begin_growth(&[0, 1, 2, 3]).unwrap().unwrap();
        assert_eq!(ticket.slices, vec![0, 2, 3, 4]);
        // A second growth waits for the first.
        assert_eq!(fold.begin_growth(&[5]).unwrap(), None);
        // New slices land in every row, the old ones are backfilled.
        for i in [5, 7, 8, 9, 11] {
            fold.feed(i, &d[i]).unwrap();
        }
        assert_eq!(fold.complete_rows(), 1);
        assert!(matches!(
            fold.solve(&damaged, &rec),
            Err(FoldError::NotEnoughRows {
                needed: 3,
                usable: 1
            })
        ));
        assert!(fold.backfill(ticket.generation, &[(0, &d[0]), (2, &d[2])]));
        assert!(fold.growing());
        assert!(fold.backfill(ticket.generation, &[(3, &d[3]), (4, &d[4])]));
        assert!(!fold.growing());
        assert_eq!(fold.complete_rows(), 4);
        // A stale ticket is ignored.
        assert!(!fold.backfill(ticket.generation, &[(0, &d[0])]));
        for (idx, bytes) in fold.solve(&damaged, &rec).unwrap() {
            assert_eq!(&bytes[..d[idx].len()], &d[idx][..]);
        }
    }

    /// WI-1236: the download engine's disk feed is counted per slice, and a
    /// second read of the same slice is visible as a re-read.
    #[test]
    fn disk_reads_are_counted_once_per_slice() {
        let (n, slice) = (6, 16);
        let mut fold = RepairFold::new(n, slice as u64, &[0]).unwrap();
        assert_eq!((fold.disk_reads(), fold.disk_rereads()), (0, 0));
        fold.note_disk_read(2, slice as u64);
        fold.note_disk_read(4, slice as u64);
        assert_eq!(fold.disk_reads(), 2);
        assert_eq!(fold.disk_rereads(), 0, "first reads are not re-reads");
        assert_eq!(fold.disk_read_bytes(), 2 * slice as u64);
        // The same slice read again for a growth's new rows.
        fold.note_disk_read(2, slice as u64);
        assert_eq!(fold.disk_reads(), 3);
        assert_eq!(fold.disk_rereads(), 1);
        // The counter is independent of what was folded, so a read the caller
        // discarded (already fed by a worker) is still visible.
        assert_eq!(fold.fed_count(), 0);
    }

    #[test]
    fn abandoned_growth_leaves_the_old_rows() {
        let (n, slice) = (6, 16);
        let d = data(n, slice);
        let mut fold = RepairFold::new(n, slice as u64, &[0]).unwrap();
        fold.feed(0, &d[0]).unwrap();
        let ticket = fold.begin_growth(&[1, 2]).unwrap().unwrap();
        fold.abandon_growth();
        assert_eq!(fold.rows(), 1);
        assert!(!fold.backfill(ticket.generation, &[(0, &d[0])]));
    }

    /// Encode recovery slices the way PAR2 defines them.
    fn encode(data: &[Vec<u8>], slice: usize, exps: &[u32]) -> HashMap<u32, Vec<u8>> {
        let c = par2_input_constants(data.len());
        exps.iter()
            .map(|&e| {
                let mut out = vec![0u8; slice];
                for (i, d) in data.iter().enumerate() {
                    let mut p = d.clone();
                    p.resize(slice, 0);
                    rust_par2::gf_simd::mul_add_buffer(&mut out, &p, gf::pow(c[i], e));
                }
                (e, out)
            })
            .collect()
    }

    fn data(n: usize, slice: usize) -> Vec<Vec<u8>> {
        (0..n)
            .map(|i| {
                let len = if i == n - 1 { slice / 2 } else { slice };
                (0..len).map(|b| (b * 7 + i * 131 + 3) as u8).collect()
            })
            .collect()
    }

    #[test]
    fn solves_damage_fed_in_any_order() {
        let (n, slice) = (12, 64);
        let d = data(n, slice);
        let rec = encode(&d, slice, &[0, 1, 2, 3]);
        let damaged = [2usize, 11, 5];
        let mut fold = RepairFold::new(n, slice as u64, &[0, 1, 2, 3]).unwrap();
        for i in (0..n).rev().filter(|i| !damaged.contains(i)) {
            assert!(fold.feed(i, &d[i]).unwrap());
        }
        // Feeding twice is a no-op.
        assert!(!fold.feed(0, &d[0]).unwrap());
        let out = fold.solve(&damaged, &rec).unwrap();
        for (idx, bytes) in out {
            assert_eq!(&bytes[..d[idx].len()], &d[idx][..], "slice {idx}");
        }
    }

    #[test]
    fn disk_feed_and_row_extension_reach_the_same_answer() {
        let (n, slice) = (10, 32);
        let d = data(n, slice);
        let rec = encode(&d, slice, &[0, 1, 2, 3, 4]);
        let mut fold = RepairFold::new(n, slice as u64, &[0]).unwrap();
        // Only the second half arrived through the network path.
        for (i, slice) in d.iter().enumerate().skip(5) {
            fold.feed(i, slice).unwrap();
        }
        let damaged = [1usize, 3, 8];
        let mut read = |i: usize| Ok(d[i].clone());
        // Slice 8 was fed before it was found damaged: the fold is poisoned.
        fold.note_corrupt(8);
        assert_eq!(fold.solve(&damaged, &rec), Err(FoldError::Poisoned));

        let mut fold = RepairFold::new(n, slice as u64, &[0]).unwrap();
        for i in [5, 6, 7, 9] {
            fold.feed(i, &d[i]).unwrap();
        }
        assert_eq!(fold.feed_missing(&damaged, &mut read).unwrap(), 3);
        assert_eq!(fold.disk_fed(), 3);
        assert!(matches!(
            fold.solve(&damaged, &rec),
            Err(FoldError::NotEnoughRows {
                needed: 3,
                usable: 1
            })
        ));
        assert_eq!(fold.extend_rows(&[0, 2, 4], &mut read).unwrap(), 2);
        let out = fold.solve(&damaged, &rec).unwrap();
        for (idx, bytes) in out {
            assert_eq!(&bytes[..d[idx].len()], &d[idx][..]);
        }
    }

    #[test]
    fn rows_without_recovery_data_are_skipped() {
        let (n, slice) = (6, 16);
        let d = data(n, slice);
        // Only exponents 1 and 3 are on disk.
        let rec = encode(&d, slice, &[1, 3]);
        let mut fold = RepairFold::new(n, slice as u64, &[0, 1, 2, 3]).unwrap();
        for i in [0, 2, 3, 5] {
            fold.feed(i, &d[i]).unwrap();
        }
        let out = fold.solve(&[4, 1], &rec).unwrap();
        assert_eq!(out[0].0, 4);
        assert_eq!(&out[0].1[..], &d[4][..]);
        assert_eq!(&out[1].1[..], &d[1][..]);
    }

    /// WI-1237: a caller-tightened row cap stops a growth, which is what
    /// makes post-processing fall back to the disk repair.
    #[test]
    fn a_caller_row_cap_stops_growth() {
        let (n, slice) = (8, 16);
        let mut fold = RepairFold::new(n, slice as u64, &[0])
            .unwrap()
            .with_row_limit(2);
        assert_eq!(fold.extend_rows(&[1], &mut |_| unreachable!()).unwrap(), 1);
        assert_eq!(
            fold.extend_rows(&[2], &mut |_| unreachable!()),
            Err(FoldError::NoRows)
        );
        assert_eq!(fold.rows(), 2, "a refused growth leaves the fold as it was");
        // The cap can only tighten: a large value keeps the global budget.
        let unbounded = RepairFold::new(4, slice as u64, &[0])
            .unwrap()
            .with_row_limit(usize::MAX);
        assert_eq!(unbounded.row_limit(), max_rows(slice as u64));
    }

    #[test]
    fn budget_is_enforced() {
        assert_eq!(
            RepairFold::new(4, MAX_FOLD_SLICE_SIZE + 1, &[0]).unwrap_err(),
            FoldError::SliceTooLarge(MAX_FOLD_SLICE_SIZE + 1)
        );
        assert_eq!(max_rows(MAX_FOLD_SLICE_SIZE), 8);
        let exps: Vec<u32> = (0..9).collect();
        assert_eq!(
            RepairFold::new(4, MAX_FOLD_SLICE_SIZE, &exps).unwrap_err(),
            FoldError::NoRows
        );
    }
}
