use std::collections::BTreeSet;

use crate::error::{NntpError, NntpResult};

const REQUIRED_HEADER_FIELDS: [&[u8]; 5] = [
    b"Subject:",
    b"From:",
    b"Date:",
    b"Message-ID:",
    b"References:",
];
const MAX_OVERVIEW_FIELDS: usize = 64;
const MAX_FIELD_DESCRIPTOR_BYTES: usize = 256;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OverviewFormat {
    pub fields: Vec<Vec<u8>>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LosslessOverviewRow {
    pub article_number: u64,
    pub fields: Vec<Vec<u8>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DefectiveOverviewRowCode {
    ArticleNumberInvalid,
    ArticleNumberOutOfRange,
    DuplicateArticleNumber,
    FieldCountInvalid,
}

impl DefectiveOverviewRowCode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ArticleNumberInvalid => "article_number_invalid",
            Self::ArticleNumberOutOfRange => "article_number_out_of_range",
            Self::DuplicateArticleNumber => "duplicate_article_number",
            Self::FieldCountInvalid => "field_count_invalid",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DefectiveOverviewRow {
    pub article_number: Option<u64>,
    pub wire_line: Vec<u8>,
    pub failure_code: DefectiveOverviewRowCode,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LosslessOverviewRows {
    pub rows: Vec<LosslessOverviewRow>,
    pub defective_rows: Vec<DefectiveOverviewRow>,
}

pub fn parse_overview_format(data: &[u8]) -> NntpResult<OverviewFormat> {
    let fields = wire_lines(data)
        .map(|line| line.to_vec())
        .collect::<Vec<_>>();
    if fields.len() < REQUIRED_HEADER_FIELDS.len() + 2 || fields.len() > MAX_OVERVIEW_FIELDS {
        return Err(NntpError::Protocol(
            "LIST OVERVIEW.FMT field count is invalid".into(),
        ));
    }
    let metadata_fields_valid = fields[5].eq_ignore_ascii_case(b":bytes")
        && fields[6].eq_ignore_ascii_case(b":lines")
        || fields[5].eq_ignore_ascii_case(b"Bytes:") && fields[6].eq_ignore_ascii_case(b"Lines:");
    if !metadata_fields_valid {
        return Err(NntpError::Protocol(
            "LIST OVERVIEW.FMT metadata fields are invalid".into(),
        ));
    }
    for (index, field) in fields.iter().enumerate() {
        if field.is_empty()
            || field.len() > MAX_FIELD_DESCRIPTOR_BYTES
            || !field.iter().all(u8::is_ascii)
        {
            return Err(NntpError::Protocol(
                "LIST OVERVIEW.FMT field descriptor is invalid".into(),
            ));
        }
        if let Some(required) = REQUIRED_HEADER_FIELDS.get(index)
            && !field.eq_ignore_ascii_case(required)
        {
            return Err(NntpError::Protocol(
                "LIST OVERVIEW.FMT required field order is invalid".into(),
            ));
        }
    }
    Ok(OverviewFormat { fields })
}

pub fn parse_lossless_overview_rows(
    data: &[u8],
    format: &OverviewFormat,
    start_article: u64,
    end_article: u64,
) -> LosslessOverviewRows {
    let mut rows = Vec::new();
    let mut defective_rows = Vec::new();
    let mut seen = BTreeSet::new();
    for line in wire_lines(data) {
        let mut parts = line.split(|byte| *byte == b'\t');
        let article_bytes = parts.next().unwrap_or_default();
        let article_number = parse_ascii_u64(article_bytes);
        let fields = parts.map(|part| part.to_vec()).collect::<Vec<_>>();
        let failure_code = if article_number.is_none() {
            Some(DefectiveOverviewRowCode::ArticleNumberInvalid)
        } else if article_number
            .is_some_and(|number| number < start_article || number > end_article)
        {
            Some(DefectiveOverviewRowCode::ArticleNumberOutOfRange)
        } else if fields.len() != format.fields.len() {
            Some(DefectiveOverviewRowCode::FieldCountInvalid)
        } else if article_number.is_some_and(|number| !seen.insert(number)) {
            Some(DefectiveOverviewRowCode::DuplicateArticleNumber)
        } else {
            None
        };
        if let Some(failure_code) = failure_code {
            defective_rows.push(DefectiveOverviewRow {
                article_number,
                wire_line: line.to_vec(),
                failure_code,
            });
        } else if let Some(article_number) = article_number {
            rows.push(LosslessOverviewRow {
                article_number,
                fields,
            });
        }
    }
    LosslessOverviewRows {
        rows,
        defective_rows,
    }
}

fn wire_lines(data: &[u8]) -> impl Iterator<Item = &[u8]> {
    data.split(|byte| *byte == b'\n').filter_map(|line| {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        (!line.is_empty()).then_some(line)
    })
}

fn parse_ascii_u64(value: &[u8]) -> Option<u64> {
    if value.is_empty() || !value.iter().all(u8::is_ascii_digit) {
        return None;
    }
    std::str::from_utf8(value).ok()?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn format() -> OverviewFormat {
        parse_overview_format(
            b"Subject:\r\nFrom:\r\nDate:\r\nMessage-ID:\r\nReferences:\r\n:bytes\r\n:lines\r\nXref:full\r\n",
        )
        .expect("format")
    }

    #[test]
    fn format_requires_the_standard_prefix_and_preserves_optional_fields() {
        let format = format();
        assert_eq!(format.fields.len(), 8);
        assert_eq!(format.fields[7], b"Xref:full");
        assert!(parse_overview_format(b"From:\r\nSubject:\r\n").is_err());
    }

    #[test]
    fn format_accepts_and_preserves_the_alternative_metadata_names() {
        let format = parse_overview_format(
            b"Subject:\r\nFrom:\r\nDate:\r\nMessage-ID:\r\nReferences:\r\nBytes:\r\nLines:\r\nXref:full\r\n",
        )
        .expect("alternative metadata names");
        assert_eq!(format.fields[5], b"Bytes:");
        assert_eq!(format.fields[6], b"Lines:");
        assert!(
            parse_overview_format(
                b"Subject:\r\nFrom:\r\nDate:\r\nMessage-ID:\r\nReferences:\r\nBytes:\r\n:lines\r\n"
            )
            .is_err()
        );
    }

    #[test]
    fn rows_preserve_legacy_bytes_and_negotiated_order() {
        let rows = parse_lossless_overview_rows(
            b"42\tEspa\xf1a\tPoster\tDate\t<id@example>\t\t10\t1\tserver group:42\r\n",
            &format(),
            40,
            50,
        );
        assert!(rows.defective_rows.is_empty());
        assert_eq!(rows.rows[0].article_number, 42);
        assert_eq!(rows.rows[0].fields[0], b"Espa\xf1a");
        assert_eq!(rows.rows[0].fields[7], b"server group:42");
    }

    #[test]
    fn malformed_rows_remain_exact_and_typed() {
        let rows = parse_lossless_overview_rows(
            b"bad\tSubject\tPoster\tDate\t<id@example>\t\t10\t1\tserver group:1\r\n43\tSub\tject\tPoster\tDate\t<id@example>\t\t10\t1\tserver group:43\r\n",
            &format(),
            40,
            50,
        );
        assert_eq!(rows.defective_rows.len(), 2);
        assert_eq!(
            rows.defective_rows[0].failure_code,
            DefectiveOverviewRowCode::ArticleNumberInvalid
        );
        assert_eq!(rows.defective_rows[0].wire_line[0..3], *b"bad");
        assert_eq!(
            rows.defective_rows[1].failure_code,
            DefectiveOverviewRowCode::FieldCountInvalid
        );
    }
}
