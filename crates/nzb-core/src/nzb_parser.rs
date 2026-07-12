use std::path::Path;

use quick_xml::events::Event;
use quick_xml::reader::Reader;
use tracing::warn;
use unicode_normalization::UnicodeNormalization;

use crate::error::NzbError;
use crate::models::{Article, JobStatus, NzbFile, NzbJob, Priority};

/// Parse an NZB XML file into an NzbJob.
pub fn parse_nzb(name: &str, data: &[u8]) -> Result<NzbJob, NzbError> {
    if data.is_empty() {
        return Err(NzbError::InvalidNzb("NZB data is empty".into()));
    }

    // Detect obviously non-NZB content so the error is actionable rather than
    // "No files found in NZB". Gzip magic, JSON, and HTML are the three most
    // common things an indexer returns when the NZB is unavailable.
    if data.starts_with(&[0x1f, 0x8b]) {
        return Err(NzbError::InvalidNzb(
            "NZB data appears to be gzip-compressed; decompress before parsing".into(),
        ));
    }
    let prefix = &data[..data.len().min(512)];
    let prefix_str = String::from_utf8_lossy(prefix);
    let prefix_lower = prefix_str.to_ascii_lowercase();
    if prefix_lower.trim_start().starts_with('{') || prefix_lower.trim_start().starts_with('[') {
        return Err(NzbError::InvalidNzb(
            "NZB data appears to be a JSON response rather than an NZB XML file".into(),
        ));
    }
    if prefix_lower.contains("<html") || prefix_lower.contains("<!doctype html") {
        return Err(NzbError::InvalidNzb(
            "NZB data appears to be an HTML error page rather than an NZB XML file".into(),
        ));
    }

    let mut reader = Reader::from_reader(data);
    reader.config_mut().trim_text(true);
    let decoder = reader.decoder();

    let mut files: Vec<NzbFile> = Vec::new();
    let mut current_file: Option<FileBuilder> = None;
    let mut current_groups: Vec<String> = Vec::new();
    let mut current_segments: Vec<SegmentBuilder> = Vec::new();
    let mut in_groups = false;
    let mut in_segments = false;
    let mut buf = Vec::new();
    let mut meta_password: Option<String> = None;
    let mut reading_password_meta = false;

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) => match e.local_name().as_ref() {
                b"file" => {
                    let mut subject = String::new();
                    let mut date = 0i64;
                    for attr in e.attributes().flatten() {
                        match attr.key.as_ref() {
                            b"subject" => {
                                subject = attr
                                    .decode_and_unescape_value(decoder)
                                    .map(|v| v.into_owned())
                                    .unwrap_or_else(|_| {
                                        String::from_utf8_lossy(&attr.value).into_owned()
                                    });
                            }
                            b"date" => {
                                date = String::from_utf8_lossy(&attr.value).parse().unwrap_or(0);
                            }
                            _ => {}
                        }
                    }
                    current_file = Some(FileBuilder {
                        subject,
                        _date: date,
                    });
                    current_groups.clear();
                    current_segments.clear();
                }
                b"groups" => in_groups = true,
                b"group" => {}
                b"segments" => in_segments = true,
                b"segment" => {
                    let mut number = 0u32;
                    let mut bytes = 0u64;
                    for attr in e.attributes().flatten() {
                        match attr.key.as_ref() {
                            b"number" => {
                                number = String::from_utf8_lossy(&attr.value).parse().unwrap_or(0);
                            }
                            b"bytes" => {
                                bytes = String::from_utf8_lossy(&attr.value).parse().unwrap_or(0);
                            }
                            _ => {}
                        }
                    }
                    current_segments.push(SegmentBuilder {
                        number,
                        bytes,
                        message_id: String::new(),
                    });
                }
                b"meta" => {
                    for attr in e.attributes().flatten() {
                        if attr.key.as_ref() == b"type" && attr.value.as_ref() == b"password" {
                            reading_password_meta = true;
                        }
                    }
                }
                _ => {}
            },
            Ok(Event::End(ref e)) => match e.local_name().as_ref() {
                b"file" => {
                    if let Some(fb) = current_file.take() {
                        let filename = sanitize_filename(&extract_filename(&fb.subject));

                        // Filter out invalid segments: number=0, bytes=0, or
                        // empty message-ID are malformed and would cause
                        // zero-byte files or assembly failures downstream.
                        let valid_count = current_segments.len();
                        current_segments
                            .retain(|s| s.number > 0 && s.bytes > 0 && !s.message_id.is_empty());
                        let dropped = valid_count - current_segments.len();
                        if dropped > 0 {
                            warn!(
                                filename = %filename,
                                dropped,
                                "Dropped {dropped} invalid segment(s) (zero bytes, zero number, or empty message-ID)"
                            );
                        }

                        let total_bytes: u64 = current_segments.iter().map(|s| s.bytes).sum();
                        let articles: Vec<Article> = current_segments
                            .drain(..)
                            .map(|s| Article {
                                message_id: s.message_id,
                                segment_number: s.number,
                                bytes: s.bytes,
                                downloaded: false,
                                data_begin: None,
                                data_size: None,
                                crc32: None,
                                tried_servers: Vec::new(),
                                tries: 0,
                            })
                            .collect();

                        let is_par2 = filename.to_lowercase().ends_with(".par2");
                        let (par2_setname, par2_vol, par2_blocks) = if is_par2 {
                            parse_par2_filename(&filename)
                        } else {
                            (None, None, None)
                        };

                        files.push(NzbFile {
                            id: uuid::Uuid::new_v4().to_string(),
                            filename,
                            bytes: total_bytes,
                            bytes_downloaded: 0,
                            is_par2,
                            par2_setname,
                            par2_vol,
                            par2_blocks,
                            assembled: false,
                            groups: current_groups.clone(),
                            articles,
                        });
                    }
                }
                b"groups" => in_groups = false,
                b"segments" => in_segments = false,
                _ => {}
            },
            Ok(Event::Text(ref t)) => {
                let text = t.unescape().unwrap_or_default().into_owned();
                if reading_password_meta {
                    meta_password = Some(text);
                    reading_password_meta = false;
                } else if in_groups {
                    current_groups.push(text);
                } else if in_segments && let Some(seg) = current_segments.last_mut() {
                    seg.message_id = text;
                }
            }
            // Some NZB generators wrap message-IDs in CDATA sections.
            Ok(Event::CData(ref c)) => {
                if in_segments && let Some(seg) = current_segments.last_mut() {
                    seg.message_id = String::from_utf8_lossy(c.as_ref()).into_owned();
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(NzbError::ParseError(format!("XML error: {e}"))),
            _ => {}
        }
        buf.clear();
    }

    if files.is_empty() {
        // Log a snippet of the raw data to help diagnose unexpected content.
        let snippet = String::from_utf8_lossy(&data[..data.len().min(256)]);
        warn!(
            %name,
            first_bytes = %snippet,
            "No <file> elements found in NZB; possible format mismatch or empty/error response"
        );
        return Err(NzbError::InvalidNzb("No files found in NZB".into()));
    }

    let total_bytes: u64 = files.iter().map(|f| f.bytes).sum();
    let article_count: usize = files.iter().map(|f| f.articles.len()).sum();
    let file_count = files.len();

    Ok(NzbJob {
        id: uuid::Uuid::new_v4().to_string(),
        name: name.nfc().collect(),
        category: "Default".into(),
        status: JobStatus::Queued,
        priority: Priority::Normal,
        total_bytes,
        downloaded_bytes: 0,
        file_count,
        files_completed: 0,
        article_count,
        articles_downloaded: 0,
        articles_failed: 0,
        added_at: chrono::Utc::now(),
        completed_at: None,
        work_dir: std::path::PathBuf::new(), // Set by queue manager
        output_dir: std::path::PathBuf::new(),
        password: meta_password,
        error_message: None,
        speed_bps: 0,
        server_stats: Vec::new(),
        files,
    })
}

