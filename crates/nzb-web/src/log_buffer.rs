//! In-memory ring buffer for log entries, with per-job filtering.
//!
//! A custom `tracing::Layer` captures log events into a bounded buffer.
//! The HTTP API can then serve these entries globally or filtered by job_id.

use std::collections::VecDeque;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use parking_lot::RwLock;
use serde::Serialize;
use tracing::field::{Field, Visit};
use tracing::span;
use tracing_subscriber::Layer;
use tracing_subscriber::layer::Context;

/// Maximum number of log entries kept in the ring buffer.
const MAX_LOG_ENTRIES: usize = 2000;

/// A single captured log entry.
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct LogEntry {
    pub timestamp: DateTime<Utc>,
    pub level: String,
    pub target: String,
    pub message: String,
    pub job_id: Option<String>,
    /// Monotonic sequence number for pagination
    pub seq: u64,
}

/// Thread-safe ring buffer of log entries.
#[derive(Clone)]
pub struct LogBuffer {
    inner: Arc<RwLock<LogBufferInner>>,
}

struct LogBufferInner {
    entries: VecDeque<LogEntry>,
    next_seq: u64,
}

impl Default for LogBuffer {
    fn default() -> Self {
        Self::new()
    }
}

impl LogBuffer {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(LogBufferInner {
                entries: VecDeque::with_capacity(MAX_LOG_ENTRIES),
                next_seq: 0,
            })),
        }
    }

    /// Push a log entry into the buffer.
    fn push(&self, mut entry: LogEntry) {
        let mut inner = self.inner.write();
        entry.seq = inner.next_seq;
        inner.next_seq += 1;

        if inner.entries.len() >= MAX_LOG_ENTRIES {
            inner.entries.pop_front();
        }
        inner.entries.push_back(entry);
    }

    /// Get all entries (optionally filtered by job_id), after a given sequence number.
    pub fn get_entries(
        &self,
        job_id: Option<&str>,
        after_seq: Option<u64>,
        level: Option<&str>,
        limit: usize,
    ) -> Vec<LogEntry> {
        let inner = self.inner.read();
        inner
            .entries
            .iter()
            .filter(|e| {
                if let Some(after) = after_seq
                    && e.seq <= after
                {
                    return false;
                }
                if let Some(jid) = job_id
                    && e.job_id.as_deref() != Some(jid)
                {
                    return false;
                }
                if let Some(lvl) = level
                    && !e.level.eq_ignore_ascii_case(lvl)
                {
                    return false;
                }
                true
            })
            .rev()
            .take(limit)
            .cloned()
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect()
    }

    /// Get the latest sequence number.
    pub fn latest_seq(&self) -> u64 {
        let inner = self.inner.read();
        if inner.next_seq > 0 {
            inner.next_seq - 1
        } else {
            0
        }
    }
}

/// A visitor that extracts fields from tracing events into strings.
struct FieldVisitor {
    message: String,
    job_id: Option<String>,
}

impl Visit for FieldVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.message = format!("{value:?}");
        } else if field.name() == "job_id" {
            self.job_id = Some(format!("{value:?}"));
        } else {
            if !self.message.is_empty() {
                self.message.push(' ');
            }
            self.message
                .push_str(&format!("{}={:?}", field.name(), value));
        }
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message = value.to_string();
        } else if field.name() == "job_id" {
            self.job_id = Some(value.to_string());
        } else {
            if !self.message.is_empty() {
                self.message.push(' ');
            }
            self.message
                .push_str(&format!("{}={}", field.name(), value));
        }
    }
}

/// Tracing layer that captures events into a `LogBuffer`.
pub struct LogBufferLayer {
    buffer: LogBuffer,
}

impl LogBufferLayer {
    pub fn new(buffer: LogBuffer) -> Self {
        Self { buffer }
    }
}

impl<S> Layer<S> for LogBufferLayer
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    fn on_event(&self, event: &tracing::Event<'_>, ctx: Context<'_, S>) {
        let metadata = event.metadata();

        // Extract fields
        let mut visitor = FieldVisitor {
            message: String::new(),
            job_id: None,
        };
        event.record(&mut visitor);

        // Also check parent spans for job_id
        if visitor.job_id.is_none()
            && let Some(scope) = ctx.event_scope(event)
        {
            for span in scope {
                let extensions = span.extensions();
                if let Some(fields) = extensions.get::<SpanFields>()
                    && visitor.job_id.is_none()
                {
                    visitor.job_id = fields.job_id.clone();
                }
            }
        }

        let entry = LogEntry {
            timestamp: Utc::now(),
            level: metadata.level().to_string(),
            target: metadata.target().to_string(),
            message: visitor.message,
            job_id: visitor.job_id,
            seq: 0, // will be set by push()
        };

        self.buffer.push(entry);
    }

    fn on_new_span(&self, attrs: &span::Attributes<'_>, id: &span::Id, ctx: Context<'_, S>) {
        let mut visitor = FieldVisitor {
            message: String::new(),
            job_id: None,
        };
        attrs.record(&mut visitor);

        if visitor.job_id.is_some()
            && let Some(span) = ctx.span(id)
        {
            let mut extensions = span.extensions_mut();
            extensions.insert(SpanFields {
                job_id: visitor.job_id,
            });
        }
    }
}

/// Fields stored on spans for propagation to child events.
struct SpanFields {
    job_id: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(level: &str, job_id: Option<&str>) -> LogEntry {
        LogEntry {
            timestamp: Utc::now(),
            level: level.into(),
            target: "test".into(),
            message: "message".into(),
            job_id: job_id.map(str::to_string),
            seq: 0,
        }
    }

    #[test]
    fn filters_entries_by_sequence_job_and_level_in_chronological_order() {
        let buffer = LogBuffer::new();
        buffer.push(entry("INFO", Some("one")));
        buffer.push(entry("WARN", Some("two")));
        buffer.push(entry("INFO", Some("one")));

        let entries = buffer.get_entries(Some("one"), Some(0), Some("info"), 10);
        assert_eq!(
            entries.iter().map(|entry| entry.seq).collect::<Vec<_>>(),
            vec![2]
        );
        assert_eq!(buffer.latest_seq(), 2);
    }

    #[test]
    fn ring_buffer_discards_oldest_entries_at_capacity() {
        let buffer = LogBuffer::new();
        for _ in 0..=MAX_LOG_ENTRIES {
            buffer.push(entry("INFO", None));
        }
        let entries = buffer.get_entries(None, None, None, MAX_LOG_ENTRIES + 1);
        assert_eq!(entries.len(), MAX_LOG_ENTRIES);
        assert_eq!(entries.first().unwrap().seq, 1);
        assert_eq!(entries.last().unwrap().seq, MAX_LOG_ENTRIES as u64);
    }
}
