//! In-flight repair fold state (WI-1235).
//!
//! [`nzb_decode::RepairFold`] (WI-148) is the arithmetic: bounded rows of
//! XOR-accumulated intact-slice contributions. This module is the *state*
//! around it that a download engine can hand slices to while the job runs:
//!
//! - the set's per-slice IFSC digests, so a slice is only ever folded when its
//!   digest matches what PAR2 says it should be;
//! - the damaged set, which grows when a slice is refused or found corrupt;
//! - the recovery rows, sized to the damage within a memory budget and
//!   extensible at job end when late damage outgrows them;
//! - the recovery volumes' data, fed as they land, and [`RepairFold::solve`].
//!
//! A refused slice is **never** folded. Its contribution would be wrong, and
//! XOR cannot take it back out, so the alternative — folding first and
//! detecting the corruption later — poisons every row it touched. Instead the
//! slice is marked `Unknown` and joins the damaged set, which leaves the fold
//! solvable: the slice becomes one of the unknowns the recovery rows solve
//! for. [`RepairFold::note_corrupt_slice`] covers the other order, where a
//! slice was folded and is only later found bad; that one does poison the
//! fold, exactly like zurg's `noteCorruptSlice`.
//!
//! Memory: `rows × slice_size`, never more than [`MAX_ROW_BYTES`] and never
//! with a slice over [`MAX_SLICE_SIZE`], plus 20 bytes per slice of digest and
//! the recovery slices the caller feeds. Recovery is retained only up to the
//! number of rows the budget could ever open ([`RepairBudget::max_rows`]): a
//! fold can never use more rows than that, so retaining more recovery data
//! would be memory no solve could reach. The worst case is therefore
//! `2 × row_bytes`, not `recovery_blocks × slice_size`.
//!
//! Recovery volumes are untrusted input: a packet's declared length is bounds
//! checked without overflowing, packets that are not 4-aligned are rejected,
//! and the packet MD5 is verified before its data is kept. Solved slices are
//! checked against the IFSC digest the set already holds before they are
//! returned, so a corrupt recovery slice produces an error and the caller can
//! use the disk path rather than writing wrong bytes.
//!
//! There is no download-engine integration here; that is the next child.

use std::collections::{BTreeSet, HashMap};
use std::fmt;

use md5::Digest;

use nzb_decode::repair_fold::{MAX_FOLD_SLICE_SIZE, MAX_REPAIR_ROW_BYTES, RepairFold as RowFold};
use nzb_decode::{FoldError as RowFailure, SliceDigest, SliceState};
use rust_par2::Par2FileSet;

/// Most memory the recovery rows may hold: `rows × slice_size`.
pub const MAX_ROW_BYTES: u64 = MAX_REPAIR_ROW_BYTES;
/// Sets with larger slices are left to the disk repair path.
pub const MAX_SLICE_SIZE: u64 = MAX_FOLD_SLICE_SIZE;

const MAGIC: &[u8; 8] = b"PAR2\x00PKT";
const TYPE_RECOVERY: &[u8; 16] = b"PAR 2.0\x00RecvSlic";
const PACKET_HEADER: usize = 64;

/// How much memory the recovery rows may use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RepairBudget {
    /// Ceiling on `rows × slice_size`.
    pub row_bytes: u64,
    /// Largest slice the fold accepts at all.
    pub max_slice_size: u64,
}

impl Default for RepairBudget {
    fn default() -> Self {
        Self {
            row_bytes: MAX_ROW_BYTES,
            max_slice_size: MAX_SLICE_SIZE,
        }
    }
}

impl RepairBudget {
    /// How many rows of `slice_size` fit. Zero when the slice is too large.
    pub fn max_rows(&self, slice_size: u64) -> usize {
        if slice_size == 0 || slice_size > self.max_slice_size {
            0
        } else {
            (self.row_bytes / slice_size) as usize
        }
    }
}

/// What [`RepairFold::feed`] did with a slice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Feed {
    /// The digest matched: the slice is folded into every recovery row.
    Fed,
    /// The digest did not match: the slice is not folded and joins the
    /// damaged set, so the solve rebuilds it.
    Rejected,
    /// The slice was already folded.
    Duplicate,
    /// The set carries no IFSC checksum for the slice, so there is nothing to
    /// check it against. It is left `Unknown` and is neither folded nor solved
    /// for; the caller must not offer slices it cannot verify.
    Unverified,
}

/// Why the fold state could not be built, extended or solved.
#[derive(Debug, thiserror::Error)]
pub enum FoldError {
    #[error("slice size {0} is over the in-flight repair limit")]
    SliceTooLarge(u64),
    #[error("no recovery rows fit the memory budget")]
    NoRows,
    #[error("damage needs {needed} recovery rows, the budget allows {affordable}")]
    Budget { needed: usize, affordable: usize },
    #[error("slice {0} is outside the set")]
    OutOfRange(usize),
    #[error("slice {index} is {len} bytes, the set's slice is {expected}")]
    Length {
        index: usize,
        len: usize,
        expected: u64,
    },
    #[error("a slice folded into the rows was later found corrupt")]
    Poisoned,
    #[error("{0} slice(s) are neither fed nor damaged; feed them before solving")]
    Incomplete(usize),
    #[error("the rows were fixed at construction and cannot be replaced")]
    FixedExponents,
    #[error("solved slice {0} does not match its IFSC checksum")]
    CorruptOutput(usize),
    #[error("slice {0} has no IFSC checksum, so its solved bytes cannot be checked")]
    UnverifiedOutput(usize),
    #[error("recovery slice {0} is not slice-sized")]
    RecoveryLength(u32),
    #[error(transparent)]
    Rows(#[from] RowFailure),
}

/// Where a global slice lives, so the caller can read it back to feed it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SliceLocation<'a> {
    pub file: &'a str,
    pub offset: u64,
    /// Actual length; the last slice of a file is short.
    pub len: u64,
}

/// One global slice of the set, in PAR2 file order.
#[derive(Debug, Clone)]
struct Placed {
    file: String,
    offset: u64,
    len: u64,
    /// `None` when the set has no IFSC checksum for this slice.
    expected: Option<SliceDigest>,
}

/// Bounded repair rows fed by verified slices, solved at job end.
pub struct RepairFold {
    slice_size: usize,
    slices: Vec<Placed>,
    recovery_set_id: [u8; 16],
    /// Exponents the allocated rows carry, in row order.
    exponents: Vec<u32>,
    /// The caller named the exponents, so there are no more to grow into.
    explicit_exponents: bool,
    /// Recovery slices the set carries, a ceiling on rows.
    available: u32,
    budget: RepairBudget,
    rows: RowFold,
    states: Vec<SliceState>,
    damaged: BTreeSet<usize>,
    fed: Vec<bool>,
    recovery: HashMap<u32, Vec<u8>>,
}

