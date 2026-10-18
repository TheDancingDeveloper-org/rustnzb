//! Map NZB files to PAR2 files (WI-144).
//!
//! Subject names in an NZB are often obfuscated, so a downloaded file's name
//! cannot be trusted to match the name in the PAR2 recovery set. PAR2 records
//! the MD5 of each file's first 16 KiB (`hash_16k`), which identifies a file
//! from its first article alone. This is the shared home of that matcher; the
//! download engine and the streamer both use it. Inputs are plain data so this
//! crate needs no PAR2 parser.

use std::collections::{HashMap, HashSet};
use std::hash::Hash;

/// Bytes covered by a PAR2 `hash_16k`.
pub const PAR2_HASH_16K_LEN: usize = 16 * 1024;

/// One file as described by a PAR2 FileDesc packet.
#[derive(Debug, Clone)]
pub struct Par2FileRef<'a, P> {
    pub id: P,
    pub name: &'a str,
    pub hash_16k: [u8; 16],
    pub size: u64,
}

/// One NZB file to match. `first_16k_md5` is the MD5 of its first
/// [`PAR2_HASH_16K_LEN`] bytes (or of the whole file if shorter), `None` when
/// those bytes have not arrived yet; `size` is `None` when not yet known.
#[derive(Debug, Clone)]
pub struct CandidateFile<'a, N> {
    pub id: N,
    pub name: &'a str,
    pub first_16k_md5: Option<[u8; 16]>,
    pub size: Option<u64>,
}

/// Match NZB files to PAR2 files: exact name first, then `hash_16k` (and size
/// when both sides know it). Each PAR2 file is claimed at most once; files that
/// match nothing are absent from the result.
pub fn match_files_by_hash16k<P, N>(
    par2_files: &[Par2FileRef<'_, P>],
    candidates: &[CandidateFile<'_, N>],
) -> HashMap<N, P>
where
    P: Clone + Eq + Hash,
    N: Clone + Eq + Hash,
{
    let mut out = HashMap::new();
    let mut claimed: HashSet<usize> = HashSet::new();

    for cand in candidates {
        if let Some(pi) = par2_files
            .iter()
            .position(|p| p.name == cand.name)
            .filter(|pi| !claimed.contains(pi))
        {
            claimed.insert(pi);
            out.insert(cand.id.clone(), par2_files[pi].id.clone());
        }
    }

    for cand in candidates {
        if out.contains_key(&cand.id) {
            continue;
        }
        let Some(md5) = cand.first_16k_md5 else {
            continue;
        };
        let found = par2_files.iter().enumerate().position(|(pi, p)| {
            !claimed.contains(&pi) && p.hash_16k == md5 && cand.size.is_none_or(|s| s == p.size)
        });
        if let Some(pi) = found {
            claimed.insert(pi);
            out.insert(cand.id.clone(), par2_files[pi].id.clone());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(id: u8, name: &str, h: u8, size: u64) -> Par2FileRef<'_, u8> {
        Par2FileRef {
            id,
            name,
            hash_16k: [h; 16],
            size,
        }
    }
    fn c(id: u32, name: &str, h: Option<u8>, size: Option<u64>) -> CandidateFile<'_, u32> {
        CandidateFile {
            id,
            name,
            first_16k_md5: h.map(|h| [h; 16]),
            size,
        }
    }

    #[test]
    fn name_match_wins() {
        let par2 = [p(1, "a.mkv", 1, 10), p(2, "b.nfo", 2, 5)];
        let cands = [c(10, "b.nfo", None, None), c(11, "a.mkv", Some(9), None)];
        let m = match_files_by_hash16k(&par2, &cands);
        assert_eq!(m[&10], 2);
        assert_eq!(m[&11], 1);
    }

    #[test]
    fn obfuscated_names_match_by_hash() {
        let par2 = [p(1, "a.mkv", 1, 10), p(2, "b.nfo", 2, 5)];
        let cands = [
            c(10, "x7f3", Some(2), Some(5)),
            c(11, "q9z1", Some(1), None),
        ];
        let m = match_files_by_hash16k(&par2, &cands);
        assert_eq!(m[&10], 2);
        assert_eq!(m[&11], 1);
    }

    #[test]
    fn size_disambiguates_and_each_par2_file_claimed_once() {
        // Two PAR2 files share a 16 KiB prefix; size decides.
        let par2 = [p(1, "a", 7, 100), p(2, "b", 7, 200)];
        let cands = [
            c(10, "x", Some(7), Some(200)),
            c(11, "y", Some(7), Some(100)),
            c(12, "z", Some(7), Some(100)),
        ];
        let m = match_files_by_hash16k(&par2, &cands);
        assert_eq!(m[&10], 2);
        assert_eq!(m[&11], 1);
        assert!(!m.contains_key(&12));
    }

    #[test]
    fn unknown_hash_is_unmatched() {
        let par2 = [p(1, "a", 1, 10)];
        let m = match_files_by_hash16k(
            &par2,
            &[c(10, "x", None, Some(10)), c(11, "y", Some(3), None)],
        );
        assert!(m.is_empty());
    }
}
