//! Unpacking of compressed NZB uploads shared by the HTTP API and the watch
//! folder: `.gz`, `.bz2`, and `.zip` archives are expanded into the `.nzb`
//! documents they carry, with a bound on the decompressed size.

use std::io::{Cursor, Read as _};

use bzip2::read::BzDecoder;
use flate2::read::GzDecoder;

/// Upper bound on the decompressed size of NZB content taken from one archive.
pub const MAX_NZB_DECOMPRESSED_BYTES: u64 = 100 * 1024 * 1024;

/// Extract NZB files from an uploaded file. If it's an archive (zip, gz, bz2),
/// returns all `.nzb` entries found inside. Otherwise returns the file as-is.
pub fn extract_nzbs(file_name: &str, data: &[u8]) -> Result<Vec<(String, Vec<u8>)>, anyhow::Error> {
    let lower = file_name.to_lowercase();

    // .nzb.gz or .gz containing an nzb
    if lower.ends_with(".gz") {
        let decompressed = read_bounded(GzDecoder::new(data), "gzip")?;
        let inner_name = &file_name[..file_name.len() - ".gz".len()];
        return Ok(vec![(inner_name.to_string(), decompressed)]);
    }

    // .nzb.bz2 or .bz2 containing an nzb
    if lower.ends_with(".bz2") {
        let decompressed = read_bounded(BzDecoder::new(data), "bzip2")?;
        let inner_name = &file_name[..file_name.len() - ".bz2".len()];
        return Ok(vec![(inner_name.to_string(), decompressed)]);
    }

    // .zip archive — extract all .nzb files inside
    if lower.ends_with(".zip") {
        let cursor = Cursor::new(data);
        let mut archive = zip::ZipArchive::new(cursor)
            .map_err(|e| anyhow::anyhow!("Failed to read zip archive: {e}"))?;
        let mut nzbs = Vec::new();
        let mut total_uncompressed = 0u64;
        for i in 0..archive.len() {
            let mut entry = archive
                .by_index(i)
                .map_err(|e| anyhow::anyhow!("Zip entry error: {e}"))?;
            let entry_name = entry.name().to_string();
            if entry_name.to_lowercase().ends_with(".nzb") {
                total_uncompressed = total_uncompressed
                    .checked_add(entry.size())
                    .ok_or_else(|| anyhow::anyhow!("Zip archive size overflow"))?;
                if total_uncompressed > MAX_NZB_DECOMPRESSED_BYTES {
                    anyhow::bail!(
                        "Decompressed NZB exceeds the {} MB limit",
                        MAX_NZB_DECOMPRESSED_BYTES / 1024 / 1024
                    );
                }

                let mut buf = Vec::new();
                entry
                    .by_ref()
                    .take(MAX_NZB_DECOMPRESSED_BYTES + 1)
                    .read_to_end(&mut buf)
                    .map_err(|e| anyhow::anyhow!("Failed to read zip entry '{entry_name}': {e}"))?;
                if buf.len() as u64 > MAX_NZB_DECOMPRESSED_BYTES {
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

fn read_bounded(reader: impl std::io::Read, format: &str) -> Result<Vec<u8>, anyhow::Error> {
    let mut decompressed = Vec::new();
    reader
        .take(MAX_NZB_DECOMPRESSED_BYTES + 1)
        .read_to_end(&mut decompressed)
        .map_err(|e| anyhow::anyhow!("Failed to decompress {format}: {e}"))?;
    if decompressed.len() as u64 > MAX_NZB_DECOMPRESSED_BYTES {
        anyhow::bail!(
            "Decompressed NZB exceeds the {} MB limit",
            MAX_NZB_DECOMPRESSED_BYTES / 1024 / 1024
        );
    }
    Ok(decompressed)
}

#[cfg(test)]
mod tests {
    use super::{MAX_NZB_DECOMPRESSED_BYTES, extract_nzbs};
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

    #[test]
    fn extract_nzbs_rejects_zip_bombs() {
        let oversized = vec![b'x'; (MAX_NZB_DECOMPRESSED_BYTES + 1) as usize];
        let zip = build_zip(&[("oversized.nzb", oversized.as_slice())]);
        let err = extract_nzbs("oversized.zip", &zip).unwrap_err();
        assert!(err.to_string().contains("100 MB limit"));
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