/// Parse NZB from a file path.
pub fn parse_nzb_file(path: &Path) -> Result<NzbJob, NzbError> {
    let data = std::fs::read(path)?;
    let name = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "Unknown".into());
    parse_nzb(&name, &data)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

struct FileBuilder {
    subject: String,
    _date: i64,
}

struct SegmentBuilder {
    number: u32,
    bytes: u64,
    message_id: String,
}

/// Sanitize a filename by removing/replacing characters that are unsafe on
/// common filesystems (Windows NTFS, Linux ext4, macOS HFS+).
fn sanitize_filename(name: &str) -> String {
    // Normalize to NFC first — filenames from NZB subjects may arrive in
    // decomposed (NFD) form, causing duplicate files or lookup failures
    // across platforms (macOS HFS+/APFS uses NFD internally).
    let normalized: String = name.nfc().collect();
    let mut out = String::with_capacity(normalized.len());
    for ch in normalized.chars() {
        match ch {
            // Illegal on Windows / problematic everywhere
            '<' | '>' | ':' | '"' | '|' | '?' | '*' => {}
            // Control characters (including null)
            c if c.is_control() => {}
            // Backslash → forward slash is also risky; strip it
            '\\' => {}
            _ => out.push(ch),
        }
    }
    // Trim leading/trailing whitespace and dots (Windows rejects trailing dots)
    let trimmed = out.trim().trim_end_matches('.');
    if trimmed.is_empty() {
        // Extremely unlikely, but don't return an empty filename
        return "unnamed".to_string();
    }
    trimmed.to_string()
}

