//! Synthesise real, parseable PAR2 index files for integration tests.
//!
//! The repo has no `.par2` fixtures and no `par2create` binary in CI, which
//! previously made the PAR2-guided deobfuscation path (`rename_to_par2_names`)
//! untestable end-to-end. This builder writes spec-correct PAR2 packets so
//! `rust_par2::parse` accepts them and the pipeline exercises the real code
//! path rather than a mock.
//!
//! Only the packets the pipeline actually reads are emitted:
//!
//! * **Main** — carries the slice size; `parse` returns `NoMainPacket` without it.
//! * **FileDesc** — one per file: file ID, full-file MD5, first-16K MD5, size,
//!   and the *expected* filename. The 16K hash is what `rename_to_par2_names`
//!   matches obfuscated files against.
//!
//! * **IFSC** — one per file: the per-slice MD5 and CRC32 checksums, with the
//!   final partial slice zero-padded to the slice size, exactly as PAR2 (and
//!   `rust_par2`'s verifier) compute them. This is what lets `verify` report a
//!   correct file as intact and what the in-flight slice verifier (WI-144)
//!   checks decoded slices against.
//!
//! Recovery (`RecvSlic`) packets are still omitted: they only matter for actual
//! repair, which these fixtures do not exercise. `verify` reports the recovery
//! set as unrepairable (zero recovery blocks), which is fine — the assertions
//! here are about slice checksums and filenames, not repair.
//!
//! Packet layout implemented here (little-endian), per the PAR 2.0 spec:
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

const MAGIC: &[u8; 8] = b"PAR2\x00PKT";
const TYPE_MAIN: &[u8; 16] = b"PAR 2.0\x00Main\x00\x00\x00\x00";
const TYPE_FILE_DESC: &[u8; 16] = b"PAR 2.0\x00FileDesc";
const TYPE_IFSC: &[u8; 16] = b"PAR 2.0\x00IFSC\x00\x00\x00\x00";

/// One file recorded in the recovery set.
struct FileEntry {
    /// The name PAR2 considers canonical — what a rename should restore.
    expected_name: String,
    /// File ID (spec: MD5 of hash_16k + size + name).
    file_id: [u8; 16],
    hash: [u8; 16],
    hash_16k: [u8; 16],
    size: u64,
    /// Per-slice (MD5, CRC32) checksums, in slice order.
    slices: Vec<([u8; 16], u32)>,
}

/// Builds a PAR2 index file describing a set of files by content.
pub struct Par2Fixture {
    slice_size: u64,
    recovery_set_id: [u8; 16],
    files: Vec<FileEntry>,
}

impl Par2Fixture {
    /// Start a new recovery set. `slice_size` must be a multiple of 4.
    pub fn new() -> Self {
        Self {
            slice_size: 4096,
            // Fixed, not random: tests must be deterministic.
            recovery_set_id: *b"rustnzbfixture01",
            files: Vec::new(),
        }
    }

    // Not every integration-test binary that links this support module uses
    // this builder; the per-binary compile flags it as dead there.
    #[allow(dead_code)]
    pub fn with_recovery_set_id(mut self, recovery_set_id: [u8; 16]) -> Self {
        self.recovery_set_id = recovery_set_id;
        self
    }

    /// Record `contents` under the canonical name PAR2 should report.
    ///
    /// This does not write the file to disk — the test decides whether to
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

        // Per-slice checksums for the IFSC packet. Each slice is hashed
        // zero-padded to the slice size, matching PAR2 and rust_par2's verifier.
        let slice_size = self.slice_size as usize;
        let mut slices = Vec::new();
        let mut off = 0;
        while off < contents.len() {
            let end = (off + slice_size).min(contents.len());
            let mut chunk = contents[off..end].to_vec();
            chunk.resize(slice_size, 0);
            let slice_md5: [u8; 16] = Md5::digest(&chunk).into();
            let mut crc = crc32fast::Hasher::new();
            crc.update(&chunk);
            slices.push((slice_md5, crc.finalize()));
            off = end;
        }

        self.files.push(FileEntry {
            expected_name: expected_name.to_string(),
            file_id,
            hash,
            hash_16k,
            size,
            slices,
        });
        self
    }

    /// Write the index `.par2` file to `path`.
    pub fn write_index(&self, path: &Path) {
        let mut out = Vec::new();

        // Main packet: slice size, file count, then the file IDs in order.
        let mut main_body = Vec::new();
        main_body.extend_from_slice(&self.slice_size.to_le_bytes());
        main_body.extend_from_slice(&(self.files.len() as u32).to_le_bytes());
        for file in &self.files {
            main_body.extend_from_slice(&file.file_id);
        }
        out.extend_from_slice(&self.packet(TYPE_MAIN, &main_body));

        // One FileDesc packet per file.
        for file in &self.files {
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

        // One IFSC packet per file: file ID then (MD5[16] + CRC32[4]) per slice.
        // 16 + 20*n is always 4-aligned, so the packet length stays valid.
        for file in &self.files {
            let mut body = Vec::new();
            body.extend_from_slice(&file.file_id);
            for (slice_md5, crc32) in &file.slices {
                body.extend_from_slice(slice_md5);
                body.extend_from_slice(&crc32.to_le_bytes());
            }
            out.extend_from_slice(&self.packet(TYPE_IFSC, &body));
        }

        std::fs::write(path, &out).unwrap();
    }

    /// Frame `body` as a PAR2 packet with a correct length and MD5.
    ///
    /// The parser recomputes the MD5 over everything from offset 32 and skips
    /// packets that don't match, so this must be exact — a wrong hash makes
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
