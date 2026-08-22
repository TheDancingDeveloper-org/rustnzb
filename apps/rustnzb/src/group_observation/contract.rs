use serde::Deserialize;

const MAX_OBSERVATION_HEADERS: u64 = 10_000;
const MAX_PATTERN_ARTICLES: u64 = 100_000;
const MAX_PATTERNS: usize = 16;
const MAX_PATTERN_COMMAND_BYTES: usize = 400;
const MAX_PATTERN_MATCHES: usize = 1_000;

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
pub(crate) struct HeaderPatternInput {
    pub(crate) request_id: String,
    pub(crate) group: String,
    pub(crate) start_article: u64,
    pub(crate) end_article: u64,
    pub(crate) patterns: Vec<String>,
    pub(crate) max_matches: usize,
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

impl HeaderPatternInput {
    pub(super) fn validate(&self) -> Result<(), &'static str> {
        validate_observation_identity(&self.request_id, &self.group)?;
        range_count(self.start_article, self.end_article, MAX_PATTERN_ARTICLES)?;
        if self.patterns.is_empty()
            || self.patterns.len() > MAX_PATTERNS
            || self.max_matches == 0
            || self.max_matches > MAX_PATTERN_MATCHES
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
        {
            return Err("header pattern request is outside its admitted bounds");
        }
        Ok(())
    }
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

        let pattern = HeaderPatternInput {
            request_id: "pattern-one".to_string(),
            group: "esp.binarios.series.misc".to_string(),
            start_article: 1,
            end_article: 10_000,
            patterns: vec!["*Traitors*Espana*".to_string()],
            max_matches: 100,
        };
        assert!(pattern.validate().is_ok());

        let mut broad = HeaderPatternInput {
            request_id: "pattern-two".to_string(),
            group: "esp.binarios.series.misc".to_string(),
            start_article: 1,
            end_article: 100_001,
            patterns: vec!["*".to_string()],
            max_matches: 100,
        };
        assert!(broad.validate().is_err());
        broad.end_article = 100_000;
        assert!(broad.validate().is_err());

        let mut injection = pattern;
        injection.patterns = vec!["*Traitors*\r\nQUIT".to_string()];
        assert!(injection.validate().is_err());
    }
}
