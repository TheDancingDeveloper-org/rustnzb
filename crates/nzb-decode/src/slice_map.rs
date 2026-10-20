//! In-flight PAR2 slice verification (WI-144 core).
//!
//! As decoded articles land in the assembler, the bytes that make up each PAR2
//! recovery slice arrive too — out of order, at arbitrary offsets, from many
//! workers. This module turns those writes into a per-slice verdict
//! (`Verified` / `Damaged` / `Unknown`) so the post-download PAR2 verify pass
//! can be skipped for a clean job (WI-147) and damage can be compared to
//! recovery blocks during the download (WI-145).
//!
//! MD5 is not associative, so a slice cannot be hashed incrementally from
//! out-of-order fragments. Instead each slice tracks how many of its bytes
//! have been written; once a slice's whole byte range is filled the caller
//! reads it back (from the assembler's file, page-cache-hot) and calls
//! [`SliceVerifier::verify`], which pads the final partial slice with zeros —
//! matching how PAR2 IFSC checksums are computed — and compares MD5 and CRC32.
//!
//! The verifier holds only PAR2 geometry as plain data ([`SliceLayout`]), so
//! this crate needs no PAR2 parser: the download engine builds the layout from
//! a parsed `rust-par2` file set and the NZB-file-to-PAR2-file mapping.

use crc32fast::Hasher as Crc32;
use md5::{Digest, Md5};

/// A 16-byte MD5 digest.
pub type Md5Hash = [u8; 16];

/// The verdict for one slice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SliceState {
    /// Not yet fully written, or written but not yet checked.
    Unknown,
    /// Fully written and its MD5 and CRC32 match the PAR2 checksums.
    Verified,
    /// Fully written but a checksum mismatched, or a byte is known missing.
    Damaged,
}

/// The expected checksums for one slice, from a PAR2 IFSC packet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SliceDigest {
    pub md5: Md5Hash,
    pub crc32: u32,
}

/// One file's slice checksums, in slice order, with the file's total size.
#[derive(Debug, Clone)]
pub struct FileSlices {
    pub size: u64,
    pub slices: Vec<SliceDigest>,
}

/// PAR2 geometry the verifier needs, in the recovery set's file order. Built by
/// the caller from a parsed `rust-par2` `Par2FileSet` (its `slice_size`,
/// `file_order`, and each file's IFSC `slices`).
#[derive(Debug, Clone)]
pub struct SliceLayout {
    pub slice_size: u64,
    pub files: Vec<FileSlices>,
}

#[derive(Debug)]
struct SliceCell {
    file_index: usize,
    /// Byte offset of this slice within its file.
    start: u64,
    /// Actual byte length (the last slice of a file may be shorter).
    len: u64,
    /// Bytes written into this slice's range so far.
    filled: u64,
    expected: SliceDigest,
    state: SliceState,
}

/// Tracks slice fill as writes arrive and verifies each slice once complete.
#[derive(Debug)]
pub struct SliceVerifier {
    slice_size: u64,
    cells: Vec<SliceCell>,
    /// Global index of the first slice of each file, plus a trailing sentinel
    /// equal to `cells.len()` so `[file_index]..[file_index + 1]` is the file's
    /// slice range for any valid `file_index`.
    file_bounds: Vec<usize>,
}

fn overlap(a0: u64, a1: u64, b0: u64, b1: u64) -> u64 {
    a1.min(b1).saturating_sub(a0.max(b0))
}

impl SliceVerifier {
    /// Build a verifier for a PAR2 set. Every slice starts `Unknown`.
    pub fn new(layout: &SliceLayout) -> Self {
        let mut cells = Vec::new();
        let mut file_bounds = Vec::with_capacity(layout.files.len() + 1);
        for (file_index, file) in layout.files.iter().enumerate() {
            file_bounds.push(cells.len());
            for (i, digest) in file.slices.iter().enumerate() {
                let start = i as u64 * layout.slice_size;
                let len = file.size.saturating_sub(start).min(layout.slice_size);
                cells.push(SliceCell {
                    file_index,
                    start,
                    len,
                    filled: 0,
                    expected: *digest,
                    state: SliceState::Unknown,
                });
            }
        }
        file_bounds.push(cells.len());
        Self {
            slice_size: layout.slice_size,
            cells,
            file_bounds,
        }
    }

    /// Record that `len` bytes were written to `file_index` at `offset`.
    /// Returns the global indices of any slices that became fully filled with
    /// this write and are ready to [`verify`](Self::verify).
    pub fn mark_written(&mut self, file_index: usize, offset: u64, len: u64) -> Vec<usize> {
        let mut completed = Vec::new();
        let Some(&start_g) = self.file_bounds.get(file_index) else {
            return completed;
        };
        let end_g = self.file_bounds[file_index + 1];
        let (w0, w1) = (offset, offset + len);
        for gi in start_g..end_g {
            let cell = &mut self.cells[gi];
            if cell.state != SliceState::Unknown {
                continue;
            }
            let ov = overlap(cell.start, cell.start + cell.len, w0, w1);
            if ov == 0 {
                continue;
            }
            cell.filled = (cell.filled + ov).min(cell.len);
            if cell.filled == cell.len && cell.len > 0 {
                completed.push(gi);
            }
        }
        completed
    }

    /// Which file and byte range a global slice covers, so the caller can read
    /// the slice's bytes back to verify it. Returns `(file_index, offset, len)`.
    pub fn slice_range(&self, global_index: usize) -> Option<(usize, u64, u64)> {
        self.cells
            .get(global_index)
            .map(|c| (c.file_index, c.start, c.len))
    }

