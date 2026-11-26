//! Detect encrypted RAR volumes from their headers alone.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

const RAR4: &[u8] = b"Rar!\x1a\x07\x00";
const RAR5: &[u8] = b"Rar!\x1a\x07\x01\x00";
/// Headers read before giving up; archives put encrypted data up front.
const MAX_BLOCKS: usize = 64;
const MAX_HEADER: u64 = 2 * 1024 * 1024;

/// Whether `path` is a RAR volume with encrypted headers or file data.
/// Anything else, including an unreadable or malformed file, is `false`.
pub fn rar_is_encrypted(path: &Path) -> bool {
    let inner = || -> Option<bool> {
        let mut file = File::open(path).ok()?;
        let mut signature = [0; 8];
        file.read_exact(&mut signature).ok()?;
        if signature == RAR5 {
            rar5_encrypted(&mut file)
        } else if signature.starts_with(RAR4) {
            file.seek(SeekFrom::Start(RAR4.len() as u64)).ok()?;
            rar4_encrypted(&mut file)
        } else {
            None
        }
    };
    inner().unwrap_or(false)
}

fn rar4_encrypted(file: &mut File) -> Option<bool> {
    for _ in 0..MAX_BLOCKS {
        let start = file.stream_position().ok()?;
        let mut head = [0; 7];
        file.read_exact(&mut head).ok()?;
        let flags = u16::from_le_bytes([head[3], head[4]]);
        let size = u64::from(u16::from_le_bytes([head[5], head[6]]));
        let mut data: u64 = 0;
        match head[2] {
            // Main header: MHD_PASSWORD.
            0x73 if flags & 0x0080 != 0 => return Some(true),
            // File header: LHD_PASSWORD.
            0x74 if flags & 0x0004 != 0 => return Some(true),
            0x7b => return Some(false),
            _ => {}
        }
        if flags & 0x8000 != 0 || head[2] == 0x74 {
            let mut low = [0; 4];
            file.read_exact(&mut low).ok()?;
            data = u64::from(u32::from_le_bytes(low));
            // LHD_LARGE: the high half of the packed size is at offset 32.
            if head[2] == 0x74 && flags & 0x0100 != 0 {
                let mut high = [0; 4];
                file.seek(SeekFrom::Start(start + 32)).ok()?;
                file.read_exact(&mut high).ok()?;
                data |= u64::from(u32::from_le_bytes(high)) << 32;
            }
        }
        if size < 7 {
            return None;
        }
        file.seek(SeekFrom::Start(start.checked_add(size)?.checked_add(data)?))
            .ok()?;
    }
    Some(false)
}

fn vint(bytes: &mut &[u8]) -> Option<u64> {
    let mut value: u64 = 0;
    for shift in 0..10 {
        let (&byte, rest) = bytes.split_first()?;
        *bytes = rest;
        value |= u64::from(byte & 0x7f).checked_shl(shift * 7)?;
        if byte & 0x80 == 0 {
            return Some(value);
        }
    }
    None
}

