//! Synthesise real, parseable PAR2 recovery sets for tests.
//!
//! The repo has no `.par2` fixtures and no `par2create` binary in CI, so this
//! builder writes spec-correct PAR2 packets from in-memory file contents.
//! `rust_par2::parse` accepts the index, `rust_par2::verify` checks files
//! against it, and `rust_par2::repair` rebuilds damaged slices from the
//! recovery volumes, all on the real code paths rather than a mock.
//!
//! Packets emitted:
//!
//! * **Main**: the slice size and the file IDs, sorted by ID as the spec
//!   requires. Global slice numbering follows this order.
//! * **FileDesc**: one per file. It carries the file ID, the full-file MD5, the
//!   MD5 of the first 16 KiB, the size and the *expected* filename. The 16K hash
//!   is what PAR2-guided deobfuscation matches obfuscated files against.
//! * **IFSC**: one per file, with the per-slice MD5 and CRC32. The final
//!   partial slice is zero-padded to the slice size, exactly as PAR2 (and
//!   `rust_par2`'s verifier) compute them.
//! * **RecvSlic**: only in volumes ([`Par2Fixture::volume_bytes`]). Each
//!   recovery slice for exponent `e` is `Σ c[i]^e · input[i]` over GF(2^16),
//!   built with `rust_par2`'s own encoding matrix.
//!
//! Packet layout (little-endian), per the PAR 2.0 spec:
//!
//! ```text
//!   0..8    magic "PAR2\0PKT"
//!   8..16   packet length (u64, whole packet, multiple of 4)
//!  16..32   MD5 of everything from offset 32 onward
//!  32..48   recovery set ID
//!  48..64   packet type
//!  64..     body
//! ```

use std::path::Path;

use md5::{Digest, Md5};
use rust_par2::gf_simd_public::mul_add_multi;
use rust_par2::matrix::GfMatrix;

const MAGIC: &[u8; 8] = b"PAR2\x00PKT";
const TYPE_MAIN: &[u8; 16] = b"PAR 2.0\x00Main\x00\x00\x00\x00";
const TYPE_FILE_DESC: &[u8; 16] = b"PAR 2.0\x00FileDesc";
const TYPE_IFSC: &[u8; 16] = b"PAR 2.0\x00IFSC\x00\x00\x00\x00";
const TYPE_RECOVERY: &[u8; 16] = b"PAR 2.0\x00RecvSlic";

/// One file recorded in the recovery set.
#[derive(Clone, Debug)]
struct FileEntry {
    /// The name PAR2 considers canonical, which a rename should restore.
    expected_name: String,
    /// File ID (spec: MD5 of hash_16k + size + name).
    file_id: [u8; 16],
    hash: [u8; 16],
    hash_16k: [u8; 16],
    size: u64,
    /// Per-slice (MD5, CRC32) checksums, in slice order.
    slices: Vec<([u8; 16], u32)>,
    /// The file's slices, each zero-padded to the slice size. Kept so
    /// recovery slices can be computed without the caller re-supplying data.
    padded: Vec<Vec<u8>>,
}

/// Builds a PAR2 recovery set describing files by content.
#[derive(Clone, Debug)]
pub struct Par2Fixture {
    slice_size: u64,
    recovery_set_id: [u8; 16],
    files: Vec<FileEntry>,
}

impl Default for Par2Fixture {
    fn default() -> Self {
        Self::new()
    }
}

impl Par2Fixture {
    /// Start a new recovery set with a 4096-byte slice size.
    pub fn new() -> Self {
        Self {
            slice_size: 4096,
            // Fixed, not random: tests must be deterministic.
            recovery_set_id: *b"rustnzbfixture01",
            files: Vec::new(),
        }
    }

