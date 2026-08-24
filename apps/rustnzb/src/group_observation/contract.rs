use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_OBSERVATION_HEADERS: u64 = 10_000;
const MAX_PATTERNS: usize = 16;
const MAX_PATTERN_COMMAND_BYTES: usize = 400;
const MAX_PATTERN_MATCHES: usize = 1_000;
pub(super) const MAX_HEAD_BYTES: usize = 64 * 1024;
pub(super) const MAX_BODY_PREFIX_BYTES: usize = 256 * 1024;
pub(super) const MAX_PAYLOAD_PREFIX_BYTES: usize = 64 * 1024;
const MAX_CLEAR_SEARCH_RESPONSE_BYTES: usize = 64 * 1024 * 1024;
const MAX_CLEAR_SEARCH_RANGES: usize = 8;
const MAX_CLEAR_SEARCH_ARTICLES_PER_RANGE: u64 = 10_000;
const MAX_CLEAR_SEARCH_DURATION_MS: u64 = 120_000;
const MAX_AVAILABILITY_SEGMENTS: usize = 64;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct OverviewRangeInput {
    pub(crate) request_id: String,
    pub(crate) group: String,
    pub(crate) start_article: u64,
    pub(crate) end_article: u64,
    pub(crate) max_headers: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ArticleHeadInput {
    pub(crate) request_id: String,
    pub(crate) group: String,
    pub(crate) article_number: u64,
    pub(crate) max_header_bytes: usize,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ArticleBodyPrefixInput {
    pub(crate) request_id: String,
    pub(crate) group: String,
    pub(crate) message_id: String,
    pub(crate) max_wire_bytes: usize,
    pub(crate) max_payload_bytes: usize,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ClearSearchRangeInput {
    pub(crate) start_article: u64,
    pub(crate) end_article: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ClearSearchInput {
    pub(crate) request_id: String,
    pub(crate) cancellation_id: String,
    pub(crate) group: String,
    pub(crate) ranges: Vec<ClearSearchRangeInput>,
    pub(crate) patterns: Vec<String>,
    pub(crate) predicate_sha256: String,
    pub(crate) max_matches_per_range: usize,
    pub(crate) max_response_bytes: usize,
    pub(crate) deadline_at_unix_ms: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ArticleAvailabilityInput {
    pub(crate) request_id: String,
    pub(crate) message_ids: Vec<String>,
    pub(crate) sample_sha256: String,
}

impl OverviewRangeInput {
    pub(super) fn validate(&self) -> Result<(), &'static str> {
        validate_observation_identity(&self.request_id, &self.group)?;
        let count = range_count(
            self.start_article,
            self.end_article,
            MAX_OBSERVATION_HEADERS,
        )?;
        if self.max_headers == 0
            || self.max_headers > MAX_OBSERVATION_HEADERS
            || count > self.max_headers
        {
            return Err("overview range exceeds its admitted header bound");
        }
        Ok(())
    }
}

impl ArticleHeadInput {
    pub(super) fn validate(&self) -> Result<(), &'static str> {
        validate_observation_identity(&self.request_id, &self.group)?;
        if self.article_number == 0
            || self.max_header_bytes == 0
            || self.max_header_bytes > MAX_HEAD_BYTES
        {
            return Err("article head request is outside its admitted bounds");
        }
        Ok(())
    }
}

impl ArticleBodyPrefixInput {
    pub(super) fn validate(&self) -> Result<(), &'static str> {
        validate_observation_identity(&self.request_id, &self.group)?;
        let message_id = self.message_id.trim_matches(['<', '>']);
        if message_id.is_empty()
            || message_id.len() > 2048
            || !message_id.contains('@')
            || !message_id
                .bytes()
                .all(|byte| byte.is_ascii_graphic() && !matches!(byte, b'/' | b'\\'))
            || self.max_wire_bytes == 0
            || self.max_wire_bytes > MAX_BODY_PREFIX_BYTES
            || self.max_payload_bytes == 0
            || self.max_payload_bytes > MAX_PAYLOAD_PREFIX_BYTES
            || self.max_payload_bytes > self.max_wire_bytes
        {
            return Err("article body prefix request is outside its admitted bounds");
        }
        Ok(())
    }
}

impl ClearSearchInput {
    pub(super) fn validate(&self) -> Result<(), &'static str> {
        validate_observation_identity(&self.request_id, &self.group)?;
        let now = now_unix_ms()?;
        if !valid_control_id(&self.cancellation_id)
            || self.ranges.is_empty()
            || self.ranges.len() > MAX_CLEAR_SEARCH_RANGES
            || self.ranges.iter().any(|range| {
                range_count(
                    range.start_article,
                    range.end_article,
                    MAX_CLEAR_SEARCH_ARTICLES_PER_RANGE,
                )
                .is_err()
            })
            || self
                .ranges
                .windows(2)
                .any(|pair| pair[0].end_article >= pair[1].start_article)
            || self.patterns.is_empty()
            || self.patterns.len() > MAX_PATTERNS
            || self.max_matches_per_range == 0
            || self.max_matches_per_range > MAX_PATTERN_MATCHES
            || self.patterns.iter().map(String::len).sum::<usize>()
                + self.patterns.len().saturating_sub(1)
                > MAX_PATTERN_COMMAND_BYTES
            || self.patterns.iter().any(|pattern| {
                pattern.is_empty()
                    || pattern.len() > 256
                    || pattern
                        .chars()
                        .filter(|character| character.is_alphanumeric())
                        .take(3)
                        .count()
                        < 3
                    || pattern
                        .bytes()
                        .any(|byte| byte.is_ascii_control() || byte.is_ascii_whitespace())
            })
            || self.predicate_sha256 != clear_search_predicate_digest(&self.patterns)
            || self.max_response_bytes == 0
            || self.max_response_bytes > MAX_CLEAR_SEARCH_RESPONSE_BYTES
            || self.deadline_at_unix_ms <= now
            || self.deadline_at_unix_ms.saturating_sub(now) > MAX_CLEAR_SEARCH_DURATION_MS
        {
            return Err("clear search request is outside its admitted bounds");
        }
        Ok(())
    }
}

impl ArticleAvailabilityInput {
    pub(super) fn validate(&self) -> Result<(), &'static str> {
        if !valid_control_id(&self.request_id)
            || self.message_ids.is_empty()
            || self.message_ids.len() > MAX_AVAILABILITY_SEGMENTS
            || self.message_ids.iter().any(|message_id| {
                let message_id = message_id.trim_matches(['<', '>']);
                message_id.is_empty()
                    || message_id.len() > 2048
                    || !message_id.contains('@')
                    || !message_id
                        .bytes()
                        .all(|byte| byte.is_ascii_graphic() && !matches!(byte, b'/' | b'\\'))
            })
            || self
                .message_ids
                .iter()
                .map(|message_id| message_id.trim_matches(['<', '>']))
                .collect::<std::collections::BTreeSet<_>>()
                .len()
                != self.message_ids.len()
            || self.sample_sha256 != article_availability_digest(&self.message_ids)
        {
            return Err("article availability request is outside its admitted bounds");
        }
        Ok(())
    }
}