fn rar5_encrypted(file: &mut File) -> Option<bool> {
    for _ in 0..MAX_BLOCKS {
        // CRC32, then the header size as a vint of at most three bytes.
        let start = file.stream_position().ok()?;
        let mut head = [0; 7];
        let read = file.read(&mut head).ok()?;
        let mut cursor = head[..read].get(4..)?;
        let before = cursor.len();
        let size = vint(&mut cursor)?;
        let header_at = start + 4 + (before - cursor.len()) as u64;
        if size == 0 || size > MAX_HEADER {
            return None;
        }
        let mut header = vec![0; size as usize];
        file.seek(SeekFrom::Start(header_at)).ok()?;
        file.read_exact(&mut header).ok()?;

        let mut rest = header.as_slice();
        let kind = vint(&mut rest)?;
        // Archive encryption header: everything after it is encrypted.
        if kind == 4 {
            return Some(true);
        }
        if kind == 5 {
            return Some(false);
        }
        let flags = vint(&mut rest)?;
        let extra_size = if flags & 1 != 0 { vint(&mut rest)? } else { 0 };
        let data_size = if flags & 2 != 0 { vint(&mut rest)? } else { 0 };
        if kind == 2 {
            let extra_at = header
                .len()
                .checked_sub(usize::try_from(extra_size).ok()?)?;
            let mut extra = &header[extra_at..];
            while !extra.is_empty() {
                let record_size = usize::try_from(vint(&mut extra)?).ok()?;
                let mut record = extra.get(..record_size)?;
                extra = &extra[record_size..];
                // File encryption record.
                if vint(&mut record)? == 1 {
                    return Some(true);
                }
            }
        }
        file.seek(SeekFrom::Start(
            header_at.checked_add(size)?.checked_add(data_size)?,
        ))
        .ok()?;
    }
    Some(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(bytes: &[u8]) -> bool {
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), bytes).unwrap();
        rar_is_encrypted(file.path())
    }

    fn rar5(blocks: &[&[u8]]) -> Vec<u8> {
        let mut out = RAR5.to_vec();
        for block in blocks {
            out.extend([0; 4]);
            out.push(block.len() as u8);
            out.extend(*block);
        }
        out
    }

    fn rar4_block(kind: u8, flags: u16, size: u16, rest: &[u8]) -> Vec<u8> {
        let mut out = vec![0, 0, kind];
        out.extend(flags.to_le_bytes());
        out.extend(size.to_le_bytes());
        out.extend(rest);
        out
    }

    /// Main header: type 1, no flags, archive flags 0.
    const MAIN5: &[u8] = &[1, 0, 0];

    #[test]
    fn rar5_header_and_file_encryption_are_found() {
        assert!(check(&rar5(&[&[4, 0, 0, 0, 15]])));
        // File header with an extra area holding an encryption record.
        assert!(check(&rar5(&[
            MAIN5,
            &[2, 1, 3, 0, 0, 0, 0, 0, 1, b'a', 2, 1, 0]
        ])));
        // The same with a hash record (type 2) only.
        assert!(!check(&rar5(&[
            MAIN5,
            &[2, 1, 3, 0, 0, 0, 0, 0, 1, b'a', 2, 2, 0]
        ])));
        assert!(!check(&rar5(&[MAIN5, &[5, 0, 0]])));
    }

    #[test]
    fn rar5_data_areas_are_skipped() {
        let mut bytes = rar5(&[MAIN5, &[2, 2, 3, 0, 0, 0, 0, 0, 1, b'a']]);
        bytes.extend([4, 4, 4]);
        bytes.extend([0, 0, 0, 0, 13]);
        bytes.extend([2, 1, 3, 0, 0, 0, 0, 0, 1, b'b', 2, 1, 0]);
        assert!(check(&bytes));
    }

    #[test]
    fn rar4_header_and_file_encryption_are_found() {
        let archive = |main: u16, file: u16| {
            let mut out = RAR4.to_vec();
            out.extend(rar4_block(0x73, main, 13, &[0; 6]));
            out.extend(rar4_block(0x74, 0x8000 | file, 32, &[0; 25]));
            out
        };
        assert!(check(&archive(0x0080, 0)));
        assert!(check(&archive(0, 0x0004)));
        assert!(!check(&archive(0, 0)));
    }

    #[test]
    fn other_and_malformed_files_are_not_encrypted() {
        assert!(!check(b""));
        assert!(!check(b"PK\x03\x04 something"));
        assert!(!check(RAR5));
        assert!(!check(&rar5(&[&[2, 1, 200, 1]])));
        let mut huge = RAR5.to_vec();
        huge.extend([0xff; 64]);
        assert!(!check(&huge));
        assert!(!rar_is_encrypted(Path::new("/nonexistent")));
    }
}