    /// Change the slice size. Must be a non-zero multiple of 4 and set before
    /// any file is added.
    pub fn with_slice_size(mut self, slice_size: u64) -> Self {
        assert!(
            slice_size > 0 && slice_size % 4 == 0,
            "PAR2 slice size must be a non-zero multiple of 4, got {slice_size}"
        );
        assert!(self.files.is_empty(), "set the slice size before add_file");
        self.slice_size = slice_size;
        self
    }

    pub fn with_recovery_set_id(mut self, recovery_set_id: [u8; 16]) -> Self {
        self.recovery_set_id = recovery_set_id;
        self
    }

    /// Record `contents` under the canonical name PAR2 should report.
    ///
    /// This does not write the file to disk. The test decides whether to
    /// place it under its canonical name or an obfuscated one.
    pub fn add_file(mut self, expected_name: &str, contents: &[u8]) -> Self {
        let hash: [u8; 16] = Md5::digest(contents).into();
        // Mirrors `rust_par2::compute_hash_16k`: MD5 of the first 16 KiB.
        let head = &contents[..contents.len().min(16384)];
        let hash_16k: [u8; 16] = Md5::digest(head).into();
        let size = contents.len() as u64;

        // Spec: File ID = MD5(hash_16k || size_le || name).
        let mut id_input = Vec::new();
        id_input.extend_from_slice(&hash_16k);
        id_input.extend_from_slice(&size.to_le_bytes());
        id_input.extend_from_slice(expected_name.as_bytes());
        let file_id: [u8; 16] = Md5::digest(&id_input).into();

        // Each slice is hashed zero-padded to the slice size, matching PAR2
        // and rust_par2's verifier.
        let slice_size = self.slice_size as usize;
        let mut slices = Vec::new();
        let mut padded = Vec::new();
        for chunk in contents.chunks(slice_size) {
            let mut chunk = chunk.to_vec();
            chunk.resize(slice_size, 0);
            let slice_md5: [u8; 16] = Md5::digest(&chunk).into();
            slices.push((slice_md5, crc32fast::hash(&chunk)));
            padded.push(chunk);
        }

        self.files.push(FileEntry {
            expected_name: expected_name.to_string(),
            file_id,
            hash,
            hash_16k,
            size,
            slices,
            padded,
        });
        self
    }

    pub fn slice_size(&self) -> u64 {
        self.slice_size
    }

    /// Total input slices across every file.
    pub fn total_slices(&self) -> u32 {
        self.files.iter().map(|f| f.slices.len() as u32).sum()
    }

    /// Canonical file names in PAR2 global order (sorted by file ID).
    pub fn file_order(&self) -> Vec<String> {
        self.ordered()
            .iter()
            .map(|f| f.expected_name.clone())
            .collect()
    }

    /// The global index of a file's first slice, or `None` for an unknown
    /// name. Global index = slices of every earlier file in
    /// [`file_order`](Self::file_order) plus the slice index within the file.
    pub fn first_global_slice(&self, expected_name: &str) -> Option<u32> {
        let mut base = 0u32;
        for file in self.ordered() {
            if file.expected_name == expected_name {
                return Some(base);
            }
            base += file.slices.len() as u32;
        }
        None
    }