/// Extract a filename from an NZB subject line.
/// Common pattern: `"Some Post" filename.ext (01/10)`
fn extract_filename(subject: &str) -> String {
    // Try to find quoted filename first
    if let Some(start) = subject.find('"')
        && let Some(end) = subject[start + 1..].find('"')
    {
        return subject[start + 1..start + 1 + end].to_string();
    }

    // Try to find filename before (xx/yy) pattern
    if let Some(paren_pos) = subject.rfind('(') {
        let before_paren = subject[..paren_pos].trim();
        // Take the last space-separated token as filename
        if let Some(last_space) = before_paren.rfind(' ') {
            let candidate = &before_paren[last_space + 1..];
            if candidate.contains('.') {
                return candidate.to_string();
            }
        }
        if before_paren.contains('.') {
            return before_paren.to_string();
        }
    }

    subject.to_string()
}

/// Parse par2 filename for volume/block info.
/// Pattern: `setname.vol00+01.par2`
pub fn parse_par2_filename(filename: &str) -> (Option<String>, Option<u32>, Option<u32>) {
    let lower = filename.to_lowercase();
    if !lower.ends_with(".par2") {
        return (None, None, None);
    }

    let without_ext = &filename[..filename.len() - 5];

    // Check for .volNN+NN pattern
    if let Some(vol_pos) = without_ext.to_lowercase().rfind(".vol") {
        let setname = without_ext[..vol_pos].to_string();
        let vol_part = &without_ext[vol_pos + 4..];

        if let Some(plus_pos) = vol_part.find('+') {
            let vol: u32 = vol_part[..plus_pos].parse().unwrap_or(0);
            let blocks: u32 = vol_part[plus_pos + 1..].parse().unwrap_or(0);
            return (Some(setname), Some(vol), Some(blocks));
        }
    }

    // No volume info — this is the index par2
    let setname = without_ext.to_string();
    (Some(setname), None, None)
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // extract_filename tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_extract_filename_quoted() {
        let subject = r#"Some Poster "myfile.rar" (01/10)"#;
        assert_eq!(extract_filename(subject), "myfile.rar");
    }

    #[test]
    fn test_extract_filename_quoted_with_spaces() {
        let subject = r#"Poster "My File Name.part01.rar" (01/10)"#;
        assert_eq!(extract_filename(subject), "My File Name.part01.rar");
    }

    #[test]
    fn test_extract_filename_before_parens() {
        let subject = "Some.Movie.2024.720p.BluRay.x264-GROUP file.rar (01/50)";
        assert_eq!(extract_filename(subject), "file.rar");
    }

    #[test]
    fn test_extract_filename_dotted_name_before_parens() {
        let subject = "movie.mkv (1/1)";
        assert_eq!(extract_filename(subject), "movie.mkv");
    }

    #[test]
    fn test_extract_filename_no_pattern() {
        // No quotes, no parens — returns whole subject
        let subject = "just some text without pattern";
        assert_eq!(extract_filename(subject), "just some text without pattern");
    }

    #[test]
    fn test_extract_filename_obfuscated_hash() {
        // Common obfuscated pattern — no quotes, hash before parens, no dot
        // Falls through all patterns → returns full subject
        let subject = "a8f3c72d1e4b5689 (1/50)";
        assert_eq!(extract_filename(subject), "a8f3c72d1e4b5689 (1/50)");
    }

    // -----------------------------------------------------------------------
    // parse_par2_filename tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_parse_par2_with_volume() {
        let (set, vol, blocks) = parse_par2_filename("MyDownload.vol00+01.par2");
        assert_eq!(set, Some("MyDownload".into()));
        assert_eq!(vol, Some(0));
        assert_eq!(blocks, Some(1));
    }

    #[test]
    fn test_parse_par2_with_large_volume() {
        let (set, vol, blocks) = parse_par2_filename("data.vol15+16.par2");
        assert_eq!(set, Some("data".into()));
        assert_eq!(vol, Some(15));
        assert_eq!(blocks, Some(16));
    }

    #[test]
    fn test_parse_par2_index() {
        let (set, vol, blocks) = parse_par2_filename("MyDownload.par2");
        assert_eq!(set, Some("MyDownload".into()));
        assert_eq!(vol, None);
        assert_eq!(blocks, None);
    }

    #[test]
    fn test_parse_par2_not_par2() {
        let (set, vol, blocks) = parse_par2_filename("myfile.rar");
        assert_eq!(set, None);
        assert_eq!(vol, None);
        assert_eq!(blocks, None);
    }

    #[test]
    fn test_parse_par2_case_insensitive() {
        let (set, _, _) = parse_par2_filename("MyFile.PAR2");
        assert_eq!(set, Some("MyFile".into()));

        let (set2, vol, blocks) = parse_par2_filename("MyFile.Vol00+01.PAR2");
        assert_eq!(set2, Some("MyFile".into()));
        assert_eq!(vol, Some(0));
        assert_eq!(blocks, Some(1));
    }

    // -----------------------------------------------------------------------
    // parse_nzb tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_parse_nzb_basic() {
        let nzb_data = br#"<?xml version="1.0" encoding="UTF-8"?>
