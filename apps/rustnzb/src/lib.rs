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
