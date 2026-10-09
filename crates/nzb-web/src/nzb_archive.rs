//! Unpacking of compressed NZB uploads shared by the HTTP API and the watch
//! folder: `.gz`, `.bz2`, and `.zip` archives are expanded into the `.nzb`
//! documents they carry, with a bound on the decompressed size.

use std::io::{Cursor, Read as _};

use bzip2::read::BzDecoder;
use flate2::read::GzDecoder;

/// Upper bound on the decompressed size of NZB content taken from one archive.
pub const MAX_NZB_DECOMPRESSED_BYTES: u64 = 100 * 1024 * 1024;

/// Upper bound on the number of `.nzb` entries taken from one zip. A central
/// directory can name far more entries than the decompressed budget could ever
/// hold, and each one costs an allocation.
const MAX_ZIP_NZB_ENTRIES: usize = 10_000;

/// Extract NZB files from an uploaded file. If it's an archive (zip, gz, bz2),
/// returns all `.nzb` entries found inside. Otherwise returns the file as-is.
pub fn extract_nzbs(file_name: &str, data: &[u8]) -> Result<Vec<(String, Vec<u8>)>, anyhow::Error> {
    extract_nzbs_bounded(file_name, data, MAX_NZB_DECOMPRESSED_BYTES)
}

fn extract_nzbs_bounded(
    file_name: &str,
    data: &[u8],
    max_bytes: u64,
) -> Result<Vec<(String, Vec<u8>)>, anyhow::Error> {
    let lower = file_name.to_lowercase();

    // .nzb.gz or .gz containing an nzb
    if lower.ends_with(".gz") {
        let decompressed = read_bounded(GzDecoder::new(data), "gzip", max_bytes)?;
        let inner_name = &file_name[..file_name.len() - ".gz".len()];
        return Ok(vec![(inner_name.to_string(), decompressed)]);
    }

    // .nzb.bz2 or .bz2 containing an nzb
    if lower.ends_with(".bz2") {
        let decompressed = read_bounded(BzDecoder::new(data), "bzip2", max_bytes)?;
        let inner_name = &file_name[..file_name.len() - ".bz2".len()];
        return Ok(vec![(inner_name.to_string(), decompressed)]);
    }

    // .zip archive — extract all .nzb files inside
    if lower.ends_with(".zip") {
        let cursor = Cursor::new(data);
        let mut archive = zip::ZipArchive::new(cursor)
            .map_err(|e| anyhow::anyhow!("Failed to read zip archive: {e}"))?;
        // Overlapping central-directory entries let one compressed span be
        // counted once per entry, so the size budget would under-count the
        // bytes actually produced. Such archives are rejected outright.
        if archive
            .has_overlapping_files()
            .map_err(|e| anyhow::anyhow!("Failed to read zip archive: {e}"))?
        {
            anyhow::bail!("Zip archive '{file_name}' contains overlapping files");
        }
        let mut nzbs = Vec::new();
        let mut total_uncompressed = 0u64;
        for i in 0..archive.len() {
            let mut entry = archive
                .by_index(i)
                .map_err(|e| anyhow::anyhow!("Zip entry error: {e}"))?;
            let entry_name = entry.name().to_string();
            if entry_name.to_lowercase().ends_with(".nzb") {
                if nzbs.len() >= MAX_ZIP_NZB_ENTRIES {
                    anyhow::bail!("Zip archive '{file_name}' contains too many NZB files");
                }
                // Budget on the bytes actually read, never the declared
                // `entry.size()`: the central directory's uncompressed size is
                // attacker-controlled and zip does not enforce it.
                let room = max_bytes
                    .saturating_sub(total_uncompressed)
                    .saturating_add(1);
                let mut buf = Vec::new();
                entry
                    .by_ref()
                    .take(room)
                    .read_to_end(&mut buf)
                    .map_err(|e| anyhow::anyhow!("Failed to read zip entry '{entry_name}': {e}"))?;
                total_uncompressed = total_uncompressed
                    .checked_add(buf.len() as u64)
                    .ok_or_else(|| anyhow::anyhow!("Zip archive size overflow"))?;
                if total_uncompressed > max_bytes {
                    anyhow::bail!(
                        "Decompressed NZB exceeds the {} MB limit",
                        MAX_NZB_DECOMPRESSED_BYTES / 1024 / 1024
                    );
                }
                nzbs.push((entry_name, buf));
            }
        }
        if nzbs.is_empty() {
            anyhow::bail!("No .nzb files found in zip archive '{file_name}'");
        }
        return Ok(nzbs);
    }

    // Plain .nzb or unrecognized — pass through as-is
    Ok(vec![(file_name.to_string(), data.to_vec())])
}

fn read_bounded(
    reader: impl std::io::Read,
    format: &str,
    max_bytes: u64,
) -> Result<Vec<u8>, anyhow::Error> {
    let mut decompressed = Vec::new();
    reader
        .take(max_bytes + 1)
        .read_to_end(&mut decompressed)
        .map_err(|e| anyhow::anyhow!("Failed to decompress {format}: {e}"))?;
    if decompressed.len() as u64 > max_bytes {
        anyhow::bail!(
            "Decompressed NZB exceeds the {} MB limit",
            MAX_NZB_DECOMPRESSED_BYTES / 1024 / 1024
        );
    }
    Ok(decompressed)
}