<nzb xmlns="http://www.newzbin.com/DTD/2003/nzb">
  <file poster="test@example.com" date="1234567890" subject="test.rar (1/2)">
    <groups><group>alt.binaries.test</group></groups>
    <segments>
      <segment number="1" bytes="768000">article1@example.com</segment>
      <segment number="2" bytes="768000">article2@example.com</segment>
    </segments>
  </file>
</nzb>"#;

        let job = parse_nzb("test", nzb_data).unwrap();
        assert_eq!(job.name, "test");
        assert_eq!(job.file_count, 1);
        assert_eq!(job.article_count, 2);
        assert_eq!(job.total_bytes, 1536000);
        assert_eq!(job.files[0].articles[0].message_id, "article1@example.com");
    }

    #[test]
    fn test_parse_nzb_multiple_files() {
        let nzb_data = br#"<?xml version="1.0" encoding="UTF-8"?>
<nzb xmlns="http://www.newzbin.com/DTD/2003/nzb">
  <file poster="p@x.com" date="100" subject="Some Post file1.rar (1/2)">
    <groups><group>alt.test</group></groups>
    <segments>
      <segment number="1" bytes="500000">seg1@x</segment>
    </segments>
  </file>
  <file poster="p@x.com" date="100" subject="Some Post file2.rar (2/2)">
    <groups><group>alt.test</group></groups>
    <segments>
      <segment number="1" bytes="300000">seg2@x</segment>
    </segments>
  </file>
</nzb>"#;

        let job = parse_nzb("multi", nzb_data).unwrap();
        assert_eq!(job.file_count, 2);
        assert_eq!(job.article_count, 2);
        assert_eq!(job.total_bytes, 800000);
        assert_eq!(job.files[0].filename, "file1.rar");
        assert_eq!(job.files[1].filename, "file2.rar");
    }

    #[test]
    fn test_parse_nzb_multiple_groups() {
        let nzb_data = br#"<?xml version="1.0" encoding="UTF-8"?>
<nzb xmlns="http://www.newzbin.com/DTD/2003/nzb">
  <file poster="p@x.com" date="100" subject="test.rar (1/1)">
    <groups>
      <group>alt.binaries.test</group>
      <group>alt.binaries.misc</group>
    </groups>
    <segments>
      <segment number="1" bytes="100000">seg@x</segment>
    </segments>
  </file>
</nzb>"#;

        let job = parse_nzb("groups", nzb_data).unwrap();
        assert_eq!(job.files[0].groups.len(), 2);
        assert_eq!(job.files[0].groups[0], "alt.binaries.test");
        assert_eq!(job.files[0].groups[1], "alt.binaries.misc");
    }

    #[test]
    fn test_parse_nzb_with_password() {
        let nzb_data = br#"<?xml version="1.0" encoding="UTF-8"?>
<nzb xmlns="http://www.newzbin.com/DTD/2003/nzb">
  <head>
    <meta type="password">secret123</meta>
  </head>
  <file poster="p@x.com" date="100" subject="test.rar (1/1)">
    <groups><group>alt.test</group></groups>
    <segments>
      <segment number="1" bytes="100">seg@x</segment>
    </segments>
  </file>
</nzb>"#;

        let job = parse_nzb("password", nzb_data).unwrap();
        assert_eq!(job.password.as_deref(), Some("secret123"));
    }

    #[test]
    fn test_parse_nzb_par2_detection() {
        let nzb_data = br#"<?xml version="1.0" encoding="UTF-8"?>
<nzb xmlns="http://www.newzbin.com/DTD/2003/nzb">
  <file poster="p@x.com" date="100" subject="Post data.par2 (1/1)">
    <groups><group>alt.test</group></groups>
    <segments>
      <segment number="1" bytes="1000">s1@x</segment>
    </segments>
  </file>
  <file poster="p@x.com" date="100" subject="Post data.vol00+01.par2 (1/1)">
    <groups><group>alt.test</group></groups>
    <segments>
      <segment number="1" bytes="2000">s2@x</segment>
    </segments>
  </file>
  <file poster="p@x.com" date="100" subject="Post data.rar (1/1)">
    <groups><group>alt.test</group></groups>
    <segments>
      <segment number="1" bytes="3000">s3@x</segment>
    </segments>
  </file>
</nzb>"#;

        let job = parse_nzb("par2", nzb_data).unwrap();
        assert_eq!(job.file_count, 3);

        // Index par2
        assert!(job.files[0].is_par2);
        assert_eq!(job.files[0].par2_setname.as_deref(), Some("data"));
        assert!(job.files[0].par2_vol.is_none());

        // Volume par2
        assert!(job.files[1].is_par2);
        assert_eq!(job.files[1].par2_setname.as_deref(), Some("data"));
        assert_eq!(job.files[1].par2_vol, Some(0));
        assert_eq!(job.files[1].par2_blocks, Some(1));

        // Not par2
        assert!(!job.files[2].is_par2);
        assert!(job.files[2].par2_setname.is_none());
    }

    #[test]
    fn test_parse_nzb_empty_returns_error() {
        let nzb_data = br#"<?xml version="1.0" encoding="UTF-8"?>
<nzb xmlns="http://www.newzbin.com/DTD/2003/nzb">
</nzb>"#;

        let result = parse_nzb("empty", nzb_data);
        assert!(result.is_err());
        match result {
            Err(NzbError::InvalidNzb(msg)) => assert!(msg.contains("No files")),
            _ => panic!("Expected InvalidNzb error"),
        }
    }

    #[test]
    fn test_parse_nzb_invalid_xml() {
        let result = parse_nzb("bad", b"this is not xml at all <<<<");
        // Should either parse error or return InvalidNzb (no files)
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_nzb_segment_ordering() {
        let nzb_data = br#"<?xml version="1.0" encoding="UTF-8"?>
<nzb xmlns="http://www.newzbin.com/DTD/2003/nzb">
  <file poster="p@x.com" date="100" subject="test.rar (1/1)">
    <groups><group>alt.test</group></groups>
    <segments>
      <segment number="3" bytes="100">seg3@x</segment>
      <segment number="1" bytes="100">seg1@x</segment>
      <segment number="2" bytes="100">seg2@x</segment>
    </segments>
  </file>
</nzb>"#;

        let job = parse_nzb("order", nzb_data).unwrap();
        let articles = &job.files[0].articles;
        assert_eq!(articles.len(), 3);
        // Segments should be in the order they appear in the XML
        assert_eq!(articles[0].segment_number, 3);
        assert_eq!(articles[1].segment_number, 1);
        assert_eq!(articles[2].segment_number, 2);
    }

    #[test]
    fn test_parse_nzb_article_initial_state() {
        let nzb_data = br#"<?xml version="1.0" encoding="UTF-8"?>
<nzb xmlns="http://www.newzbin.com/DTD/2003/nzb">
  <file poster="p@x.com" date="100" subject="test.rar (1/1)">
    <groups><group>alt.test</group></groups>
    <segments>
      <segment number="1" bytes="50000">art@x</segment>
    </segments>
  </file>
</nzb>"#;

        let job = parse_nzb("state", nzb_data).unwrap();
        let article = &job.files[0].articles[0];
        assert_eq!(article.message_id, "art@x");
        assert_eq!(article.segment_number, 1);
        assert_eq!(article.bytes, 50000);
        assert!(!article.downloaded);
        assert!(article.data_begin.is_none());
        assert!(article.data_size.is_none());
        assert!(article.crc32.is_none());
        assert!(article.tried_servers.is_empty());
        assert_eq!(article.tries, 0);
    }

    #[test]
    fn test_parse_nzb_job_initial_state() {
        let nzb_data = br#"<?xml version="1.0" encoding="UTF-8"?>
<nzb xmlns="http://www.newzbin.com/DTD/2003/nzb">
  <file poster="p@x.com" date="100" subject="test.rar (1/1)">
    <groups><group>alt.test</group></groups>
    <segments>
      <segment number="1" bytes="100">s@x</segment>
    </segments>
  </file>
</nzb>"#;

        let job = parse_nzb("init", nzb_data).unwrap();
        assert_eq!(job.status, JobStatus::Queued);
        assert_eq!(job.priority, Priority::Normal);
        assert_eq!(job.downloaded_bytes, 0);
        assert_eq!(job.files_completed, 0);
        assert_eq!(job.articles_downloaded, 0);
        assert_eq!(job.articles_failed, 0);
        assert!(job.completed_at.is_none());
        assert!(job.password.is_none());
        assert!(job.error_message.is_none());
        assert_eq!(job.category, "Default");
    }

    // -----------------------------------------------------------------------
    // sanitize_filename tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_sanitize_removes_unsafe_chars() {
        assert_eq!(sanitize_filename(r#"file<name>.rar"#), "filename.rar");
        assert_eq!(sanitize_filename(r#""myfile.mkv""#), "myfile.mkv");
        assert_eq!(sanitize_filename("file:name.rar"), "filename.rar");
        assert_eq!(sanitize_filename("file|name?.rar"), "filename.rar");
    }

    #[test]
    fn test_sanitize_preserves_normal_names() {
        assert_eq!(sanitize_filename("movie.part01.rar"), "movie.part01.rar");
        assert_eq!(
            sanitize_filename("My.Movie.2024.1080p.WEB-DL.mkv"),
            "My.Movie.2024.1080p.WEB-DL.mkv"
        );
    }

    #[test]
    fn test_sanitize_trims_whitespace_and_dots() {
        assert_eq!(sanitize_filename("  file.rar  "), "file.rar");
        assert_eq!(sanitize_filename("file.rar..."), "file.rar");
    }

    #[test]
    fn test_sanitize_empty_returns_unnamed() {
        assert_eq!(sanitize_filename(""), "unnamed");
        assert_eq!(sanitize_filename("<>:"), "unnamed");
    }

    // -----------------------------------------------------------------------
    // XML entity unescaping in subject attributes
    // -----------------------------------------------------------------------

    #[test]
    fn test_parse_nzb_unescapes_xml_entities_in_subject() {
        // NZB subjects commonly contain &#34; (numeric entity for ")
        // and &lt;/&gt; for angle brackets. The parser must unescape
        // these so extract_filename can find the quoted filename.
        let nzb_data = br#"<?xml version="1.0" encoding="UTF-8"?>
<nzb xmlns="http://www.newzbin.com/DTD/2003/nzb">
  <file poster="p@x.com" date="100" subject="&lt; Release.Name &gt; - &#34;Release.Name.part01.rar&#34; yEnc (01/10)">
    <groups><group>alt.test</group></groups>
    <segments>
      <segment number="1" bytes="100">seg@x</segment>
    </segments>
  </file>
</nzb>"#;

        let job = parse_nzb("entity_test", nzb_data).unwrap();
        // After unescaping &#34; → ", extract_filename should find the
        // quoted name "Release.Name.part01.rar"
        assert_eq!(job.files[0].filename, "Release.Name.part01.rar");
    }

    #[test]
    fn test_parse_nzb_unescapes_amp_entities() {
        let nzb_data = br#"<?xml version="1.0" encoding="UTF-8"?>
<nzb xmlns="http://www.newzbin.com/DTD/2003/nzb">
  <file poster="p@x.com" date="100" subject="His &amp; Hers S01E01 &#34;His.and.Hers.S01E01.mkv&#34; (1/1)">
    <groups><group>alt.test</group></groups>
    <segments>
      <segment number="1" bytes="100">seg@x</segment>
    </segments>
  </file>
</nzb>"#;

        let job = parse_nzb("amp_test", nzb_data).unwrap();
        assert_eq!(job.files[0].filename, "His.and.Hers.S01E01.mkv");
    }

    // -----------------------------------------------------------------------
    // Unicode NFC normalization tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_sanitize_normalizes_nfd_to_nfc() {
        // "café.rar" in NFD: 'e' + combining acute accent (U+0301)
        let nfd = "caf\u{0065}\u{0301}.rar";
        // Should produce NFC: precomposed 'é' (U+00E9)
        assert_eq!(sanitize_filename(nfd), "caf\u{00E9}.rar");
    }

    #[test]
    fn test_parse_nzb_normalizes_job_name() {
        let nzb_data = br#"<?xml version="1.0" encoding="UTF-8"?>
<nzb xmlns="http://www.newzbin.com/DTD/2003/nzb">
  <file poster="p@x.com" date="100" subject="test.rar (1/1)">
    <groups><group>alt.test</group></groups>
    <segments>
      <segment number="1" bytes="100">s@x</segment>
    </segments>
  </file>
</nzb>"#;

        // Pass NFD name: "café" decomposed
        let job = parse_nzb("caf\u{0065}\u{0301}", nzb_data).unwrap();
        // Job name should be NFC
        assert_eq!(job.name, "caf\u{00E9}");
    }

    #[test]
    fn test_extract_filename_with_angle_brackets() {
        // After XML unescaping, subjects look like:
        // < Release > - "filename.rar" yEnc (01/10)
        let subject = r#"< Mayday.S26E10 > - "Mayday.S26E10.part01.rar" yEnc (01/57)"#;
        assert_eq!(extract_filename(subject), "Mayday.S26E10.part01.rar");
    }

    // -----------------------------------------------------------------------
    // Empty / zero-byte segment handling tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_parse_nzb_drops_zero_byte_segments() {
        let nzb_data = br#"<?xml version="1.0" encoding="UTF-8"?>
<nzb xmlns="http://www.newzbin.com/DTD/2003/nzb">
  <file poster="p@x.com" date="100" subject="test.rar (1/1)">
    <groups><group>alt.test</group></groups>
    <segments>
      <segment number="1" bytes="500000">good@x</segment>
      <segment number="2" bytes="0">zerobytes@x</segment>
      <segment number="3" bytes="300000">also-good@x</segment>
    </segments>
  </file>
</nzb>"#;

        let job = parse_nzb("zero_bytes", nzb_data).unwrap();
        assert_eq!(
            job.files[0].articles.len(),
            2,
            "Zero-byte segment should be dropped"
        );
        assert_eq!(job.files[0].articles[0].message_id, "good@x");
        assert_eq!(job.files[0].articles[1].message_id, "also-good@x");
        assert_eq!(
            job.total_bytes, 800000,
            "Total bytes should exclude zero-byte segment"
        );
    }

    #[test]
    fn test_parse_nzb_drops_zero_number_segments() {
        let nzb_data = br#"<?xml version="1.0" encoding="UTF-8"?>
<nzb xmlns="http://www.newzbin.com/DTD/2003/nzb">
  <file poster="p@x.com" date="100" subject="test.rar (1/1)">
    <groups><group>alt.test</group></groups>
    <segments>
      <segment number="0" bytes="500000">bad-number@x</segment>
      <segment number="1" bytes="500000">good@x</segment>
    </segments>
  </file>
</nzb>"#;

        let job = parse_nzb("zero_num", nzb_data).unwrap();
        assert_eq!(
            job.files[0].articles.len(),
            1,
            "Zero-number segment should be dropped"
        );
        assert_eq!(job.files[0].articles[0].message_id, "good@x");
    }

    #[test]
    fn test_parse_nzb_drops_empty_message_id_segments() {
        let nzb_data = br#"<?xml version="1.0" encoding="UTF-8"?>
<nzb xmlns="http://www.newzbin.com/DTD/2003/nzb">
  <file poster="p@x.com" date="100" subject="test.rar (1/1)">
    <groups><group>alt.test</group></groups>
    <segments>
      <segment number="1" bytes="500000"></segment>
      <segment number="2" bytes="500000">good@x</segment>
    </segments>
  </file>
</nzb>"#;

        let job = parse_nzb("empty_msgid", nzb_data).unwrap();
        assert_eq!(
            job.files[0].articles.len(),
            1,
            "Empty message-ID segment should be dropped"
        );
        assert_eq!(job.files[0].articles[0].message_id, "good@x");
    }

    #[test]
    fn test_parse_nzb_all_segments_invalid_creates_empty_file() {
        // All segments are invalid — file should still be created but with
        // zero articles and zero bytes. This matches NZB spec (file exists
        // but has no downloadable content).
        let nzb_data = br#"<?xml version="1.0" encoding="UTF-8"?>
<nzb xmlns="http://www.newzbin.com/DTD/2003/nzb">
  <file poster="p@x.com" date="100" subject="test.rar (1/1)">
    <groups><group>alt.test</group></groups>
    <segments>
      <segment number="0" bytes="0"></segment>
      <segment number="0" bytes="500000">bad@x</segment>
    </segments>
  </file>
</nzb>"#;

        let job = parse_nzb("all_invalid", nzb_data).unwrap();
        assert_eq!(job.files[0].articles.len(), 0);
        assert_eq!(job.files[0].bytes, 0);
        assert_eq!(job.total_bytes, 0);
        assert_eq!(job.article_count, 0);
    }

    #[test]
    fn test_parse_nzb_mixed_valid_invalid_segments_across_files() {
        let nzb_data = br#"<?xml version="1.0" encoding="UTF-8"?>
<nzb xmlns="http://www.newzbin.com/DTD/2003/nzb">
  <file poster="p@x.com" date="100" subject="file1.rar (1/2)">
    <groups><group>alt.test</group></groups>
    <segments>
      <segment number="1" bytes="100">s1@x</segment>
      <segment number="2" bytes="0">bad@x</segment>
    </segments>
  </file>
  <file poster="p@x.com" date="100" subject="file2.rar (2/2)">
    <groups><group>alt.test</group></groups>
    <segments>
      <segment number="1" bytes="200">s2@x</segment>
      <segment number="2" bytes="300">s3@x</segment>
    </segments>
  </file>
</nzb>"#;

        let job = parse_nzb("mixed", nzb_data).unwrap();
        assert_eq!(job.file_count, 2);
        assert_eq!(
            job.files[0].articles.len(),
            1,
            "file1 should have 1 valid segment"
        );
        assert_eq!(job.files[0].bytes, 100);
        assert_eq!(
            job.files[1].articles.len(),
            2,
            "file2 should have 2 valid segments"
        );
        assert_eq!(job.files[1].bytes, 500);
        assert_eq!(job.total_bytes, 600);
        assert_eq!(job.article_count, 3);
    }

    #[test]
    fn test_parse_nzb_negative_bytes_parsed_as_zero() {
        // Negative values in unsigned parse → unwrap_or(0) → dropped
        let nzb_data = br#"<?xml version="1.0" encoding="UTF-8"?>
<nzb xmlns="http://www.newzbin.com/DTD/2003/nzb">
  <file poster="p@x.com" date="100" subject="test.rar (1/1)">
    <groups><group>alt.test</group></groups>
    <segments>
      <segment number="1" bytes="-1">neg@x</segment>
      <segment number="2" bytes="500000">good@x</segment>
    </segments>
  </file>
</nzb>"#;

        let job = parse_nzb("neg_bytes", nzb_data).unwrap();
        // "-1" fails u64 parse → unwrap_or(0) → dropped by bytes > 0 filter
        assert_eq!(job.files[0].articles.len(), 1);
        assert_eq!(job.files[0].articles[0].message_id, "good@x");
    }

    #[test]
    fn test_parse_nzb_missing_bytes_attr_treated_as_zero() {
        // Missing bytes attribute → unwrap_or(0) → dropped
        let nzb_data = br#"<?xml version="1.0" encoding="UTF-8"?>
<nzb xmlns="http://www.newzbin.com/DTD/2003/nzb">
  <file poster="p@x.com" date="100" subject="test.rar (1/1)">
    <groups><group>alt.test</group></groups>
    <segments>
      <segment number="1">no-bytes@x</segment>
      <segment number="2" bytes="500000">good@x</segment>
    </segments>
  </file>
</nzb>"#;

        let job = parse_nzb("no_bytes_attr", nzb_data).unwrap();
        assert_eq!(job.files[0].articles.len(), 1);
        assert_eq!(job.files[0].articles[0].message_id, "good@x");
    }

    // -----------------------------------------------------------------------
    // Namespace-prefix handling (local_name fix)
    // -----------------------------------------------------------------------

    #[test]
    fn test_parse_nzb_namespace_prefixed_elements() {
        // Some indexers/generators use namespace-prefixed elements like <nzb:file>.
        // local_name() strips the prefix, so this must parse correctly.
        let nzb_data = br#"<?xml version="1.0" encoding="UTF-8"?>
<nzb:nzb xmlns:nzb="http://www.newzbin.com/DTD/2003/nzb">
  <nzb:file poster="p@x.com" date="100" subject="test.rar (1/1)">
    <nzb:groups><nzb:group>alt.test</nzb:group></nzb:groups>
    <nzb:segments>
      <nzb:segment number="1" bytes="500000">ns-seg@x</nzb:segment>
    </nzb:segments>
  </nzb:file>
</nzb:nzb>"#;

        let job = parse_nzb("ns_test", nzb_data).unwrap();
        assert_eq!(job.file_count, 1);
        assert_eq!(job.files[0].articles.len(), 1);
        assert_eq!(job.files[0].articles[0].message_id, "ns-seg@x");
        assert_eq!(job.files[0].groups, vec!["alt.test"]);
    }

    // -----------------------------------------------------------------------
    // CDATA segment message-ID handling
    // -----------------------------------------------------------------------

    #[test]
    fn test_parse_nzb_cdata_segment_message_id() {
        // Message-IDs wrapped in CDATA sections must be extracted correctly.
        let nzb_data = br#"<?xml version="1.0" encoding="UTF-8"?>
<nzb xmlns="http://www.newzbin.com/DTD/2003/nzb">
  <file poster="p@x.com" date="100" subject="test.rar (1/1)">
    <groups><group>alt.test</group></groups>
    <segments>
      <segment number="1" bytes="500000"><![CDATA[cdata-msgid@example.com]]></segment>
      <segment number="2" bytes="300000">plain-msgid@example.com</segment>
    </segments>
  </file>
</nzb>"#;

        let job = parse_nzb("cdata_test", nzb_data).unwrap();
        assert_eq!(job.files[0].articles.len(), 2);
        assert_eq!(
            job.files[0].articles[0].message_id,
            "cdata-msgid@example.com"
        );
        assert_eq!(
            job.files[0].articles[1].message_id,
            "plain-msgid@example.com"
        );
    }

    // -----------------------------------------------------------------------
    // Early detection of non-NZB content
    // -----------------------------------------------------------------------

    #[test]
    fn test_parse_nzb_empty_data_returns_error() {
        let result = parse_nzb("empty", b"");
        assert!(result.is_err());
        match result {
            Err(NzbError::InvalidNzb(msg)) => assert!(msg.contains("empty")),
            _ => panic!("Expected InvalidNzb error for empty data"),
        }
    }

    #[test]
    fn test_parse_nzb_gzip_data_returns_error() {
        // Gzip magic bytes — should be detected before XML parsing.
        let result = parse_nzb("gzip", &[0x1f, 0x8b, 0x08, 0x00]);
        assert!(result.is_err());
        match result {
            Err(NzbError::InvalidNzb(msg)) => assert!(msg.contains("gzip")),
            _ => panic!("Expected InvalidNzb error for gzip data"),
        }
    }

    #[test]
    fn test_parse_nzb_json_data_returns_error() {
        let result = parse_nzb("json", b"{\"error\": \"not found\"}");
        assert!(result.is_err());
        match result {
            Err(NzbError::InvalidNzb(msg)) => assert!(msg.contains("JSON")),
            _ => panic!("Expected InvalidNzb error for JSON data"),
        }
    }

    #[test]
    fn test_parse_nzb_html_data_returns_error() {
        let result = parse_nzb(
            "html",
            b"<!DOCTYPE html><html><body>NZB not found</body></html>",
        );
        assert!(result.is_err());
        match result {
            Err(NzbError::InvalidNzb(msg)) => assert!(msg.contains("HTML")),
            _ => panic!("Expected InvalidNzb error for HTML data"),
        }
    }
}