pub(super) fn article_availability_digest(message_ids: &[String]) -> String {
    let mut digest = Sha256::new();
    digest.update(b"newsgroups-article-availability-sample");
    for message_id in message_ids {
        let message_id = message_id.trim_matches(['<', '>']);
        digest.update((message_id.len() as u64).to_be_bytes());
        digest.update(message_id.as_bytes());
    }
    format!("{:x}", digest.finalize())
}

pub(super) fn clear_search_predicate_digest(patterns: &[String]) -> String {
    let mut digest = Sha256::new();
    digest.update(b"newsgroups-clear-search-predicates");
    for pattern in patterns {
        digest.update((pattern.len() as u64).to_be_bytes());
        digest.update(pattern.as_bytes());
    }
    format!("{:x}", digest.finalize())
}

pub(super) fn now_unix_ms() -> Result<u64, &'static str> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .map_err(|_| "system clock is before the Unix epoch")
}

fn valid_control_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

fn validate_observation_identity(request_id: &str, group: &str) -> Result<(), &'static str> {
    if request_id.is_empty()
        || request_id.len() > 128
        || !request_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        || group.is_empty()
        || group.len() > 255
        || group.split('.').any(|component| {
            component.is_empty()
                || !component
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        })
    {
        return Err("observation identity is invalid");
    }
    Ok(())
}

fn range_count(start: u64, end: u64, maximum: u64) -> Result<u64, &'static str> {
    if start == 0 || end < start {
        return Err("article range is invalid");
    }
    let count = end
        .checked_sub(start)
        .and_then(|delta| delta.checked_add(1))
        .ok_or("article range is invalid")?;
    if count > maximum {
        return Err("article range exceeds the operation bound");
    }
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_are_exact_and_bounded() {
        let overview = OverviewRangeInput {
            request_id: "scan-one".to_string(),
            group: "esp.binarios.series.misc".to_string(),
            start_article: 10,
            end_article: 19,
            max_headers: 10,
        };
        assert!(overview.validate().is_ok());

        let mut oversized = overview;
        oversized.end_article = 10_010;
        assert!(oversized.validate().is_err());

        let mut head = ArticleHeadInput {
            request_id: "head-one".to_string(),
            group: "esp.binarios.series.misc".to_string(),
            article_number: 42,
            max_header_bytes: MAX_HEAD_BYTES,
        };
        assert!(head.validate().is_ok());
        head.max_header_bytes += 1;
        assert!(head.validate().is_err());

        let mut body = ArticleBodyPrefixInput {
            request_id: "body-one".to_string(),
            group: "esp.binarios.series.misc".to_string(),
            message_id: "one@example.invalid".to_string(),
            max_wire_bytes: MAX_BODY_PREFIX_BYTES,
            max_payload_bytes: MAX_PAYLOAD_PREFIX_BYTES,
        };
        assert!(body.validate().is_ok());
        body.max_payload_bytes = body.max_wire_bytes + 1;
        assert!(body.validate().is_err());

        let mut availability = ArticleAvailabilityInput {
            request_id: "availability-one".to_string(),
            message_ids: vec!["one@example.invalid".to_string()],
            sample_sha256: article_availability_digest(&["one@example.invalid".to_string()]),
        };
        assert!(availability.validate().is_ok());
        availability
            .message_ids
            .push("one@example.invalid".to_string());
        assert!(availability.validate().is_err());

        let mut clear = ClearSearchInput {
            request_id: "clear-one".to_string(),
            cancellation_id: "cancel-one".to_string(),
            group: "esp.binarios.series.misc".to_string(),
            ranges: vec![
                ClearSearchRangeInput {
                    start_article: 1,
                    end_article: 10,
                },
                ClearSearchRangeInput {
                    start_article: 20,
                    end_article: 30,
                },
            ],
            patterns: vec!["*Traitors*".to_string()],
            predicate_sha256: clear_search_predicate_digest(&["*Traitors*".to_string()]),
            max_matches_per_range: 100,
            max_response_bytes: 1024,
            deadline_at_unix_ms: now_unix_ms().expect("clock") + 1_000,
        };
        assert!(clear.validate().is_ok());
        clear.ranges[1].start_article = 10;
        assert!(clear.validate().is_err());
    }
}