    /// The bytes of the index `.par2` file: Main, FileDesc and IFSC packets.
    pub fn index_bytes(&self) -> Vec<u8> {
        let ordered = self.ordered();
        let mut out = Vec::new();

        // Main packet: slice size, file count, then the file IDs in order.
        let mut main_body = Vec::new();
        main_body.extend_from_slice(&self.slice_size.to_le_bytes());
        main_body.extend_from_slice(&(ordered.len() as u32).to_le_bytes());
        for file in &ordered {
            main_body.extend_from_slice(&file.file_id);
        }
        out.extend_from_slice(&self.packet(TYPE_MAIN, &main_body));

        for file in &ordered {
            let mut body = Vec::new();
            body.extend_from_slice(&file.file_id);
            body.extend_from_slice(&file.hash);
            body.extend_from_slice(&file.hash_16k);
            body.extend_from_slice(&file.size.to_le_bytes());
            body.extend_from_slice(file.expected_name.as_bytes());
            // Filename is null-padded to a multiple of 4 so the packet length
            // stays 4-aligned; the parser rejects lengths that are not.
            while body.len() % 4 != 0 {
                body.push(0);
            }
            out.extend_from_slice(&self.packet(TYPE_FILE_DESC, &body));
        }

        // IFSC: file ID then (MD5[16] + CRC32[4]) per slice. 16 + 20*n is
        // always 4-aligned, so the packet length stays valid.
        for file in &ordered {
            let mut body = Vec::new();
            body.extend_from_slice(&file.file_id);
            for (slice_md5, crc32) in &file.slices {
                body.extend_from_slice(slice_md5);
                body.extend_from_slice(&crc32.to_le_bytes());
            }
            out.extend_from_slice(&self.packet(TYPE_IFSC, &body));
        }

        out
    }

    /// The bytes of a recovery volume carrying one RecvSlic packet per
    /// exponent, preceded by the index packets as real volumes are.
    pub fn volume_bytes(&self, exponents: &[u32]) -> Vec<u8> {
        let inputs: Vec<&[u8]> = self
            .ordered()
            .into_iter()
            .flat_map(|f| f.padded.iter().map(Vec::as_slice))
            .collect();
        let input_count = inputs.len();
        let matrix = GfMatrix::par2_encoding_matrix(input_count, exponents);

        let mut out = self.index_bytes();
        for (row, &exponent) in exponents.iter().enumerate() {
            let coeffs: Vec<u16> = (0..input_count)
                .map(|col| matrix.get(input_count + row, col))
                .collect();
            let mut recovery = vec![0u8; self.slice_size as usize];
            mul_add_multi(&mut recovery, &inputs, &coeffs);

            let mut body = Vec::with_capacity(4 + recovery.len());
            body.extend_from_slice(&exponent.to_le_bytes());
            body.extend_from_slice(&recovery);
            out.extend_from_slice(&self.packet(TYPE_RECOVERY, &body));
        }
        out
    }

    /// Write the index `.par2` file to `path`.
    pub fn write_index(&self, path: &Path) {
        std::fs::write(path, self.index_bytes()).unwrap();
    }

    /// Write a recovery volume for `exponents` to `path`.
    pub fn write_volume(&self, path: &Path, exponents: &[u32]) {
        std::fs::write(path, self.volume_bytes(exponents)).unwrap();
    }

    fn ordered(&self) -> Vec<&FileEntry> {
        let mut files: Vec<&FileEntry> = self.files.iter().collect();
        files.sort_by_key(|f| f.file_id);
        files
    }

    /// Frame `body` as a PAR2 packet with a correct length and MD5.
    ///
    /// The parser recomputes the MD5 over everything from offset 32 and skips
    /// packets that don't match, so this must be exact. A wrong hash makes
    /// the fixture silently empty rather than failing loudly.
    fn packet(&self, packet_type: &[u8; 16], body: &[u8]) -> Vec<u8> {
        // `data` is what the MD5 covers: set ID + type + body.
        let mut data = Vec::with_capacity(32 + body.len());
        data.extend_from_slice(&self.recovery_set_id);
        data.extend_from_slice(packet_type);
        data.extend_from_slice(body);

        let packet_len = (32 + data.len()) as u64;
        assert!(
            packet_len % 4 == 0,
            "PAR2 packet length must be 4-aligned, got {packet_len}"
        );
        let md5: [u8; 16] = Md5::digest(&data).into();

        let mut packet = Vec::with_capacity(packet_len as usize);
        packet.extend_from_slice(MAGIC);
        packet.extend_from_slice(&packet_len.to_le_bytes());
        packet.extend_from_slice(&md5);
        packet.extend_from_slice(&data);
        packet
    }
}
