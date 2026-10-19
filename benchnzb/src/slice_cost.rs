//! Per-article cost of in-flight PAR2 slice verification (WI-1229).
//!
//! rustnzb's download engine reads every completed PAR2 slice back from disk
//! and hashes it (MD5 and CRC32 over the slice, zero-padded to the slice size)
//! as articles land. This case runs the same synthetic job twice, once with
//! that work ("verifier on") and once without ("verifier off"), and reports the
//! slice-verify CPU cost per article next to the yEnc decode cost per article
//! on the host it runs on. The verify work mirrors `SliceVerifier::verify` in
//! `nzb-decode`; slices tile the file, so every decoded byte is read back and
//! hashed exactly once.

use std::io::{Read, Seek, SeekFrom, Write};
use std::time::{Duration, Instant};

use md5::{Digest, Md5};
use serde::Serialize;

#[derive(Debug, Serialize)]
pub struct SliceCostReport {
    pub articles: usize,
    pub article_bytes: usize,
    pub slice_bytes: usize,
    pub slices: usize,
    /// Wall time of the job with the verifier off: yEnc decode and write.
    pub off_secs: f64,
    /// Wall time of the job with the verifier on: decode, write, read-back
    /// and hash every slice.
    pub on_secs: f64,
    pub decode_ns_per_article: f64,
    pub slice_verify_ns_per_article: f64,
    /// Slice verify cost over decode cost. Below 1.0 means verification costs
    /// less CPU per article than decoding it.
    pub verify_over_decode: f64,
}

/// Run the job `rounds` times each way and keep the fastest round of each.
pub fn run(
    articles: usize,
    article_bytes: usize,
    slice_bytes: usize,
    rounds: usize,
) -> anyhow::Result<SliceCostReport> {
    anyhow::ensure!(articles > 0 && article_bytes > 0 && slice_bytes > 0 && rounds > 0);
    let total = articles * article_bytes;
    let payload: Vec<u8> = (0..total as u64)
        .map(|i| (i.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 56) as u8)
        .collect();
    let encoded: Vec<Vec<u8>> = (0..articles)
        .map(|n| {
            let start = n * article_bytes;
            crate::yenc::encode_article(
                &payload[start..start + article_bytes],
                "slice-cost.bin",
                n as u32 + 1,
                articles as u32,
                start as u64,
                total as u64,
            )
            .0
        })
        .collect();
    let dir = std::env::temp_dir().join(format!("benchnzb-slice-cost-{}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("slice-cost.bin");

    let mut off = Duration::MAX;
    let mut on = Duration::MAX;
    let mut decode = Duration::MAX;
    let mut verify = Duration::MAX;
    for _ in 0..rounds {
        let (wall, d, _) = job(&path, &encoded, total, slice_bytes, false)?;
        off = off.min(wall);
        decode = decode.min(d);
        let (wall, _, v) = job(&path, &encoded, total, slice_bytes, true)?;
        on = on.min(wall);
        verify = verify.min(v);
    }
    let _ = std::fs::remove_dir_all(&dir);

    let per = |d: Duration| d.as_nanos() as f64 / articles as f64;
    let report = SliceCostReport {
        articles,
        article_bytes,
        slice_bytes,
        slices: total.div_ceil(slice_bytes),
        off_secs: off.as_secs_f64(),
        on_secs: on.as_secs_f64(),
        decode_ns_per_article: per(decode),
        slice_verify_ns_per_article: per(verify),
        verify_over_decode: verify.as_secs_f64() / decode.as_secs_f64(),
    };
    Ok(report)
}

/// One job: decode and write each article; with `verify`, read back and hash
/// each slice as soon as the articles covering it are written. Returns (wall,
/// decode time, verify time).
fn job(
    path: &std::path::Path,
    encoded: &[Vec<u8>],
    total: usize,
    slice_bytes: usize,
    verify: bool,
) -> anyhow::Result<(Duration, Duration, Duration)> {
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .read(true)
        .write(true)
        .open(path)?;
    let mut decode = Duration::ZERO;
    let mut hashing = Duration::ZERO;
    let mut written = 0usize;
    let mut next_slice = 0usize;
    let mut slice = vec![0u8; slice_bytes];
    let start = Instant::now();
    for article in encoded {
        let t = Instant::now();
        let decoded =
            yenc_simd::decode_yenc(article).map_err(|e| anyhow::anyhow!("decode: {e:?}"))?;
        decode += t.elapsed();
        file.seek(SeekFrom::Start(written as u64))?;
        file.write_all(&decoded.data)?;
        written += decoded.data.len();
        if !verify {
            continue;
        }
        let t = Instant::now();
        while next_slice * slice_bytes < written
            && ((next_slice + 1) * slice_bytes <= written || written == total)
        {
            let offset = next_slice * slice_bytes;
            let len = slice_bytes.min(total - offset);
            file.seek(SeekFrom::Start(offset as u64))?;
            slice.resize(slice_bytes, 0);
            file.read_exact(&mut slice[..len])?;
            slice[len..].fill(0);
            let md5: [u8; 16] = Md5::digest(&slice).into();
            let mut crc = crc32fast::Hasher::new();
            crc.update(&slice);
            std::hint::black_box((md5, crc.finalize()));
            next_slice += 1;
        }
        hashing += t.elapsed();
    }
    Ok((start.elapsed(), decode, hashing))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_job_reports_both_costs() {
        let report = run(8, 64 * 1024, 100_000, 1).unwrap();
        assert_eq!(report.slices, (8 * 64 * 1024usize).div_ceil(100_000));
        assert!(report.decode_ns_per_article > 0.0);
        assert!(report.slice_verify_ns_per_article > 0.0);
        assert!(report.verify_over_decode > 0.0);
    }
}
