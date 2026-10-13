pub mod admissions;
pub mod group_handlers;
pub mod group_observation;
pub mod handlers;
pub mod server;

#[cfg(feature = "webdav")]
pub mod dav;

use tracing_subscriber::EnvFilter;
use tracing_subscriber::reload::Handle;

/// The installed tracing filter, kept so a settings change can raise or lower
/// the log level without a restart. Unset in any process that never installed
/// this subscriber, which includes the in-process test harness.
type LogFilterHandle = Handle<EnvFilter, tracing_subscriber::Registry>;

pub static LOG_FILTER: std::sync::OnceLock<LogFilterHandle> = std::sync::OnceLock::new();

/// The directive the debug-logging toggle installs when it is turned on.
///
/// It raises rustnzb's own crates to debug and leaves the default at info, so
/// transport crates (`h2`, `hyper`, `tonic`, `reqwest`) and the OTLP exporter
/// stay quiet. A bare `debug` would export those crates' own debug events
/// through the log bridge and flood Loki.
pub const DEBUG_LOG_DIRECTIVE: &str = "info,nzb_core=debug,nzb_news=debug,nzb_dispatch=debug,nzb_nntp=debug,nzb_decode=debug,nzb_postproc=debug,nzb_web=debug,rustnzb=debug";

/// The log filter source that beats `general.log_level` in this process:
/// `RUST_LOG`, `--log-level` or `RUSTNZB_LOG_LEVEL`, with the filter it
/// resolved to. `main` sets it once at startup; `None` means config rules.
pub static LOG_OVERRIDE: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();

/// The override in effect, if any. A process that never set [`LOG_OVERRIDE`]
/// (the test harness) reads `RUST_LOG` and `RUSTNZB_LOG_LEVEL` directly.
pub fn log_level_override() -> Option<String> {
    if let Some(set) = LOG_OVERRIDE.get() {
        return set.clone();
    }
    ["RUST_LOG", "RUSTNZB_LOG_LEVEL"]
        .into_iter()
        .find_map(|key| {
            std::env::var(key)
                .ok()
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        })
}

/// Whether a filter enables debug (or trace) for rustnzb's own crates.
pub fn filter_is_debug(directive: &str) -> bool {
    directive
        .split(',')
        .map(str::trim)
        .any(|part| match part.split_once('=') {
            None => matches!(part.to_ascii_lowercase().as_str(), "debug" | "trace"),
            Some((target, level)) => {
                (target.starts_with("nzb_") || target.starts_with("rustnzb"))
                    && matches!(level.to_ascii_lowercase().as_str(), "debug" | "trace")
            }
        })
}

/// Replace the live log filter. Returns false when no filter was installed or
/// the directive does not parse.
pub fn set_log_filter(directive: &str) -> bool {
    let Some(handle) = LOG_FILTER.get() else {
        return false;
    };
    let Ok(filter) = EnvFilter::try_new(directive) else {
        return false;
    };
    handle.reload(filter).is_ok()
}