impl fmt::Debug for RepairFold {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RepairFold")
            .field("slice_size", &self.slice_size)
            .field("slices", &self.slices.len())
            .field("rows", &self.exponents.len())
            .field("fed", &self.rows.fed_count())
            .field("damaged", &self.damaged.len())
            .field("recovery", &self.recovery.len())
            .finish()
    }
}

impl RepairFold {
    /// A fold for `set` that expects `damaged` slices to need rebuilding.
    ///
    /// `recovery_blocks` is how many recovery slices the set carries (from
    /// the volumes, which an index usually does not describe). Rows opened are
    /// the damaged count itself: that is what a D×D solve consumes. One row is
    /// always opened, so a fold that starts clean can still fold what it sees;
    /// damage after that grows the rows in step.
    ///
    /// Rows are opened on exponents `0..rows`. When the volumes use other
    /// exponents, [`solve`](Self::solve) gives up those rows
    /// (`NotEnoughRows` with `usable < needed`); [`extend_rows`](Self::extend_rows)
    /// then adds the exponents the volumes carry. [`solve_with`](Self::solve_with)
    /// does both at job end.
    ///
    /// Refuses a slice over [`MAX_SLICE_SIZE`], a set with no affordable row,
    /// and more damage than the budget can solve for.
    pub fn new(
        set: &Par2FileSet,
        damaged: &[usize],
        recovery_blocks: u32,
        budget: RepairBudget,
    ) -> Result<Self, FoldError> {
        let want = damaged.len().max(1);
        let exponents: Vec<u32> = (0..want as u32).collect();
        Self::with_rows(set, &exponents, damaged, recovery_blocks, budget, true)
    }

    /// A fold whose rows are `exponents`, in that order — for when the caller
    /// already knows which volumes it holds. No rows can be added later; the
    /// budget still bounds them.
    pub fn with_exponents(
        set: &Par2FileSet,
        exponents: &[u32],
        damaged: &[usize],
        budget: RepairBudget,
    ) -> Result<Self, FoldError> {
        let available = set.recovery_block_count.max(exponents.len() as u32);
        Self::with_rows(set, exponents, damaged, available, budget, false)
    }

    fn with_rows(
        set: &Par2FileSet,
        exponents: &[u32],
        damaged: &[usize],
        recovery_blocks: u32,
        budget: RepairBudget,
        growable: bool,
    ) -> Result<Self, FoldError> {
        let slices = layout(set);
        let slice_size = set.slice_size;
        for &j in damaged {
            if j >= slices.len() {
                return Err(FoldError::OutOfRange(j));
            }
        }
        if slice_size > budget.max_slice_size {
            return Err(FoldError::SliceTooLarge(slice_size));
        }
        let affordable = budget.max_rows(slice_size).min(recovery_blocks as usize);
        if affordable == 0 || exponents.is_empty() {
            return Err(FoldError::NoRows);
        }
        if damaged.len() > exponents.len() {
            return Err(FoldError::Budget {
                needed: damaged.len(),
                affordable: exponents.len(),
            });
        }
        if exponents.len() > affordable {
            return Err(FoldError::Budget {
                needed: exponents.len(),
                affordable,
            });
        }
        let rows = RowFold::new(slices.len(), slice_size, exponents)?;
        let n = slices.len();
        let mut fold = Self {
            slice_size: slice_size as usize,
            slices,
            recovery_set_id: set.recovery_set_id,
            exponents: exponents.to_vec(),
            explicit_exponents: !growable,
            available: recovery_blocks.max(exponents.len() as u32),
            budget,
            rows,
            states: vec![SliceState::Unknown; n],
            damaged: BTreeSet::new(),
            fed: vec![false; n],
            recovery: HashMap::new(),
        };
        for &j in damaged {
            fold.mark_damaged(j);
        }
        Ok(fold)
    }

    pub fn slice_size(&self) -> u64 {
        self.slice_size as u64
    }
    pub fn total_slices(&self) -> usize {
        self.slices.len()
    }
    /// Recovery rows allocated.
    pub fn rows(&self) -> usize {
        self.exponents.len()
    }
    pub fn exponents(&self) -> &[u32] {
        &self.exponents
    }
    /// Slices folded so far.
    pub fn fed_count(&self) -> usize {
        self.rows.fed_count()
    }
    /// Slices known to need rebuilding.
    pub fn damaged(&self) -> &BTreeSet<usize> {
        &self.damaged
    }
    /// Verdict per slice, in global index order.
    pub fn states(&self) -> &[SliceState] {
        &self.states
    }
    /// Bytes the rows hold: `rows × slice_size`.
    pub fn row_bytes(&self) -> u64 {
        self.rows.row_bytes()
    }
    /// Whether a slice folded earlier was later found corrupt. A poisoned
    /// fold cannot be solved; use the disk path.
    pub fn is_poisoned(&self) -> bool {
        self.rows.is_poisoned()
    }
    /// Recovery slices fed so far.
    pub fn recovery_count(&self) -> usize {
        self.recovery.len()
    }

