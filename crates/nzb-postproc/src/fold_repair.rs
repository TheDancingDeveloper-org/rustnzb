//! Repair a PAR2 set from the download engine's in-flight repair fold
//! (WI-148).
//!
//! The fold already holds the intact slices' contributions to its recovery
//! rows. What is left here is: fold any intact slice it missed (read from
//! disk once), XOR in the recovery data, solve the D×D system and write the D
//! repaired slices. Any shortfall (the fold does not match the set, too few
//! rows with recovery data and no budget to add them, a poisoned row) returns
//! an error and the caller uses `rust_par2::repair_from_verify` instead. Both
//! paths produce the same bytes: the fold uses `rust-par2`'s arithmetic.

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use nzb_decode::{FoldError, SharedFold};
use rust_par2::{Par2File, Par2FileSet, VerifyResult};
use tracing::{debug, info};

/// What a fold repair did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FoldRepair {
    pub blocks_repaired: u32,
    pub files_repaired: usize,
    /// Rows the fold held when it solved.
    pub rows: usize,
    /// Intact slices fed during the download (network path + prefix).
    pub slices_prefed: usize,
    /// Intact slices the fold missed and that were read from disk here.
    pub slices_read_at_solve: usize,
}

/// Why the fold could not repair the set.
#[derive(Debug, thiserror::Error)]
pub enum FoldRepairError {
    #[error("the fold does not describe this recovery set")]
    Mismatch,
    #[error(transparent)]
    Fold(#[from] FoldError),
    #[error("I/O error writing repaired slices: {0}")]
    Io(#[from] std::io::Error),
}

struct Placed<'a> {
    file: &'a Par2File,
    start: usize,
    count: usize,
}

fn layout(set: &Par2FileSet) -> Vec<Placed<'_>> {
    let mut start = 0;
    set.file_order
        .iter()
        .filter_map(|id| set.files.get(id))
        .map(|file| {
            let count = if set.slice_size == 0 {
                0
            } else {
                file.size.div_ceil(set.slice_size) as usize
            };
            let placed = Placed { file, start, count };
            start += count;
            placed
        })
        .collect()
}

/// Global indices of every damaged or missing slice, as rust-par2 maps them.
fn damaged_indices(placed: &[Placed<'_>], verify: &VerifyResult) -> Vec<usize> {
    let find = |name: &str| placed.iter().find(|p| p.file.filename == name);
    let mut out = Vec::new();
    for d in &verify.damaged {
        if let Some(p) = find(&d.filename) {
            if d.damaged_block_indices.is_empty() {
                out.extend(p.start..p.start + p.count);
            } else {
                out.extend(
                    d.damaged_block_indices
                        .iter()
                        .map(|i| p.start + *i as usize),
                );
            }
        }
    }
    for m in &verify.missing {
        if let Some(p) = find(&m.filename) {
            out.extend(p.start..p.start + p.count);
        }
    }
    out.sort_unstable();
    out.dedup();
    out
}

/// (file, byte offset, byte length) of a global slice.
fn locate<'a>(placed: &'a [Placed<'a>], gi: usize, slice: u64) -> Option<(&'a Par2File, u64, u64)> {
    let p = placed
        .iter()
        .find(|p| gi >= p.start && gi < p.start + p.count)?;
    let offset = (gi - p.start) as u64 * slice;
    Some((p.file, offset, (p.file.size - offset).min(slice)))
}

/// Repair `set` in `dir` from `fold`, given the set's verify result.
pub fn repair_from_fold(
    set: &Par2FileSet,
    dir: &Path,
    verify: &VerifyResult,
    fold: &SharedFold,
) -> Result<FoldRepair, FoldRepairError> {
    let placed = layout(set);
    let total: usize = placed.iter().map(|p| p.count).sum();
    let mut fold = fold.lock();
    if fold.total_slices() != total || fold.slice_size() != set.slice_size {
        return Err(FoldRepairError::Mismatch);
    }
    let damaged = damaged_indices(&placed, verify);
    // Rows still being backfilled by the download do not count; drop them
    // so the solve can add rows of its own.
    fold.abandon_growth();
    let slices_prefed = fold.fed_count();
    let slice = set.slice_size;

    let mut handles: HashMap<String, std::fs::File> = HashMap::new();
    let mut read = |gi: usize| -> std::io::Result<Vec<u8>> {
        let (file, offset, len) = locate(&placed, gi, slice)
            .ok_or_else(|| std::io::Error::other("slice outside the set"))?;
        let handle = match handles.entry(file.filename.clone()) {
            std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
            std::collections::hash_map::Entry::Vacant(e) => {
                e.insert(std::fs::File::open(dir.join(&file.filename))?)
            }
        };
        handle.seek(SeekFrom::Start(offset))?;
        let mut buf = vec![0u8; len as usize];
        handle.read_exact(&mut buf)?;
        Ok(buf)
    };
    let slices_read_at_solve = fold.feed_missing(&damaged, &mut read)?;

    let recovery: HashMap<u32, Vec<u8>> =
        rust_par2::recovery::load_recovery_blocks(dir, &set.recovery_set_id, set.slice_size)
            .into_iter()
            .map(|b| (b.exponent, b.data))
            .collect();
    let repaired = match fold.solve(&damaged, &recovery) {
        Err(FoldError::NotEnoughRows { needed, usable }) if usable < needed => {
            // The rows the download opened miss recovery data on disk (or are
            // too few). Add rows for exponents that are on disk, re-reading
            // the intact slices for the new rows only.
            let mut present: Vec<u32> = recovery
                .keys()
                .copied()
                .filter(|e| !fold.exponents().contains(e))
                .collect();
            present.sort_unstable();
            present.truncate(needed - usable);
            fold.extend_rows(&present, &mut read)?;
            fold.solve(&damaged, &recovery)?
        }
        other => other?,
    };

    let mut touched = std::collections::HashSet::new();
    for (gi, bytes) in &repaired {
        let (file, offset, len) = locate(&placed, *gi, slice).ok_or(FoldError::OutOfRange(*gi))?;
        let path = dir.join(&file.filename);
        debug!(
            file = file.filename,
            slice = gi,
            offset,
            len,
            "Writing slice repaired from the fold"
        );
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)?;
        if f.metadata()?.len() < file.size {
            f.set_len(file.size)?;
        }
        f.seek(SeekFrom::Start(offset))?;
        f.write_all(&bytes[..len as usize])?;
        touched.insert(file.filename.clone());
    }
    let result = FoldRepair {
        blocks_repaired: repaired.len() as u32,
        files_repaired: touched.len(),
        rows: fold.rows(),
        slices_prefed,
        slices_read_at_solve,
    };
    info!(
        blocks = result.blocks_repaired,
        files = result.files_repaired,
        rows = result.rows,
        prefed = result.slices_prefed,
        read_at_solve = result.slices_read_at_solve,
        "PAR2 set repaired from the in-flight fold"
    );
    Ok(result)
}