#[cfg(test)]
mod tests {
    use super::{extract_nzbs, extract_nzbs_bounded};
    use std::io::Write;

    use zip::CompressionMethod;
    use zip::write::SimpleFileOptions;

    fn build_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let cursor = std::io::Cursor::new(Vec::new());
        let mut writer = zip::ZipWriter::new(cursor);
        let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);

        for (name, contents) in entries {
            writer.start_file(name, options).unwrap();
            writer.write_all(contents).unwrap();
        }

        writer.finish().unwrap().into_inner()
    }

    /// An entry whose central-directory header claims a far smaller
    /// uncompressed size than the bytes it actually decompresses to.
    fn build_zip_underdeclared(name: &str, contents: &[u8], declared: u64) -> Vec<u8> {
        let mut zip = build_zip(&[(name, contents)]);
        let name_bytes = name.as_bytes();
        let mut patched = 0;
        let mut i = 0;
        while i + 30 < zip.len() {
            if zip[i..i + 4] == [0x50, 0x4b, 0x01, 0x02] {
                let name_len = u16::from_le_bytes([zip[i + 28], zip[i + 29]]) as usize;
                if i + 46 + name_len <= zip.len() && &zip[i + 46..i + 46 + name_len] == name_bytes {
                    zip[i + 24..i + 28].copy_from_slice(&(declared as u32).to_le_bytes());
                    patched += 1;
                }
            }
            i += 1;
        }
        assert_eq!(patched, 1, "central directory entry not patched");
        zip
    }

    #[test]
    fn extract_nzbs_rejects_zip_entries_that_underdeclare_their_size() {
        // Two entries each declare 1 byte but decompress to just over half the
        // cap, so the declared total (2) is inside the budget while the bytes
        // actually produced are not. The declared size must not be trusted.
        // A small cap stands in for the production 100 MB one: the check is on
        // bytes read, so the magnitude does not matter.
        let cap: u64 = 64;
        let payload = vec![b'x'; (cap / 2 + 1) as usize];
        let zip = build_zip_underdeclared("one.nzb", &payload, 1);
        let second = build_zip_underdeclared("two.nzb", &payload, 1);
        // A zip is not two concatenated archives; rebuild it with both entries.
        let _ = (zip, second);
        let cursor = std::io::Cursor::new(Vec::new());
        let mut writer = zip::ZipWriter::new(cursor);
        let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
        for name in ["one.nzb", "two.nzb"] {
            writer.start_file(name, options).unwrap();
            writer.write_all(&payload).unwrap();
        }
        let mut zip = writer.finish().unwrap().into_inner();
        let mut patched = 0;
        let mut i = 0;
        while i + 30 < zip.len() {
            if zip[i..i + 4] == [0x50, 0x4b, 0x01, 0x02] {
                zip[i + 24..i + 28].copy_from_slice(&1u32.to_le_bytes());
                patched += 1;
            }
            i += 1;
        }
        assert_eq!(patched, 2, "both central directory entries patched");

        let err = extract_nzbs_bounded("underdeclared.zip", &zip, cap).unwrap_err();
        assert!(
            err.to_string().contains("limit"),
            "under-declared sizes must still hit the cap, got: {err}"
        );
    }

    #[test]
    fn extract_nzbs_rejects_zip_bombs() {
        let cap: u64 = 64;
        let oversized = vec![b'x'; (cap + 1) as usize];
        let zip = build_zip(&[("oversized.nzb", oversized.as_slice())]);
        let err = extract_nzbs_bounded("oversized.zip", &zip, cap).unwrap_err();
        assert!(err.to_string().contains("limit"));
    }

    #[test]
    fn extract_nzbs_accepts_small_zip_nzb() {
        let zip = build_zip(&[("sample.nzb", br#"<nzb><file subject="ok" /></nzb>"#)]);
        let nzbs = extract_nzbs("sample.zip", &zip).unwrap();
        assert_eq!(nzbs.len(), 1);
        assert_eq!(nzbs[0].0, "sample.nzb");
        assert_eq!(nzbs[0].1, br#"<nzb><file subject="ok" /></nzb>"#);
    }

    #[test]
    fn extract_nzbs_unpacks_gzip_and_bzip2() {
        let body = br#"<nzb><file subject="ok" /></nzb>"#;

        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(body).unwrap();
        let nzbs = extract_nzbs("sample.nzb.GZ", &gz.finish().unwrap()).unwrap();
        assert_eq!(nzbs, vec![("sample.nzb".to_string(), body.to_vec())]);

        let mut bz = bzip2::write::BzEncoder::new(Vec::new(), bzip2::Compression::default());
        bz.write_all(body).unwrap();
        let nzbs = extract_nzbs("sample.nzb.bz2", &bz.finish().unwrap()).unwrap();
        assert_eq!(nzbs, vec![("sample.nzb".to_string(), body.to_vec())]);
    }
}