    /// Verify a completed slice against its PAR2 checksums. `bytes` is the
    /// slice's actual bytes (`len` from [`slice_range`](Self::slice_range));
    /// they are zero-padded to the slice size before hashing, as PAR2 does.
    pub fn verify(&mut self, global_index: usize, bytes: &[u8]) -> SliceState {
        let slice_size = self.slice_size as usize;
        let Some(cell) = self.cells.get_mut(global_index) else {
            return SliceState::Unknown;
        };
        let mut padded = bytes.to_vec();
        padded.resize(slice_size, 0);

        let md5: Md5Hash = Md5::digest(&padded).into();
        let mut crc = Crc32::new();
        crc.update(&padded);
        let crc = crc.finalize();

        cell.state = if md5 == cell.expected.md5 && crc == cell.expected.crc32 {
            SliceState::Verified
        } else {
            SliceState::Damaged
        };
        cell.state
    }

    /// Mark a slice damaged without hashing — used when the ledger confirms an
    /// article overlapping the slice is missing.
    pub fn mark_damaged(&mut self, global_index: usize) {
        if let Some(cell) = self.cells.get_mut(global_index) {
            cell.state = SliceState::Damaged;
        }
    }

    /// The verdict of every slice, in global index order.
    pub fn states(&self) -> Vec<SliceState> {
        self.cells.iter().map(|c| c.state).collect()
    }

    /// The set's slice size.
    pub fn slice_size(&self) -> u64 {
        self.slice_size
    }

    /// Total number of slices in the set.
    pub fn total_slices(&self) -> usize {
        self.cells.len()
    }

    /// Number of slices currently marked `Damaged`.
    pub fn damaged_count(&self) -> usize {
        self.cells
            .iter()
            .filter(|c| c.state == SliceState::Damaged)
            .count()
    }

    /// True once no slice is still `Unknown` — the map is decided.
    pub fn is_complete(&self) -> bool {
        self.cells.iter().all(|c| c.state != SliceState::Unknown)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn digest_of(slice: &[u8], slice_size: usize) -> SliceDigest {
        let mut padded = slice.to_vec();
        padded.resize(slice_size, 0);
        let md5: Md5Hash = Md5::digest(&padded).into();
        let mut crc = Crc32::new();
        crc.update(&padded);
        SliceDigest {
            md5,
            crc32: crc.finalize(),
        }
    }

    /// Build a one-file layout plus the payload for `size` bytes at
    /// `slice_size`, with correct per-slice checksums.
    fn one_file(size: usize, slice_size: usize) -> (SliceLayout, Vec<u8>) {
        let payload: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
        let slices = payload
            .chunks(slice_size)
            .map(|c| digest_of(c, slice_size))
            .collect();
        let layout = SliceLayout {
            slice_size: slice_size as u64,
            files: vec![FileSlices {
                size: size as u64,
                slices,
            }],
        };
        (layout, payload)
    }

    #[test]
    fn in_order_writes_verify_all_slices() {
        let (layout, payload) = one_file(250, 100); // slices of 100, 100, 50
        let mut v = SliceVerifier::new(&layout);
        assert_eq!(v.total_slices(), 3);

        // Three articles, one per slice, in order.
        for (i, chunk) in payload.chunks(100).enumerate() {
            let completed = v.mark_written(0, (i * 100) as u64, chunk.len() as u64);
            assert_eq!(completed, vec![i]);
            let (_f, off, len) = v.slice_range(i).unwrap();
            assert_eq!(
                v.verify(i, &payload[off as usize..(off + len) as usize]),
                SliceState::Verified
            );
        }
        assert!(v.is_complete());
        assert_eq!(v.damaged_count(), 0);
        assert_eq!(v.states(), vec![SliceState::Verified; 3]);
    }

    #[test]
    fn article_spanning_two_slices_and_out_of_order() {
        let (layout, payload) = one_file(300, 100);
        let mut v = SliceVerifier::new(&layout);

        // A write covering [50, 250) straddles slices 0, 1 and 2 but completes
        // only the fully covered middle slice 1.
        assert_eq!(v.mark_written(0, 50, 200), vec![1]);
        // Fill the head of slice 0 and the tail of slice 2, out of order.
        assert_eq!(v.mark_written(0, 250, 50), vec![2]);
        assert_eq!(v.mark_written(0, 0, 50), vec![0]);

        for gi in 0..3 {
            let (_f, off, len) = v.slice_range(gi).unwrap();
            assert_eq!(
                v.verify(gi, &payload[off as usize..(off + len) as usize]),
                SliceState::Verified
            );
        }
        assert!(v.is_complete());
    }

    #[test]
    fn corrupt_bytes_mark_slice_damaged() {
        let (layout, payload) = one_file(200, 100);
        let mut v = SliceVerifier::new(&layout);

        v.mark_written(0, 0, 100);
        let mut corrupt = payload[0..100].to_vec();
        corrupt[0] ^= 0xff;
        assert_eq!(v.verify(0, &corrupt), SliceState::Damaged);
        assert_eq!(v.damaged_count(), 1);
        assert!(!v.is_complete()); // slice 1 still Unknown
    }

    #[test]
    fn partial_fill_does_not_complete() {
        let (layout, _payload) = one_file(200, 100);
        let mut v = SliceVerifier::new(&layout);
        // Only half of slice 0.
        assert!(v.mark_written(0, 0, 50).is_empty());
        assert_eq!(v.states()[0], SliceState::Unknown);
    }

    #[test]
    fn mark_damaged_sets_state_directly() {
        let (layout, _payload) = one_file(150, 100);
        let mut v = SliceVerifier::new(&layout);
        v.mark_damaged(1);
        assert_eq!(v.states()[1], SliceState::Damaged);
        assert_eq!(v.damaged_count(), 1);
    }
}
