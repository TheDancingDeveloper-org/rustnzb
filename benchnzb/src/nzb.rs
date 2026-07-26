use crate::config::{ARTICLE_SIZE, MSG_ID_DOMAIN, NNTP_GROUP};

pub struct NzbFile {
    pub filename: String,
    pub segments: Vec<NzbSegment>,
}

pub struct NzbSegment {
    pub message_id: String,
    pub bytes: u64,
    pub number: u32,
}

/// Generate NZB XML from a list of files with their segments.
pub fn generate_nzb(files: &[NzbFile], poster: &str) -> String {
    let mut xml = String::new();
    xml.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    xml.push_str("<!DOCTYPE nzb PUBLIC \"-//newzBin//DTD NZB 1.1//EN\" \"http://www.newzbin.com/DTD/nzb/nzb-1.1.dtd\">\n");
    xml.push_str("<nzb xmlns=\"http://www.newzbin.com/DTD/2003/nzb\">\n");

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();

    for file in files {
        let total_segs = file.segments.len();
        let subject = format!(
            "{} (1/{total_segs}) - \"{}\" yEnc (1/{total_segs})",
            file.filename, file.filename
        );
        xml.push_str(&format!(
            "  <file poster=\"{}\" date=\"{now}\" subject=\"{}\">\n",
            xml_escape(poster),
            xml_escape(&subject)
        ));
        xml.push_str("    <groups>\n");
        xml.push_str(&format!("      <group>{NNTP_GROUP}</group>\n"));
        xml.push_str("    </groups>\n");
        xml.push_str("    <segments>\n");
        for seg in &file.segments {
            xml.push_str(&format!(
                "      <segment bytes=\"{}\" number=\"{}\">{}</segment>\n",
                seg.bytes,
                seg.number,
                xml_escape(&seg.message_id)
            ));
        }
        xml.push_str("    </segments>\n");
        xml.push_str("  </file>\n");
    }

    xml.push_str("</nzb>\n");
    xml
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Build segments for a file of given size using the standard article size.
pub fn build_segments(msg_prefix: &str, file_size: u64) -> Vec<NzbSegment> {
    let total_parts = file_size.div_ceil(ARTICLE_SIZE) as u32;
    let mut segments = Vec::with_capacity(total_parts as usize);

    for part in 1..=total_parts {
        let offset = (part as u64 - 1) * ARTICLE_SIZE;
        let bytes = std::cmp::min(ARTICLE_SIZE, file_size - offset);
        segments.push(NzbSegment {
            message_id: format!("{msg_prefix}-p{part:05}@{MSG_ID_DOMAIN}"),
            bytes,
            number: part,
        });
    }
    segments
}