    /// Where a global slice lives.
    pub fn locate(&self, global_index: usize) -> Option<SliceLocation<'_>> {
        self.slices.get(global_index).map(|p| SliceLocation {
            file: &p.file,
            offset: p.offset,
            len: p.len,
        })
    }

    /// Feed one slice of the set's data. The bytes are checked against the
    /// slice's IFSC digest first; only a match is folded. A mismatch marks the
    /// slice damaged and refuses it, which is the safe direction: the solve
    /// rebuilds it rather than poisoning every row it was folded into.
    ///
    /// `digest` is what the caller's own verification produced, when it has
    /// one; a digest that disagrees with the set refuses the slice even if the
    /// bytes happen to match the IFSC packet.
    pub fn feed(
        &mut self,
        global_index: usize,
        bytes: &[u8],
        digest: Option<&SliceDigest>,
    ) -> Result<Feed, FoldError> {
        let Some(placed) = self.slices.get(global_index) else {
            return Err(FoldError::OutOfRange(global_index));
        };
        if bytes.len() as u64 != placed.len {
            return Err(FoldError::Length {
                index: global_index,
                len: bytes.len(),
                expected: placed.len,
            });
        }
        if self.damaged.contains(&global_index) {
            return Ok(Feed::Rejected);
        }
        if self.fed[global_index] {
            return Ok(Feed::Duplicate);
        }
        let Some(expected) = placed.expected else {
            return Ok(Feed::Unverified);
        };
        if digest.is_some_and(|d| *d != expected) {
            self.mark_damaged(global_index);
            return Ok(Feed::Rejected);
        }
        if digest_of(bytes, self.slice_size) != expected {
            self.mark_damaged(global_index);
            return Ok(Feed::Rejected);
        }
        self.rows.feed(global_index, bytes)?;
        self.fed[global_index] = true;
        self.states[global_index] = SliceState::Verified;
        Ok(Feed::Fed)
    }

    /// Fold a slice without supplying a digest: it is checked against the
    /// set's own IFSC checksum. For callers that trust their transport.
    pub fn feed_verified(&mut self, global_index: usize, bytes: &[u8]) -> Result<Feed, FoldError> {
        self.feed(global_index, bytes, None)
    }

    /// A slice folded earlier turned out to be corrupt. XOR cannot take a
    /// contribution back out, so this poisons the fold — zurg's
    /// `noteCorruptSlice`. A slice that was never folded was never
    /// contributed, so it only joins the damaged set.
    pub fn note_corrupt_slice(&mut self, global_index: usize) -> Result<(), FoldError> {
        if global_index >= self.slices.len() {
            return Err(FoldError::OutOfRange(global_index));
        }
        if self.fed[global_index] {
            self.rows.note_corrupt(global_index);
        }
        self.mark_damaged(global_index);
        Ok(())
    }

    /// Mark a slice damaged without data: an article covering it was
    /// confirmed missing. Out-of-range indices are ignored.
    pub fn mark_damaged(&mut self, global_index: usize) {
        if global_index >= self.slices.len() {
            return;
        }
        self.damaged.insert(global_index);
        self.states[global_index] = SliceState::Damaged;
    }

    /// Give up an intact slice whose article never arrived (the state is
    /// unestablished rather than damaged): it cannot be folded, so the solve
    /// has to rebuild it too.
    pub fn mark_unknown(&mut self, global_index: usize) -> Result<(), FoldError> {
        if global_index >= self.slices.len() {
            return Err(FoldError::OutOfRange(global_index));
        }
        if !self.fed[global_index] {
            self.mark_damaged(global_index);
        }
        Ok(())
    }

    /// Feed a PAR2 recovery volume's bytes: every `RecvSlic` packet belonging
    /// to this set. Packets are keyed by exponent, so volumes may arrive in
    /// any order and a repeated exponent is ignored. Returns how many were
    /// added.
    ///
    /// Packets that are malformed, truncated, not 4-aligned, from another set
    /// or whose MD5 does not match are skipped: a damaged recovery article is
    /// exactly what the digest check is for, and skipping it leaves the solve
    /// short rather than folding wrong data into it.
    ///
    /// Retention is bounded by [`RepairBudget::max_rows`]: the rows can never
    /// exceed that, so exponents beyond it could never be used.
    pub fn feed_volume(&mut self, bytes: &[u8]) -> Result<usize, FoldError> {
        let mut added = 0;
        for (exponent, data) in recovery_packets(bytes, &self.recovery_set_id) {
            if data.len() != self.slice_size {
                return Err(FoldError::RecoveryLength(exponent));
            }
            if !self.retain_recovery(exponent) {
                continue;
            }
            if self.recovery.insert(exponent, data).is_none() {
                added += 1;
            }
        }
        Ok(added)
    }

    /// Whether a recovery slice may be kept: replacing one already held
    /// always is, a new one only while the retained set is below the most
    /// rows the budget can ever open.
    fn retain_recovery(&self, exponent: u32) -> bool {
        self.recovery.contains_key(&exponent) || self.recovery.len() < self.max_recovery_slices()
    }

    /// Most recovery slices retention can ever need: no more rows can exist.
    fn max_recovery_slices(&self) -> usize {
        self.budget
            .max_rows(self.slice_size as u64)
            .min(self.available as usize)
    }

    /// Bytes the retained recovery slices hold.
    pub fn recovery_bytes(&self) -> u64 {
        self.recovery.len() as u64 * self.slice_size as u64
    }

    /// Feed a recovery slice whose exponent the caller already knows.
    pub fn feed_recovery(&mut self, exponent: u32, data: &[u8]) -> Result<bool, FoldError> {
        if data.len() != self.slice_size {
            return Err(FoldError::RecoveryLength(exponent));
        }
        if !self.retain_recovery(exponent) {
            return Ok(false);
        }
        Ok(self.recovery.insert(exponent, data.to_vec()).is_none())
    }

    /// Add rows for `exponents`, folding every slice already fed into them.
    /// `read` returns a fed slice's bytes by global index; the fold keeps no
    /// slice data, so the new rows are rebuilt from the caller's source.
    ///
    /// Nothing changes when the new rows would break the budget or the caller
    /// fixed the exponents at construction.
    pub fn extend_rows<R>(&mut self, exponents: &[u32], read: R) -> Result<usize, FoldError>
    where
        R: FnMut(usize) -> std::io::Result<Vec<u8>>,
    {
        if self.explicit_exponents {
            return Err(FoldError::FixedExponents);
        }
        let new: Vec<u32> = exponents
            .iter()
            .copied()
            .filter(|e| !self.exponents.contains(e))
            .collect();
        if new.is_empty() {
            return Ok(0);
        }
        let total = self.exponents.len() + new.len();
        let affordable = self
            .budget
            .max_rows(self.slice_size as u64)
            .min(self.available as usize);
        if total > affordable {
            return Err(FoldError::Budget {
                needed: total,
                affordable,
            });
        }
        self.rows.extend_rows(&new, read)?;
        self.exponents.extend(new.iter());
        Ok(new.len())
    }

    /// Grow the rows to cover damage that arrived after [`new`](Self::new)
    /// sized them, folding the fed slices into the new rows with `read`.
    pub fn grow_to_damage<R>(&mut self, read: R) -> Result<usize, FoldError>
    where
        R: FnMut(usize) -> std::io::Result<Vec<u8>>,
    {
        let want = self.damaged.len();
        if want <= self.exponents.len() {
            return Ok(0);
        }
        let affordable = self
            .budget
            .max_rows(self.slice_size as u64)
            .min(self.available as usize);
        if want > affordable {
            return Err(FoldError::Budget {
                needed: want,
                affordable,
            });
        }
        let add: Vec<u32> = (self.exponents.len() as u32..want as u32).collect();
        self.extend_rows(&add, read)
    }

    /// Exponents the volumes carry that the rows do not, ascending.
    pub fn missing_row_exponents(&self) -> Vec<u32> {
        let mut exps: Vec<u32> = self
            .recovery
            .keys()
            .copied()
            .filter(|e| !self.exponents.contains(e))
            .collect();
        exps.sort_unstable();
        exps
    }

    /// Solve for the damaged slices. Returns `(global index, bytes)` per
    /// damaged slice, ascending, trimmed to the slice's real length. Every
    /// intact slice must have been fed: one neither fed nor damaged is an
    /// error, not a value to guess.
    ///
    /// Every solved slice is checked against its IFSC digest before it is
    /// returned. A mismatch means a recovery slice was wrong (or a row was
    /// built from wrong data): the error is [`FoldError::CorruptOutput`] and
    /// the caller must use the disk path rather than write the bytes.
    ///
    /// When the rows miss recovery data the volumes hold, the error is
    /// `FoldError::Rows(FoldError::NotEnoughRows { .. })`; add rows for
    /// [`missing_row_exponents`](Self::missing_row_exponents) and solve again.
    pub fn solve(&mut self) -> Result<Vec<(usize, Vec<u8>)>, FoldError> {
        let damaged: Vec<usize> = self.damaged.iter().copied().collect();
        if damaged.is_empty() {
            return Ok(Vec::new());
        }
        let missing = (0..self.slices.len())
            .filter(|i| !self.damaged.contains(i) && !self.fed[*i])
            .count();
        if missing > 0 {
            return Err(FoldError::Incomplete(missing));
        }
        let solved = self.rows.solve(&damaged, &self.recovery)?;
        let mut out = Vec::with_capacity(solved.len());
        for (index, bytes) in solved {
            let placed = &self.slices[index];
            let Some(expected) = placed.expected else {
                return Err(FoldError::UnverifiedOutput(index));
            };
            if digest_of(&bytes, self.slice_size) != expected {
                return Err(FoldError::CorruptOutput(index));
            }
            out.push((index, bytes[..placed.len as usize].to_vec()));
        }
        Ok(out)
    }

    /// Solve at job end, matching the rows to the job: grow to the damage,
    /// then replace rows whose exponent has no recovery data with exponents
    /// the volumes carry, then solve.
    ///
    /// Replacing is the point: rows opened on `0..D` cost D × slice_size of
    /// the budget even when the volumes use other exponents, so only adding
    /// would make a fold sized exactly at the budget fail although D rows
    /// would do. A dead row is dropped and a live one opened in its place, so
    /// the row count — and the memory — does not grow.
    ///
    /// `read` returns a fed slice's bytes by global index, for rebuilding the
    /// new rows. Growth is skipped when the rows are already enough, so a job
    /// whose damage was known up front reads nothing here.
    pub fn solve_with<R>(&mut self, mut read: R) -> Result<Vec<(usize, Vec<u8>)>, FoldError>
    where
        R: FnMut(usize) -> std::io::Result<Vec<u8>>,
    {
        if !self.explicit_exponents {
            self.grow_to_damage(&mut read)?;
            self.swap_dead_rows(&mut read)?;
        }
        match self.solve() {
            Err(FoldError::Rows(RowFailure::NotEnoughRows { needed, usable }))
                if usable < needed && !self.explicit_exponents =>
            {
                let mut exps = self.missing_row_exponents();
                exps.truncate(needed - usable);
                if exps.is_empty() {
                    return Err(FoldError::Rows(RowFailure::NotEnoughRows {
                        needed,
                        usable,
                    }));
                }
                self.extend_rows(&exps, &mut read)?;
                self.solve()
            }
            other => other,
        }
    }

    /// Replace rows whose exponent has no recovery data with exponents the
    /// volumes carry and the rows lack, keeping the row count the same.
    /// Rows that do have data are left alone, and so is a fold whose
    /// exponents were fixed at construction.
    fn swap_dead_rows<R>(&mut self, read: &mut R) -> Result<(), FoldError>
    where
        R: FnMut(usize) -> std::io::Result<Vec<u8>>,
    {
        // Rebuilding the rows would drop the poison flag, which is the only
        // record that a folded contribution is wrong. Leave a poisoned fold
        // as it is so the solve keeps refusing it.
        if self.explicit_exponents || self.rows.is_poisoned() {
            return Ok(());
        }
        let mut dead: Vec<u32> = self
            .exponents
            .iter()
            .copied()
            .filter(|e| !self.recovery.contains_key(e))
            .collect();
        if dead.is_empty() {
            return Ok(());
        }
        let mut live = self.missing_row_exponents();
        if live.is_empty() {
            return Ok(());
        }
        let mut exponents = self.exponents.clone();
        for slot in exponents.iter_mut() {
            if dead.is_empty() || live.is_empty() {
                break;
            }
            let Some(at) = dead.iter().position(|e| *e == *slot) else {
                continue;
            };
            dead.swap_remove(at);
            *slot = live.remove(0);
        }
        if exponents == self.exponents {
            return Ok(());
        }
        self.rows = RowFold::new(self.slices.len(), self.slice_size as u64, &exponents)?;
        let fed: Vec<usize> = (0..self.slices.len()).filter(|i| self.fed[*i]).collect();
        for index in fed {
            let bytes = read(index).map_err(|e| RowFailure::Read {
                index,
                message: e.to_string(),
            })?;
            self.rows.feed(index, &bytes)?;
        }
        self.exponents = exponents;
        Ok(())
    }
}

