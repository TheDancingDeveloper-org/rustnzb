//! Per-stage and per-server download metrics.
//!
//! Instruments are built once and cached. The global meter provider must be
//! installed before the first recording, which `main` does before any job
//! starts; until then (and in tests) the global provider is a noop, so every
//! call here is cheap and safe.

use std::sync::OnceLock;

use opentelemetry::KeyValue;
use opentelemetry::metrics::{Counter, Histogram};

/// The meter every rustnzb stage metric shares.
pub const METER: &str = "rustnzb";

/// `rustnzb.stage.duration`: seconds spent in one stage of one job (or, for
/// `connect`, one worker connection).
pub fn stage_duration() -> &'static Histogram<f64> {
    static H: OnceLock<Histogram<f64>> = OnceLock::new();
    H.get_or_init(|| {
        opentelemetry::global::meter(METER)
            .f64_histogram("rustnzb.stage.duration")
            .with_unit("s")
            .with_description("Wall or cumulative time per download stage")
            .build()
    })
}

/// Record one stage observation, with an optional `server` attribute.
pub fn record_stage(stage: &'static str, server: Option<&str>, seconds: f64) {
    match server {
        Some(server) => stage_duration().record(
            seconds,
            &[
                KeyValue::new("stage", stage),
                KeyValue::new("server", server.to_string()),
            ],
        ),
        None => stage_duration().record(seconds, &[KeyValue::new("stage", stage)]),
    }
}

struct ServerCounters {
    bytes: Counter<u64>,
    articles: Counter<u64>,
    response_wait: Counter<f64>,
}

fn server_counters() -> &'static ServerCounters {
    static C: OnceLock<ServerCounters> = OnceLock::new();
    C.get_or_init(|| {
        let meter = opentelemetry::global::meter(METER);
        ServerCounters {
            bytes: meter
                .u64_counter("rustnzb.nntp.bytes")
                .with_unit("By")
                .with_description("Decoded article bytes received, per server")
                .build(),
            articles: meter
                .u64_counter("rustnzb.nntp.articles")
                .with_description("Articles received and decoded, per server")
                .build(),
            response_wait: meter
                .f64_counter("rustnzb.nntp.response_wait")
                .with_unit("s")
                .with_description("Time workers spent waiting for article responses, per server")
                .build(),
        }
    })
}

/// Add one worker's accumulated NNTP numbers for `server`. Called from the
/// worker's periodic perf summary, not per article, so the hot path only
/// touches local integers.
///
/// Throughput is `rate(bytes)`; mean response latency per article is
/// `rate(response_wait) / rate(articles)`.
pub fn record_server_window(server: &str, bytes: u64, articles: u64, response_wait_secs: f64) {
    if articles == 0 && bytes == 0 {
        return;
    }
    let attrs = [KeyValue::new("server", server.to_string())];
    let c = server_counters();
    c.bytes.add(bytes, &attrs);
    c.articles.add(articles, &attrs);
    c.response_wait.add(response_wait_secs, &attrs);
}
