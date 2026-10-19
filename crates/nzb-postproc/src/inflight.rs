//! Build a PAR2 [`VerifyResult`] from the download engine's in-flight slice
//! map instead of re-reading every data file (WI-147).
//!
//! The download engine hashes each PAR2 slice as its articles land and hands
//! post-processing a [`SliceMapSummary`]: one verdict per global slice, in
//! Main-packet file order. A file whose slices are all `Verified` or `Damaged`
//! and which is still on disk at its PAR2 size needs no read at all: the map
//! already says which of its slices are bad. A file with any `Unknown` slice
//! (the index arrived late, the job was resumed, an obfuscated file was never
//! matched) or whose on-disk size disagrees is verified from disk as before,
//! and the two halves are merged.
//!
//! Recovery volumes are still counted from the directory; that reads PAR2
//! volume headers, never data files.

use std::path::Path;

use nzb_core::models::{SliceMapSummary, SliceVerdict};
use rust_par2::{DamagedFile, Par2File, Par2FileSet, VerifiedFile, VerifyResult};
use tracing::{debug, info};

/// Where a recovery set's verify result came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerifySource {
    /// Every file was decided by the slice map; no data file was read.
    InFlight,
    /// Some files came from the slice map, the rest were read from disk.
    Partial,
    /// Every file was read from disk (no usable map, or the escape hatch).
    Disk,
}

impl VerifySource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::InFlight => "in-flight",
            Self::Partial => "partial",
            Self::Disk => "disk",
        }
    }
}

/// A verify result plus how it was obtained.
#[derive(Debug)]
pub struct SetVerify {
    pub result: VerifyResult,
    pub source: VerifySource,
    /// Data-file bytes the slice map let verification skip reading.
    pub bytes_skipped: u64,
    /// Data-file bytes handed to the disk verifier.
    pub bytes_from_disk: u64,
}

fn slice_count(file: &Par2File, slice_size: u64) -> usize {
    if slice_size == 0 {
        0
    } else {
        file.size.div_ceil(slice_size) as usize
    }
}

/// Files in Main-packet order, as `rust-par2` and the slice layout walk them.
fn ordered_files(set: &Par2FileSet) -> Vec<&Par2File> {
    set.file_order
        .iter()
        .filter_map(|id| set.files.get(id))
        .collect()
}

/// Whether `map` describes `set`: its slice count must match the set's.
pub fn map_fits(set: &Par2FileSet, map: &SliceMapSummary) -> bool {
    let total: usize = ordered_files(set)
        .iter()
        .map(|f| slice_count(f, set.slice_size))
        .sum();
    total > 0 && total == map.states.len()
}

/// A copy of `set` restricted to `keep` (file ids), for a partial disk pass.
fn subset(set: &Par2FileSet, keep: &[rust_par2::types::Id16]) -> Par2FileSet {
    Par2FileSet {
        recovery_set_id: set.recovery_set_id,
        slice_size: set.slice_size,
        file_order: keep.to_vec(),
        files: keep
            .iter()
            .filter_map(|id| set.files.get(id).map(|f| (*id, f.clone())))
            .collect(),
        recovery_block_count: set.recovery_block_count,
        creator: set.creator.clone(),
    }
}

/// Verify `set` in `dir`, trusting `map` for every file it fully decides.
///
/// `map` must already be known to describe `set` (see [`map_fits`]); pass
/// `None` to force the full disk pass.
pub fn verify_set(set: &Par2FileSet, dir: &Path, map: Option<&SliceMapSummary>) -> SetVerify {
    let Some(map) = map.filter(|m| map_fits(set, m)) else {
        let total = ordered_files(set).iter().map(|f| f.size).sum();
        return SetVerify {
            result: rust_par2::verify(set, dir),
            source: VerifySource::Disk,
            bytes_skipped: 0,
            bytes_from_disk: total,
        };
    };

    let mut intact = Vec::new();
    let mut damaged = Vec::new();
    let mut from_disk = Vec::new();
    let mut bytes_skipped = 0u64;
    let mut bytes_from_disk = 0u64;
    let mut start = 0usize;
    for file in ordered_files(set) {
        let count = slice_count(file, set.slice_size);
        let states = &map.states[start..start + count];
        start += count;
        let on_disk = std::fs::metadata(dir.join(&file.filename))
            .ok()
            .filter(|m| m.is_file())
            .map(|m| m.len());
        let decided = !states.contains(&SliceVerdict::Unknown);
        if !decided || on_disk != Some(file.size) {
            debug!(
                filename = file.filename,
                decided, "Slice map cannot decide file; verifying from disk"
            );
            from_disk.push(file.file_id);
            bytes_from_disk += file.size;
            continue;
        }
        bytes_skipped += file.size;
        let bad: Vec<u32> = states
            .iter()
            .enumerate()
            .filter(|(_, s)| **s == SliceVerdict::Damaged)
            .map(|(i, _)| i as u32)
            .collect();
        if bad.is_empty() {
            intact.push(VerifiedFile {
                filename: file.filename.clone(),
                size: file.size,
            });
        } else {
            damaged.push(DamagedFile {
                filename: file.filename.clone(),
                size: file.size,
                damaged_block_count: bad.len() as u32,
                total_block_count: count as u32,
                damaged_block_indices: bad,
            });
        }
    }

    // The disk pass also counts the recovery blocks in `dir`. With nothing
    // left to read it runs on an empty subset, which touches no data file.
    let disk = rust_par2::verify(&subset(set, &from_disk), dir);
    intact.extend(disk.intact);
    damaged.extend(disk.damaged);
    let missing = disk.missing;

    let source = if from_disk.is_empty() {
        VerifySource::InFlight
    } else {
        VerifySource::Partial
    };
    let mut result = VerifyResult {
        intact,
        damaged,
        missing,
        recovery_blocks_available: disk.recovery_blocks_available,
        repair_possible: false,
    };
    result.repair_possible = result.blocks_needed() <= result.recovery_blocks_available;
    info!(
        source = source.as_str(),
        files_from_map = set.files.len() - from_disk.len(),
        files_from_disk = from_disk.len(),
        bytes_skipped,
        "PAR2 verify built from the in-flight slice map"
    );
    SetVerify {
        result,
        source,
        bytes_skipped,
        bytes_from_disk,
    }
}
