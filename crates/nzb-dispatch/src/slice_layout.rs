//! Build the in-flight verifier's [`SliceLayout`] from a parsed PAR2 set
//! (WI-144).

use nzb_decode::{FileSlices, SliceDigest, SliceLayout};
use rust_par2::Par2FileSet;

/// Convert a parsed PAR2 file set into a [`SliceLayout`].
///
/// Files follow `set.file_order` (Main-packet order), so layout file index `i`
/// is `set.file_order[i]` and a slice's global index is the slice count of
/// every earlier file plus its local index. A file ID in the Main packet with
/// no FileDesc packet is skipped, as `rust-par2` itself does.
pub fn slice_layout_from_par2(set: &Par2FileSet) -> SliceLayout {
    let files = set
        .file_order
        .iter()
        .filter_map(|id| set.files.get(id))
        .map(|f| FileSlices {
            size: f.size,
            slices: f
                .slices
                .iter()
                .map(|s| SliceDigest {
                    md5: s.md5,
                    crc32: s.crc32,
                })
                .collect(),
        })
        .collect();
    SliceLayout {
        slice_size: set.slice_size,
        files,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use md5::{Digest, Md5};
    use mock_nntp_server::damage::DamageRelease;
    use nzb_core::{CandidateFile, PAR2_HASH_16K_LEN, Par2FileRef, match_files_by_hash16k};
    use nzb_decode::SliceVerifier;
    use std::io::Cursor;

    fn parse(release: &DamageRelease) -> Par2FileSet {
        let bytes = &release.par2_index.payload;
        rust_par2::parse_par2_reader(&mut Cursor::new(bytes), bytes.len() as u64).unwrap()
    }

    #[test]
    fn layout_matches_fixture_geometry() {
        let release = DamageRelease::generate("layout", 7);
        let set = parse(&release);
        let layout = slice_layout_from_par2(&set);
        assert_eq!(layout.slice_size, release.par2.slice_size());
        let verifier = SliceVerifier::new(&layout);
        assert_eq!(
            verifier.total_slices(),
            release.par2.total_slices() as usize
        );

        let mut base = 0u32;
        for (i, id) in set.file_order.iter().enumerate() {
            let file = &set.files[id];
            assert_eq!(layout.files[i].size, file.size);
            assert_eq!(
                Some(base),
                release.par2.first_global_slice(&file.filename),
                "{}",
                file.filename
            );
            base += layout.files[i].slices.len() as u32;
        }
    }

    #[test]
    fn layout_verifies_clean_release() {
        let release = DamageRelease::generate("clean", 11);
        let set = parse(&release);
        let mut v = SliceVerifier::new(&slice_layout_from_par2(&set));
        for (fi, id) in set.file_order.iter().enumerate() {
            let data = &release.data_file(&set.files[id].filename).unwrap().payload;
            for g in v.mark_written(fi, 0, data.len() as u64) {
                let (_, start, len) = v.slice_range(g).unwrap();
                let bytes = &data[start as usize..(start + len) as usize];
                assert_eq!(v.verify(g, bytes), nzb_decode::SliceState::Verified);
            }
        }
        assert!(v.is_complete());
    }

    #[test]
    fn obfuscated_files_map_by_hash_16k() {
        let release = DamageRelease::generate("obf", 3);
        let set = parse(&release);
        let par2: Vec<Par2FileRef<'_, [u8; 16]>> = set
            .file_order
            .iter()
            .map(|id| {
                let f = &set.files[id];
                Par2FileRef {
                    id: *id,
                    name: &f.filename,
                    hash_16k: f.hash_16k,
                    size: f.size,
                }
            })
            .collect();
        let names: Vec<String> = (0..release.data_files.len())
            .map(|i| format!("a{i}b9c{}.bin", 7 * i + 3))
            .collect();
        let cands: Vec<CandidateFile<'_, usize>> = release
            .data_files
            .iter()
            .enumerate()
            .map(|(i, f)| {
                let head = &f.payload[..f.payload.len().min(PAR2_HASH_16K_LEN)];
                CandidateFile {
                    id: i,
                    name: &names[i],
                    first_16k_md5: Some(Md5::digest(head).into()),
                    size: Some(f.payload.len() as u64),
                }
            })
            .collect();
        let m = match_files_by_hash16k(&par2, &cands);
        assert_eq!(m.len(), release.data_files.len());
        for (i, f) in release.data_files.iter().enumerate() {
            assert_eq!(set.files[&m[&i]].filename, f.filename);
        }
    }
}