/// Every global slice in PAR2 order, with its IFSC digest when the set has one.
fn layout(set: &Par2FileSet) -> Vec<Placed> {
    let mut out = Vec::new();
    for id in &set.file_order {
        let Some(file) = set.files.get(id) else {
            continue;
        };
        let count = if set.slice_size == 0 {
            0
        } else {
            file.size.div_ceil(set.slice_size) as usize
        };
        for i in 0..count {
            let offset = i as u64 * set.slice_size;
            let len = (file.size - offset).min(set.slice_size);
            out.push(Placed {
                file: file.filename.clone(),
                offset,
                len,
                expected: file.slices.get(i).map(|s| SliceDigest {
                    md5: s.md5,
                    crc32: s.crc32,
                }),
            });
        }
    }
    out
}

/// A slice's IFSC digest: MD5 and CRC32 over the bytes zero-padded to the
/// slice size, as PAR2 computes them.
fn digest_of(bytes: &[u8], slice_size: usize) -> SliceDigest {
    let mut padded = Vec::with_capacity(slice_size);
    padded.extend_from_slice(bytes);
    padded.resize(slice_size, 0);
    SliceDigest {
        md5: md5::Md5::digest(&padded).into(),
        crc32: crc32fast::hash(&padded),
    }
}

/// `(exponent, data)` for every `RecvSlic` packet in `bytes` that belongs to
/// `set_id` and whose packet MD5 matches.
///
/// This parses untrusted input. A packet's declared length is added with
/// [`usize::checked_add`], lengths that are not 4-aligned or are shorter than
/// the header are rejected, and a packet whose MD5 (bytes 16..32, over
/// everything from byte 32) does not match is skipped. Skipping is the safe
/// direction: the solve then has too few rows and falls back to disk, instead
/// of folding wrong recovery data into the answer.
fn recovery_packets(bytes: &[u8], set_id: &[u8; 16]) -> Vec<(u32, Vec<u8>)> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    while pos + PACKET_HEADER <= bytes.len() {
        if &bytes[pos..pos + 8] != MAGIC {
            pos += 1;
            continue;
        }
        let declared = u64::from_le_bytes(bytes[pos + 8..pos + 16].try_into().unwrap());
        let Ok(len) = usize::try_from(declared) else {
            break;
        };
        let Some(end) = pos.checked_add(len) else {
            break;
        };
        if len < PACKET_HEADER || !len.is_multiple_of(4) || end > bytes.len() {
            break;
        }
        let packet = &bytes[pos..end];
        let digest = &packet[16..32];
        let body = &packet[32..];
        if md5::Md5::digest(body).as_slice() != digest {
            pos = end;
            continue;
        }
        let id = &packet[32..48];
        let kind = &packet[48..64];
        if id == set_id && kind == TYPE_RECOVERY && packet.len() >= PACKET_HEADER + 4 {
            let exponent = u32::from_le_bytes(packet[64..68].try_into().unwrap());
            out.push((exponent, packet[68..].to_vec()));
        }
        pos = end;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use mock_nntp_server::damage::{DamageRelease, DamageScenario, seeded_bytes};
    use mock_nntp_server::par2::Par2Fixture;
    use std::path::Path;

    /// Write the release's PAR2 index to `dir` and parse it back.
    fn set_of(release: &DamageRelease, dir: &Path) -> Par2FileSet {
        release.write_par2(dir);
        let index = dir.join(&release.par2_index.filename);
        rust_par2::parse(&index).expect("the fixture's index parses")
    }

    /// A `Par2Fixture` over `files`, written and parsed like a real index.
    fn set_from(fixture: &Par2Fixture, dir: &Path) -> Par2FileSet {
        let path = dir.join("fold.par2");
        fixture.write_index(&path);
        rust_par2::parse(&path).expect("the fixture's index parses")
    }

    /// The intact bytes of every global slice, in PAR2 order.
    fn slices(release: &DamageRelease) -> Vec<Vec<u8>> {
        let size = release.par2.slice_size() as usize;
        let mut out = vec![Vec::new(); release.par2.total_slices() as usize];
        for file in &release.data_files {
            let Some(first) = release.par2.first_global_slice(&file.filename) else {
                continue;
            };
            for (i, chunk) in file.payload.chunks(size).enumerate() {
                out[first as usize + i] = chunk.to_vec();
            }
        }
        out
    }

    /// The global slices the `MissingEverywhere` scenario damages, and a
    /// release built the same way the scenario builds it.
    fn missing_everywhere() -> (DamageRelease, BTreeSet<usize>) {
        let fixture = DamageScenario::MissingEverywhere.fixture();
        let damaged = fixture
            .expect
            .damaged_slices
            .iter()
            .map(|i| *i as usize)
            .collect();
        (fixture.release, damaged)
    }

    #[test]
    fn solves_missing_everywhere_from_the_verified_slices() {
        let dir = tempfile::tempdir().unwrap();
        let (release, damaged) = missing_everywhere();
        assert!(!damaged.is_empty(), "the scenario seeds damage");
        let set = set_of(&release, dir.path());
        let data = slices(&release);
        let mut fold = RepairFold::new(
            &set,
            &damaged.iter().copied().collect::<Vec<_>>(),
            release.recovery_blocks(),
            RepairBudget::default(),
        )
        .unwrap();

        // Feed every slice the download kept; the damaged ones never arrived.
        for (gi, bytes) in data.iter().enumerate() {
            if damaged.contains(&gi) {
                continue;
            }
            assert_eq!(
                fold.feed_verified(gi, bytes).unwrap(),
                Feed::Fed,
                "slice {gi}"
            );
        }
        assert_eq!(fold.damaged().len(), damaged.len());
        assert!(!fold.is_poisoned());

        for volume in &release.volumes {
            fold.feed_volume(&volume.file.payload).unwrap();
        }
        assert_eq!(fold.recovery_count(), release.recovery_blocks() as usize);

        let repaired = fold.solve_with(|gi| Ok(data[gi].clone())).unwrap();
        assert_eq!(repaired.len(), damaged.len());
        for (gi, bytes) in repaired {
            assert!(damaged.contains(&gi), "slice {gi} was not damaged");
            assert_eq!(bytes, data[gi], "slice {gi} did not round-trip");
        }
    }

    #[test]
    fn a_corrupt_slice_offered_as_intact_is_rejected_by_its_digest() {
        let dir = tempfile::tempdir().unwrap();
        let (release, damaged) = missing_everywhere();
        let set = set_of(&release, dir.path());
        let data = slices(&release);
        let victim = (0..data.len()).find(|i| !damaged.contains(i)).unwrap();

        let mut fold = RepairFold::new(
            &set,
            &damaged.iter().copied().collect::<Vec<_>>(),
            release.recovery_blocks(),
            RepairBudget::default(),
        )
        .unwrap();

        // The bytes are the right length and claim to be intact; one bit is
        // wrong, which only the IFSC digest catches.
        let mut corrupt = data[victim].clone();
        corrupt[0] ^= 0x01;
        assert_eq!(
            fold.feed_verified(victim, &corrupt).unwrap(),
            Feed::Rejected
        );
        assert!(fold.damaged().contains(&victim));
        assert_eq!(fold.states()[victim], SliceState::Damaged);
        assert_eq!(fold.fed_count(), 0, "a rejected slice is never folded");

        // The fold stays solvable: the refused slice is rebuilt, not poisoned.
        for (gi, bytes) in data.iter().enumerate() {
            if damaged.contains(&gi) || gi == victim {
                continue;
            }
            fold.feed_verified(gi, bytes).unwrap();
        }
        for volume in &release.volumes {
            fold.feed_volume(&volume.file.payload).unwrap();
        }
        assert!(!fold.is_poisoned());
        let repaired: HashMap<usize, Vec<u8>> = fold
            .solve_with(|gi| Ok(data[gi].clone()))
            .unwrap()
            .into_iter()
            .collect();
        assert_eq!(repaired[&victim], data[victim]);
        for gi in &damaged {
            assert_eq!(repaired[gi], data[*gi], "slice {gi}");
        }
    }

    #[test]
    fn a_slice_fed_before_it_is_found_corrupt_poisons_the_fold() {
        let dir = tempfile::tempdir().unwrap();
        let (release, damaged) = missing_everywhere();
        let set = set_of(&release, dir.path());
        let data = slices(&release);
        let victim = (0..data.len()).find(|i| !damaged.contains(i)).unwrap();

        let mut fold = RepairFold::new(
            &set,
            &damaged.iter().copied().collect::<Vec<_>>(),
            release.recovery_blocks(),
            RepairBudget::default(),
        )
        .unwrap();
        assert_eq!(
            fold.feed_verified(victim, &data[victim]).unwrap(),
            Feed::Fed
        );
        assert_eq!(
            fold.feed_verified(victim, &data[victim]).unwrap(),
            Feed::Duplicate
        );

        // Too late to refuse it: its contribution is already in every row.
        fold.note_corrupt_slice(victim).unwrap();
        assert!(fold.is_poisoned());
        for volume in &release.volumes {
            fold.feed_volume(&volume.file.payload).unwrap();
        }
        for (gi, bytes) in data.iter().enumerate() {
            if damaged.contains(&gi) || gi == victim {
                continue;
            }
            fold.feed_verified(gi, bytes).unwrap();
        }
        assert!(matches!(
            fold.solve(),
            Err(FoldError::Rows(RowFailure::Poisoned))
        ));

        // The same slice, marked corrupt without ever being folded, only
        // joins the damaged set.
        let mut fold = RepairFold::new(
            &set,
            &damaged.iter().copied().collect::<Vec<_>>(),
            release.recovery_blocks(),
            RepairBudget::default(),
        )
        .unwrap();
        fold.note_corrupt_slice(victim).unwrap();
        assert!(!fold.is_poisoned());
    }

    /// Rebuilding the rows at job end must not drop the poison flag: a fold
    /// with a wrong contribution stays unusable even through `solve_with`.
    #[test]
    fn solve_with_keeps_a_poisoned_fold_poisoned() {
        let dir = tempfile::tempdir().unwrap();
        let fixture = Par2Fixture::new()
            .with_slice_size(4096)
            .add_file("poison.bin", &seeded_bytes(8, 4096 * 8));
        let set = set_from(&fixture, dir.path());
        let data: Vec<Vec<u8>> = seeded_bytes(8, 4096 * 8)
            .chunks(4096)
            .map(|c| c.to_vec())
            .collect();
        let damaged = [1usize];
        let mut fold = RepairFold::new(&set, &damaged, 4, RepairBudget::default()).unwrap();
        for (gi, bytes) in data.iter().enumerate() {
            if !damaged.contains(&gi) {
                fold.feed_verified(gi, bytes).unwrap();
            }
        }
        fold.note_corrupt_slice(0).unwrap();
        // Rows on 0..1 are dead against this volume, so `solve_with` would
        // swap them out and rebuild the rows if the poison were forgotten.
        fold.feed_volume(&fixture.volume_bytes(&[4, 5, 6, 7]))
            .unwrap();
        assert!(matches!(
            fold.solve_with(|gi| Ok(data[gi].clone())),
            Err(FoldError::Rows(RowFailure::Poisoned))
        ));
    }

    #[test]
    fn a_slice_that_does_not_fit_the_set_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let (release, damaged) = missing_everywhere();
        let set = set_of(&release, dir.path());
        let data = slices(&release);
        let mut fold = RepairFold::new(
            &set,
            &damaged.iter().copied().collect::<Vec<_>>(),
            release.recovery_blocks(),
            RepairBudget::default(),
        )
        .unwrap();

        let short = data[0].len() - 1;
        assert!(matches!(
            fold.feed_verified(0, &data[0][..short]),
            Err(FoldError::Length { index: 0, .. })
        ));
        assert!(matches!(
            fold.feed_verified(data.len(), &data[0]),
            Err(FoldError::OutOfRange(_))
        ));
        assert!(matches!(
            fold.note_corrupt_slice(data.len()),
            Err(FoldError::OutOfRange(_))
        ));
    }

    #[test]
    fn solving_needs_every_intact_slice() {
        let dir = tempfile::tempdir().unwrap();
        let (release, damaged) = missing_everywhere();
        let set = set_of(&release, dir.path());
        let data = slices(&release);
        let mut fold = RepairFold::new(
            &set,
            &damaged.iter().copied().collect::<Vec<_>>(),
            release.recovery_blocks(),
            RepairBudget::default(),
        )
        .unwrap();
        let skip = (0..data.len()).find(|i| !damaged.contains(i)).unwrap();
        for (gi, bytes) in data.iter().enumerate() {
            if damaged.contains(&gi) || gi == skip {
                continue;
            }
            fold.feed_verified(gi, bytes).unwrap();
        }
        for volume in &release.volumes {
            fold.feed_volume(&volume.file.payload).unwrap();
        }
        assert!(matches!(fold.solve(), Err(FoldError::Incomplete(1))));

        // Giving up on it makes it something the solve rebuilds instead; the
        // rows grow to the added damage at job end.
        fold.mark_unknown(skip).unwrap();
        let repaired: HashMap<usize, Vec<u8>> = fold
            .solve_with(|gi| Ok(data[gi].clone()))
            .unwrap()
            .into_iter()
            .collect();
        assert_eq!(repaired[&skip], data[skip]);
    }

    #[test]
    fn a_slice_over_the_slice_limit_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let fixture = Par2Fixture::new()
            .with_slice_size(MAX_SLICE_SIZE + 4)
            .add_file("big.bin", &seeded_bytes(1, 2 * MAX_SLICE_SIZE as usize));
        let set = set_from(&fixture, dir.path());
        assert!(matches!(
            RepairFold::new(&set, &[], 4, RepairBudget::default()),
            Err(FoldError::SliceTooLarge(_))
        ));
        // A caller may lower the limit further.
        let fixture = Par2Fixture::new()
            .with_slice_size(4096)
            .add_file("small.bin", &seeded_bytes(2, 8192));
        let set = set_from(&fixture, dir.path());
        let budget = RepairBudget {
            row_bytes: MAX_ROW_BYTES,
            max_slice_size: 2048,
        };
        assert!(matches!(
            RepairFold::new(&set, &[], 4, budget),
            Err(FoldError::SliceTooLarge(4096))
        ));
    }

    #[test]
    fn rows_that_do_not_fit_the_byte_budget_are_refused() {
        let fixture = Par2Fixture::new()
            .with_slice_size(4096)
            .add_file("small.bin", &seeded_bytes(3, 4096 * 12));
        let dir = tempfile::tempdir().unwrap();
        let set = set_from(&fixture, dir.path());

        // Room for 8 rows only; 9 rows of damage cannot be solved.
        let budget = RepairBudget {
            row_bytes: 8 * 4096,
            max_slice_size: MAX_SLICE_SIZE,
        };
        assert!(matches!(
            RepairFold::new(&set, &[0, 1, 2, 3, 4, 5, 6, 7, 8], 16, budget),
            Err(FoldError::Budget { affordable: 8, .. })
        ));
        // No row at all.
        assert!(matches!(
            RepairFold::new(
                &set,
                &[],
                16,
                RepairBudget {
                    row_bytes: 0,
                    max_slice_size: MAX_SLICE_SIZE
                }
            ),
            Err(FoldError::NoRows)
        ));
        // A set that carries no recovery slices can never be solved.
        assert!(matches!(
            RepairFold::new(&set, &[], 0, RepairBudget::default()),
            Err(FoldError::NoRows)
        ));
        // Exactly at the budget is fine.
        assert!(RepairFold::new(&set, &[0, 1, 2, 3, 4, 5, 6, 7], 16, budget).is_ok());
    }

    #[test]
    fn late_damage_grows_the_rows_within_the_budget() {
        let dir = tempfile::tempdir().unwrap();
        let (release, damaged) = missing_everywhere();
        let set = set_of(&release, dir.path());
        let data = slices(&release);
        // Start with a single row; the missing slices arrive as damage.
        let mut fold = RepairFold::new(
            &set,
            &[],
            release.recovery_blocks(),
            RepairBudget::default(),
        )
        .unwrap();
        assert_eq!(fold.rows(), 1);
        for (gi, bytes) in data.iter().enumerate() {
            if damaged.contains(&gi) {
                fold.mark_damaged(gi);
                continue;
            }
            fold.feed_verified(gi, bytes).unwrap();
        }
        let mut read = |gi: usize| Ok(data[gi].clone());
        fold.grow_to_damage(&mut read).unwrap();
        assert_eq!(fold.rows(), damaged.len());
        assert!(fold.row_bytes() <= MAX_ROW_BYTES);

        for volume in &release.volumes {
            fold.feed_volume(&volume.file.payload).unwrap();
        }
        let repaired: HashMap<usize, Vec<u8>> = fold.solve().unwrap().into_iter().collect();
        for gi in &damaged {
            assert_eq!(repaired[gi], data[*gi], "slice {gi}");
        }

        // A budget too small to grow into refuses the growth outright.
        let budget = RepairBudget {
            row_bytes: 2 * 4096,
            max_slice_size: MAX_SLICE_SIZE,
        };
        let mut fold = RepairFold::new(&set, &[], release.recovery_blocks(), budget).unwrap();
        for gi in 0..damaged.len() {
            fold.mark_damaged(gi);
        }
        assert!(matches!(
            fold.grow_to_damage(&mut read),
            Err(FoldError::Budget { .. })
        ));
        assert_eq!(fold.rows(), 1, "a refused growth changes nothing");
    }

    #[test]
    fn rows_reach_exponents_the_volumes_use() {
        let dir = tempfile::tempdir().unwrap();
        let fixture = Par2Fixture::new()
            .with_slice_size(4096)
            .add_file("row.bin", &seeded_bytes(4, 4096 * 6));
        let set = set_from(&fixture, dir.path());
        let data: Vec<Vec<u8>> = seeded_bytes(4, 4096 * 6)
            .chunks(4096)
            .map(|c| c.to_vec())
            .collect();
        let mut fold = RepairFold::new(&set, &[2, 4], 4, RepairBudget::default()).unwrap();
        for (gi, bytes) in data.iter().enumerate() {
            if gi == 2 || gi == 4 {
                continue;
            }
            fold.feed_verified(gi, bytes).unwrap();
        }
        // The volume carries exponents 2 and 3, not 0 and 1.
        let volume = fixture.volume_bytes(&[2, 3]);
        assert_eq!(fold.feed_volume(&volume).unwrap(), 2);
        // The rows are exponents 0 and 1 and neither volume carries them, so
        // the solve reports no usable row rather than guessing.
        match fold.solve() {
            Err(FoldError::Rows(RowFailure::NotEnoughRows { needed, usable })) => {
                assert_eq!((needed, usable), (2, 0));
            }
            other => panic!("expected NotEnoughRows, got {other:?}"),
        }
        let exps = fold.missing_row_exponents();
        assert_eq!(exps, vec![2, 3]);
        let mut read = |gi: usize| Ok(data[gi].clone());
        fold.extend_rows(&exps, &mut read).unwrap();
        // Both rows that carry data are now present.
        let repaired: HashMap<usize, Vec<u8>> = fold.solve().unwrap().into_iter().collect();
        assert_eq!(repaired[&2], data[2]);
        assert_eq!(repaired[&4], data[4]);
    }

    #[test]
    fn volumes_from_another_set_are_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let (release, damaged) = missing_everywhere();
        let set = set_of(&release, dir.path());
        let mut fold = RepairFold::new(
            &set,
            &damaged.iter().copied().collect::<Vec<_>>(),
            release.recovery_blocks(),
            RepairBudget::default(),
        )
        .unwrap();
        let other = Par2Fixture::new()
            .with_recovery_set_id(*b"not-this-set-123")
            .add_file("other.bin", &seeded_bytes(5, 8192));
        assert_eq!(fold.feed_volume(&other.volume_bytes(&[0, 1])).unwrap(), 0);
        assert_eq!(fold.recovery_count(), 0);
    }

    /// A fold for the standard release with `damaged` known and `data` fed.
    fn prepared(
        release: &DamageRelease,
        damaged: &BTreeSet<usize>,
        dir: &Path,
    ) -> (RepairFold, Vec<Vec<u8>>) {
        let set = set_of(release, dir);
        let data = slices(release);
        let mut fold = RepairFold::new(
            &set,
            &damaged.iter().copied().collect::<Vec<_>>(),
            release.recovery_blocks(),
            RepairBudget::default(),
        )
        .unwrap();
        for (gi, bytes) in data.iter().enumerate() {
            if !damaged.contains(&gi) {
                fold.feed_verified(gi, bytes).unwrap();
            }
        }
        (fold, data)
    }

    /// The harness a review blocker needs: a volume whose first packet claims
    /// a length that would overflow when added to its offset.
    #[test]
    fn a_huge_packet_length_does_not_panic() {
        let dir = tempfile::tempdir().unwrap();
        let (release, damaged) = missing_everywhere();
        let (mut fold, _) = prepared(&release, &damaged, dir.path());

        let mut volume = release.volumes[0].file.payload.clone();
        // Huge declared length: `pos + len` would wrap on a 32-bit usize and
        // makes no sense on 64-bit either.
        volume[8..16].copy_from_slice(&u64::MAX.to_le_bytes());
        assert_eq!(fold.feed_volume(&volume).unwrap(), 0);
        assert_eq!(fold.recovery_count(), 0, "a bogus packet is dropped");

        // A length past the end of the buffer, and one that is not 4-aligned.
        for bad in [volume.len() as u64 + 4, 65] {
            let mut volume = release.volumes[0].file.payload.clone();
            volume[8..16].copy_from_slice(&bad.to_le_bytes());
            assert_eq!(fold.feed_volume(&volume).unwrap(), 0);
        }

        // A volume truncated inside its first packet's body.
        let short = &release.volumes[0].file.payload[..PACKET_HEADER + 100];
        assert_eq!(fold.feed_volume(short).unwrap(), 0);

        // Every truncation of a real volume is safe to feed.
        let volume = &release.volumes[1].file.payload;
        for cut in (0..volume.len()).step_by(97) {
            fold.feed_volume(&volume[..cut]).unwrap();
        }
    }

    /// A recovery slice whose packet MD5 does not match is not kept: a
    /// damaged recovery article must not reach the solve.
    #[test]
    fn a_recovery_packet_with_a_bad_md5_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let (release, damaged) = missing_everywhere();
        let (mut fold, _data) = prepared(&release, &damaged, dir.path());

        let volume = release.volumes[0].file.payload.clone();
        // Flip a data byte of the first RecvSlic packet without touching its
        // stored MD5.
        let first = volume
            .windows(16)
            .position(|w| w == TYPE_RECOVERY)
            .expect("the volume carries a RecvSlic packet")
            + 16;
        let mut corrupt = volume.clone();
        corrupt[first + 8] ^= 0xFF;
        assert_eq!(fold.feed_volume(&corrupt).unwrap(), 0);
        assert_eq!(fold.recovery_count(), 0);

        // The intact copy is kept.
        assert!(!volume.is_empty());
        assert_eq!(fold.feed_volume(&volume).unwrap(), 1);
    }

    /// Solved bytes are checked against the IFSC digest, so a wrong
    /// recovery slice fails the solve instead of being written.
    #[test]
    fn a_solved_slice_that_does_not_match_its_digest_fails_the_solve() {
        let dir = tempfile::tempdir().unwrap();
        let (release, damaged) = missing_everywhere();
        let (mut fold, data) = prepared(&release, &damaged, dir.path());
        for volume in &release.volumes {
            fold.feed_volume(&volume.file.payload).unwrap();
        }
        // Recovery data that is the right length but wrong: the rows are
        // intact, so the solve produces bytes the set does not agree with.
        let exponent = *fold.exponents().first().unwrap();
        fold.feed_recovery(exponent, &vec![0xA5; fold.slice_size() as usize])
            .unwrap();
        match fold.solve() {
            Err(FoldError::CorruptOutput(_)) => {}
            other => panic!("expected CorruptOutput, got {other:?}"),
        }

        // Put the real slice back: the same fold solves cleanly.
        let v = release
            .volumes
            .iter()
            .find(|v| v.exponents.contains(&exponent))
            .unwrap();
        fold.feed_volume(&v.file.payload).unwrap();
        let repaired: HashMap<usize, Vec<u8>> = fold.solve().unwrap().into_iter().collect();
        for gi in &damaged {
            assert_eq!(repaired[gi], data[*gi], "slice {gi}");
        }
    }

    /// Retention is capped by the rows the budget can ever open, so feeding
    /// far more volumes than a solve can use does not grow without bound.
    #[test]
    fn recovery_retention_is_bounded_by_the_row_budget() {
        let dir = tempfile::tempdir().unwrap();
        let (release, damaged) = missing_everywhere();
        let (mut fold, _) = prepared(&release, &damaged, dir.path());
        let cap = fold.max_recovery_slices();
        assert_eq!(cap, release.recovery_blocks() as usize);

        // A volume with more recovery slices than the fold could ever use.
        let extra: Vec<u32> = (0..(cap as u32 + 64)).collect();
        let volume = release.par2.volume_bytes(&extra);
        let added = fold.feed_volume(&volume).unwrap();
        assert_eq!(added, cap, "only the usable exponents are retained");
        assert_eq!(fold.recovery_count(), cap);
        assert_eq!(fold.recovery_count(), fold.max_recovery_slices());
        assert!(
            fold.recovery_bytes() <= MAX_ROW_BYTES,
            "retained recovery ({} B) stays within the row budget",
            fold.recovery_bytes()
        );
    }

    /// A fold sized exactly at the budget replaces dead rows instead of
    /// adding, so the solve still fits.
    #[test]
    fn solve_with_replaces_rows_without_recovery_data() {
        let dir = tempfile::tempdir().unwrap();
        let fixture = Par2Fixture::new()
            .with_slice_size(4096)
            .add_file("swap.bin", &seeded_bytes(6, 4096 * 8));
        let set = set_from(&fixture, dir.path());
        let data: Vec<Vec<u8>> = seeded_bytes(6, 4096 * 8)
            .chunks(4096)
            .map(|c| c.to_vec())
            .collect();
        let damaged = [1usize, 3, 5, 7];
        // Exactly four rows: no room to add a fifth.
        let budget = RepairBudget {
            row_bytes: 4 * 4096,
            max_slice_size: MAX_SLICE_SIZE,
        };
        let mut fold = RepairFold::new(&set, &damaged, 4, budget).unwrap();
        assert_eq!(fold.rows(), 4);
        for (gi, bytes) in data.iter().enumerate() {
            if !damaged.contains(&gi) {
                fold.feed_verified(gi, bytes).unwrap();
            }
        }
        // The volume starts at exponent 4, so all four rows are dead.
        assert_eq!(
            fold.feed_volume(&fixture.volume_bytes(&[4, 5, 6, 7]))
                .unwrap(),
            4
        );
        let repaired: HashMap<usize, Vec<u8>> = fold
            .solve_with(|gi| Ok(data[gi].clone()))
            .unwrap()
            .into_iter()
            .collect();
        assert_eq!(fold.rows(), 4, "rows were replaced, not added");
        for gi in damaged {
            assert_eq!(repaired[&gi], data[gi], "slice {gi}");
        }
    }

    #[test]
    fn fixed_exponents_report_why_they_cannot_grow() {
        let dir = tempfile::tempdir().unwrap();
        let fixture = Par2Fixture::new()
            .with_slice_size(4096)
            .add_file("fixed.bin", &seeded_bytes(7, 4096 * 4));
        let set = set_from(&fixture, dir.path());
        let mut fold =
            RepairFold::with_exponents(&set, &[2, 3], &[1], RepairBudget::default()).unwrap();
        let mut read = |_gi: usize| Ok(Vec::new());
        assert!(matches!(
            fold.extend_rows(&[4], &mut read),
            Err(FoldError::FixedExponents)
        ));
    }
}
