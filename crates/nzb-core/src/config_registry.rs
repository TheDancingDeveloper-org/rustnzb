//! Static settings registry.
//!
//! One entry per leaf of [`crate::config::AppConfig`] that the GUI schema can
//! describe. Servers, categories, RSS feeds and schedule rules stay on their
//! own editors and are not leaves here. See
//! `docs/gui-config-design-2026-10-16.md` §5.3 and
//! `docs/config-schema.md` for which keys the granular endpoints already write.

use serde::Serialize;

use crate::config::AppConfig;

/// How a changed value takes effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Apply {
    /// The running process picks the new value up without a restart.
    Live,
    /// The saved value differs from the value the process started with until
    /// the next start.
    Restart,
}

/// Who may change the setting, matching `nzb-web::auth_class::AuthClass`.
///
/// Defined here so the registry (and `GET /api/config/schema`) does not depend
/// on the web crate. `class_of` in `nzb-web` stays the gate for writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthClass {
    /// Session, HTTP Basic, or `X-Api-Key`.
    Normal,
    /// Interactive session plus a recent password confirmation. `X-Api-Key`
    /// and the SABnzbd API must not write it.
    Sensitive,
}

/// Value shape the GUI and `validate` use.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Kind {
    Bool,
    U64 {
        min: u64,
        max: u64,
    },
    Usize {
        min: u64,
        max: u64,
    },
    U16 {
        min: u16,
        max: u16,
    },
    U8 {
        min: u8,
        max: u8,
    },
    F64 {
        min: f64,
        max: f64,
    },
    /// Filesystem path. `optional` is `Option<PathBuf>`.
    Path {
        optional: bool,
    },
    String {
        optional: bool,
        /// `None` means unbounded.
        max_len: Option<usize>,
    },
    /// A secret string. The schema never includes the current value.
    Secret {
        optional: bool,
    },
    Enum {
        values: &'static [&'static str],
    },
    /// A list of strings (hosts, CIDRs, categories).
    StringList,
}

/// One leaf setting.
#[derive(Debug, Clone, Serialize)]
pub struct Setting {
    pub key: &'static str,
    pub kind: Kind,
    /// JSON value of the serde default, taken from `AppConfig::default()`.
    pub default: serde_json::Value,
    pub apply: Apply,
    pub class: AuthClass,
    /// Environment names, primary first, then legacy aliases.
    pub env: &'static [&'static str],
    pub cli: Option<&'static str>,
    pub group: &'static str,
    pub label: &'static str,
    /// This key is already written by a granular endpoint from WI-2079
    /// (paths, post-processing, network). The schema still lists it so the
    /// GUI can render one form; writers must keep using that endpoint.
    pub granular: bool,
}

const U64_MAX: u64 = u64::MAX;

struct Draft {
    key: &'static str,
    kind: Kind,
    default: serde_json::Value,
    apply: Apply,
    class: AuthClass,
    env: &'static [&'static str],
    cli: Option<&'static str>,
    group: &'static str,
    label: &'static str,
    granular: bool,
}

fn setting(draft: Draft) -> Setting {
    Setting {
        key: draft.key,
        kind: draft.kind,
        default: draft.default,
        apply: draft.apply,
        class: draft.class,
        env: draft.env,
        cli: draft.cli,
        group: draft.group,
        label: draft.label,
        granular: draft.granular,
    }
}

fn path_default(path: &std::path::Path) -> serde_json::Value {
    serde_json::Value::String(path.display().to_string())
}

/// Every leaf the schema publishes, in GUI-section order.
pub fn settings() -> Vec<Setting> {
    use Apply::{Live, Restart};
    use AuthClass::{Normal, Sensitive};
    let defaults = AppConfig::default();
    let general = &defaults.general;
    let otel = &defaults.otel;
    let dav = &defaults.dav;
    let note = &defaults.notifications;
    let par2 = &defaults.par2;
    vec![
        // --- general: already covered by WI-2079 granular endpoints ---
        setting(Draft {
            key: "general.listen_addr",
            kind: Kind::String {
                optional: false,
                max_len: Some(256),
            },
            default: serde_json::json!(general.listen_addr),
            apply: Restart,
            class: Sensitive,
            env: &["RUSTNZB_LISTEN_ADDR"],
            cli: Some("--listen-addr"),
            group: "api",
            label: "Listen address",
            granular: true,
        }),
        setting(Draft {
            key: "general.port",
            kind: Kind::U16 { min: 1, max: 65535 },
            default: serde_json::json!(general.port),
            apply: Restart,
            class: Sensitive,
            env: &["RUSTNZB_PORT"],
            cli: Some("--port"),
            group: "api",
            label: "HTTP port",
            granular: true,
        }),
        setting(Draft {
            key: "general.api_key",
            kind: Kind::Secret { optional: true },
            default: serde_json::Value::Null,
            apply: Live,
            class: Sensitive,
            env: &[],
            cli: None,
            group: "api",
            label: "API key",
            granular: true,
        }),
        setting(Draft {
            key: "general.incomplete_dir",
            kind: Kind::Path { optional: false },
            default: path_default(&general.incomplete_dir),
            apply: Live,
            class: Normal,
            env: &[],
            cli: None,
            group: "paths",
            label: "Incomplete directory",
            granular: true,
        }),
        setting(Draft {
            key: "general.complete_dir",
            kind: Kind::Path { optional: false },
            default: path_default(&general.complete_dir),
            apply: Live,
            class: Normal,
            env: &[],
            cli: None,
            group: "paths",
            label: "Complete directory",
            granular: true,
        }),
        setting(Draft {
            key: "general.data_dir",
            kind: Kind::Path { optional: false },
            default: path_default(&general.data_dir),
            apply: Restart,
            class: Sensitive,
            env: &["RUSTNZB_DATA_DIR"],
            cli: Some("--data-dir"),
            group: "paths",
            label: "Data directory",
            granular: true,
        }),
        setting(Draft {
            key: "general.speed_limit_bps",
            kind: Kind::U64 {
                min: 0,
                max: U64_MAX,
            },
            default: serde_json::json!(general.speed_limit_bps),
            apply: Live,
            class: Normal,
            env: &[],
            cli: None,
            group: "general",
            label: "Speed limit (bytes/sec, 0 = unlimited)",
            granular: true,
        }),
        setting(Draft {
            key: "general.log_level",
            kind: Kind::Enum {
                values: &["error", "warn", "info", "debug", "trace"],
            },
            default: serde_json::json!(general.log_level),
            apply: Restart,
            class: Normal,
            env: &["RUSTNZB_LOG_LEVEL", "RUST_LOG"],
            cli: Some("--log-level"),
            group: "general",
            label: "Log level",
            granular: true,
        }),
        setting(Draft {
            key: "general.history_retention",
            kind: Kind::Usize {
                min: 0,
                max: U64_MAX,
            },
            default: serde_json::json!(0),
            apply: Live,
            class: Normal,
            env: &[],
            cli: None,
            group: "general",
            label: "History retention (0 = keep all)",
            granular: true,
        }),
        setting(Draft {
            key: "general.keep_history_on_arr_delete",
            kind: Kind::Bool,
            default: serde_json::json!(general.keep_history_on_arr_delete),
            apply: Live,
            class: Normal,
            env: &[],
            cli: None,
            group: "general",
            label: "Keep history when an arr client deletes a job",
            granular: true,
        }),
        setting(Draft {
            key: "general.max_active_downloads",
            kind: Kind::Usize { min: 1, max: 100 },
            default: serde_json::json!(general.max_active_downloads),
            apply: Live,
            class: Normal,
            env: &[],
            cli: None,
            group: "general",
            label: "Maximum active downloads",
            granular: true,
        }),
        setting(Draft {
            key: "general.min_free_space_bytes",
            kind: Kind::U64 {
                min: 0,
                max: U64_MAX,
            },
            default: serde_json::json!(general.min_free_space_bytes),
            apply: Live,
            class: Normal,
            env: &[],
            cli: None,
            group: "disk",
            label: "Minimum free space (bytes)",
            granular: true,
        }),
        setting(Draft {
            key: "general.watch_dir",
            kind: Kind::Path { optional: true },
            default: serde_json::Value::Null,
            apply: Live,
            class: Normal,
            env: &[],
            cli: None,
            group: "paths",
            label: "Watched folder",
            granular: true,
        }),
        setting(Draft {
            key: "general.direct_unpack",
            kind: Kind::Bool,
            default: serde_json::json!(general.direct_unpack),
            apply: Live,
            class: Normal,
            env: &[],
            cli: None,
            group: "post",
            label: "Direct unpack",
            granular: true,
        }),
        setting(Draft {
            key: "general.max_nested_archive_depth",
            kind: Kind::U8 { min: 1, max: 20 },
            default: serde_json::json!(general.max_nested_archive_depth),
            apply: Restart,
            class: Normal,
            env: &[],
            cli: None,
            group: "post",
            label: "Maximum nested archive depth",
            granular: true,
        }),
        setting(Draft {
            key: "general.scripts_dir",
            kind: Kind::Path { optional: true },
            default: serde_json::Value::Null,
            apply: Live,
            class: Sensitive,
            env: &[],
            cli: None,
            group: "post",
            label: "Scripts directory",
            granular: true,
        }),
        setting(Draft {
            key: "general.script_success",
            kind: Kind::Path { optional: true },
            default: serde_json::Value::Null,
            apply: Live,
            class: Sensitive,
            env: &[],
            cli: None,
            group: "post",
            label: "Success script",
            granular: true,
        }),
        setting(Draft {
            key: "general.script_failure",
            kind: Kind::Path { optional: true },
            default: serde_json::Value::Null,
            apply: Live,
            class: Sensitive,
            env: &[],
            cli: None,
            group: "post",
            label: "Failure script",
            granular: true,
        }),
        setting(Draft {
            key: "general.script_timeout_secs",
            kind: Kind::U64 {
                min: 1,
                max: 86_400,
            },
            default: serde_json::json!(general.script_timeout_secs),
            apply: Live,
            class: Sensitive,
            env: &[],
            cli: None,
            group: "post",
            label: "Script timeout (seconds)",
            granular: true,
        }),
        setting(Draft {
            key: "general.script_max_output_bytes",
            kind: Kind::Usize {
                min: 1024,
                max: 16 * 1024 * 1024,
            },
            default: serde_json::json!(general.script_max_output_bytes),
            apply: Live,
            class: Sensitive,
            env: &[],
            cli: None,
            group: "post",
            label: "Script output cap (bytes)",
            granular: true,
        }),
        setting(Draft {
            key: "general.fetch_allow_private",
            kind: Kind::Bool,
            default: serde_json::json!(general.fetch_allow_private),
            apply: Live,
            class: Sensitive,
            env: &[],
            cli: None,
            group: "network",
            label: "Allow fetches to private addresses",
            granular: true,
        }),
        setting(Draft {
            key: "general.fetch_allowed_hosts",
            kind: Kind::StringList,
            default: serde_json::json!(general.fetch_allowed_hosts),
            apply: Live,
            class: Sensitive,
            env: &[],
            cli: None,
            group: "network",
            label: "Extra hosts server-side fetch may reach",
            granular: true,
        }),
        setting(Draft {
            key: "general.group_max_headers",
            kind: Kind::Usize {
                min: 1,
                max: 10_000_000,
            },
            default: serde_json::json!(general.group_max_headers),
            apply: Live,
            class: Normal,
            env: &[],
            cli: None,
            group: "network",
            label: "Maximum headers fetched per group",
            granular: true,
        }),
        // --- not written by a WI-2079 endpoint ---
        setting(Draft {
            key: "config_version",
            kind: Kind::U64 { min: 0, max: 1_000 },
            default: serde_json::json!(defaults.config_version),
            apply: Restart,
            class: Normal,
            env: &[],
            cli: None,
            group: "internal",
            label: "Config schema version",
            granular: false,
        }),
        setting(Draft {
            key: "general.log_level_before_debug",
            kind: Kind::String {
                optional: true,
                max_len: Some(16),
            },
            default: serde_json::Value::Null,
            apply: Live,
            class: Normal,
            env: &[],
            cli: None,
            group: "internal",
            label: "Log level saved by the debug toggle",
            granular: false,
        }),
        setting(Draft {
            key: "general.cache_size",
            kind: Kind::U64 {
                min: 0,
                max: U64_MAX,
            },
            default: serde_json::json!(general.cache_size),
            apply: Restart,
            class: Normal,
            env: &[],
            cli: None,
            group: "general",
            label: "Article cache size (bytes)",
            granular: false,
        }),
        setting(Draft {
            key: "general.log_file",
            kind: Kind::Path { optional: true },
            default: serde_json::Value::Null,
            apply: Restart,
            class: Normal,
            env: &["RUSTNZB_LOG_FILE"],
            cli: Some("--log-file"),
            group: "general",
            label: "Log file",
            granular: false,
        }),
        setting(Draft {
            key: "general.max_post_processing_jobs",
            kind: Kind::Usize { min: 1, max: 32 },
            default: serde_json::json!(general.max_post_processing_jobs),
            apply: Restart,
            class: Normal,
            env: &[],
            cli: None,
            group: "post",
            label: "Maximum post-processing jobs",
            granular: false,
        }),
        setting(Draft {
            key: "general.max_repair_workers",
            kind: Kind::Usize { min: 1, max: 32 },
            default: serde_json::json!(general.max_repair_workers),
            apply: Restart,
            class: Normal,
            env: &[],
            cli: None,
            group: "post",
            label: "Maximum PAR2 repair workers",
            granular: false,
        }),
        setting(Draft {
            key: "general.max_extract_workers",
            kind: Kind::Usize { min: 1, max: 32 },
            default: serde_json::json!(general.max_extract_workers),
            apply: Restart,
            class: Normal,
            env: &[],
            cli: None,
            group: "post",
            label: "Maximum archive extract workers",
            granular: false,
        }),
        setting(Draft {
            key: "general.rss_history_limit",
            kind: Kind::Usize {
                min: 0,
                max: 1_000_000,
            },
            default: serde_json::json!(general.rss_history_limit.unwrap_or(0)),
            apply: Live,
            class: Normal,
            env: &[],
            cli: None,
            group: "rss",
            label: "RSS history rows kept (0 = default)",
            granular: false,
        }),
        setting(Draft {
            key: "general.abort_hopeless",
            kind: Kind::Bool,
            default: serde_json::json!(general.abort_hopeless),
            apply: Live,
            class: Normal,
            env: &[],
            cli: None,
            group: "post",
            label: "Abort downloads that cannot complete",
            granular: false,
        }),
        setting(Draft {
            key: "general.early_failure_check",
            kind: Kind::Bool,
            default: serde_json::json!(general.early_failure_check),
            apply: Live,
            class: Normal,
            env: &[],
            cli: None,
            group: "post",
            label: "Fail early when required articles are missing",
            granular: false,
        }),
        setting(Draft {
            key: "general.required_completion_pct",
            kind: Kind::F64 {
                min: 0.0,
                max: 200.0,
            },
            default: serde_json::json!(general.required_completion_pct),
            apply: Live,
            class: Normal,
            env: &[],
            cli: None,
            group: "post",
            label: "Required completion percent",
            granular: false,
        }),
        setting(Draft {
            key: "general.article_timeout_secs",
            kind: Kind::U64 { min: 1, max: 3_600 },
            default: serde_json::json!(general.article_timeout_secs),
            apply: Restart,
            class: Normal,
            env: &[],
            cli: None,
            group: "network",
            label: "Article timeout (seconds)",
            granular: false,
        }),
        setting(Draft {
            key: "general.auto_sort_remaining_pct",
            kind: Kind::Bool,
            default: serde_json::json!(general.auto_sort_remaining_pct),
            apply: Live,
            class: Normal,
            env: &[],
            cli: None,
            group: "general",
            label: "Sort the queue by remaining percent",
            granular: false,
        }),
        setting(Draft {
            key: "general.rss_downloaded_item_expiry_days",
            kind: Kind::U64 {
                min: 0,
                max: 36_500,
            },
            default: serde_json::json!(0),
            apply: Live,
            class: Normal,
            env: &[],
            cli: None,
            group: "rss",
            label: "Forget downloaded RSS items after (days, 0 = never)",
            granular: false,
        }),
        setting(Draft {
            key: "general.duplicate_policy",
            kind: Kind::Enum {
                values: &["allow", "warn", "skip"],
            },
            default: serde_json::json!(general.duplicate_policy.to_string()),
            apply: Live,
            class: Normal,
            env: &[],
            cli: None,
            group: "general",
            label: "Duplicate NZB policy",
            granular: false,
        }),
        setting(Draft {
            key: "general.local_import_dir",
            kind: Kind::Path { optional: true },
            default: serde_json::Value::Null,
            apply: Restart,
            class: Sensitive,
            env: &[],
            cli: None,
            group: "paths",
            label: "Local NZB import directory",
            granular: false,
        }),
        setting(Draft {
            key: "general.queue_complete_pers",
            kind: Kind::Bool,
            default: serde_json::json!(general.queue_complete_pers),
            apply: Restart,
            class: Normal,
            env: &[],
            cli: None,
            group: "general",
            label: "Persist queue-complete actions for SABnzbd clients",
            granular: false,
        }),
        setting(Draft {
            key: "general.shutdown_timeout_secs",
            kind: Kind::U64 { min: 1, max: 3_600 },
            default: serde_json::json!(general.shutdown_timeout_secs),
            apply: Restart,
            class: Normal,
            env: &[],
            cli: None,
            group: "api",
            label: "Shutdown timeout (seconds)",
            granular: false,
        }),
        setting(Draft {
            key: "general.trusted_proxies",
            kind: Kind::StringList,
            default: serde_json::json!(general.trusted_proxies),
            apply: Restart,
            class: Sensitive,
            env: &[],
            cli: None,
            group: "network",
            label: "Trusted reverse proxies",
            granular: false,
        }),
        setting(Draft {
            key: "general.step_up_ttl_secs",
            kind: Kind::U64 {
                min: 0,
                max: 86_400,
            },
            default: serde_json::json!(general.step_up_ttl_secs),
            apply: Restart,
            class: Sensitive,
            env: &[],
            cli: None,
            group: "api",
            label: "Password confirmation lifetime (seconds)",
            granular: false,
        }),
        setting(Draft {
            key: "otel.enabled",
            kind: Kind::Bool,
            default: serde_json::json!(otel.enabled),
            apply: Restart,
            class: Normal,
            env: &[],
            cli: None,
            group: "telemetry",
            label: "Enable OpenTelemetry",
            granular: false,
        }),
        setting(Draft {
            key: "otel.endpoint",
            kind: Kind::String {
                optional: false,
                max_len: Some(512),
            },
            default: serde_json::json!(otel.endpoint),
            apply: Restart,
            class: Normal,
            env: &[],
            cli: None,
            group: "telemetry",
            label: "OTLP endpoint",
            granular: false,
        }),
        setting(Draft {
            key: "otel.logs_enabled",
            kind: Kind::Bool,
            default: serde_json::json!(false),
            apply: Restart,
            class: Normal,
            env: &[],
            cli: None,
            group: "telemetry",
            label: "Enable OTLP logs (unset = follow enabled)",
            granular: false,
        }),
        setting(Draft {
            key: "otel.logs_endpoint",
            kind: Kind::String {
                optional: true,
                max_len: Some(512),
            },
            default: serde_json::Value::Null,
            apply: Restart,
            class: Normal,
            env: &[],
            cli: None,
            group: "telemetry",
            label: "OTLP logs endpoint",
            granular: false,
        }),
        setting(Draft {
            key: "otel.metrics_enabled",
            kind: Kind::Bool,
            default: serde_json::json!(false),
            apply: Restart,
            class: Normal,
            env: &[],
            cli: None,
            group: "telemetry",
            label: "Enable OTLP metrics (unset = follow enabled)",
            granular: false,
        }),
        setting(Draft {
            key: "otel.metrics_endpoint",
            kind: Kind::String {
                optional: true,
                max_len: Some(512),
            },
            default: serde_json::Value::Null,
            apply: Restart,
            class: Normal,
            env: &[],
            cli: None,
            group: "telemetry",
            label: "OTLP metrics endpoint",
            granular: false,
        }),
        setting(Draft {
            key: "otel.service_name",
            kind: Kind::String {
                optional: false,
                max_len: Some(128),
            },
            default: serde_json::json!(otel.service_name),
            apply: Restart,
            class: Normal,
            env: &[],
            cli: None,
            group: "telemetry",
            label: "Service name",
            granular: false,
        }),
        setting(Draft {
            key: "notifications.webhook_url",
            kind: Kind::Secret { optional: false },
            default: serde_json::json!(note.webhook_url),
            apply: Restart,
            class: Sensitive,
            env: &[],
            cli: None,
            group: "notifications",
            label: "Webhook URL",
            granular: false,
        }),
        setting(Draft {
            key: "notifications.apprise_api_url",
            kind: Kind::Secret { optional: false },
            default: serde_json::json!(note.apprise_api_url),
            apply: Restart,
            class: Sensitive,
            env: &[],
            cli: None,
            group: "notifications",
            label: "Apprise API URL",
            granular: false,
        }),
        setting(Draft {
            key: "notifications.apprise_urls",
            kind: Kind::Secret { optional: false },
            default: serde_json::json!(""),
            apply: Restart,
            class: Sensitive,
            env: &[],
            cli: None,
            group: "notifications",
            label: "Apprise notification URLs",
            granular: false,
        }),
        setting(Draft {
            key: "notifications.ntfy_server",
            kind: Kind::String {
                optional: false,
                max_len: Some(512),
            },
            default: serde_json::json!(note.ntfy_server),
            apply: Restart,
            class: Normal,
            env: &[],
            cli: None,
            group: "notifications",
            label: "ntfy server",
            granular: false,
        }),
        setting(Draft {
            key: "notifications.ntfy_topic",
            kind: Kind::String {
                optional: false,
                max_len: Some(256),
            },
            default: serde_json::json!(note.ntfy_topic),
            apply: Restart,
            class: Normal,
            env: &[],
            cli: None,
            group: "notifications",
            label: "ntfy topic",
            granular: false,
        }),
        setting(Draft {
            key: "notifications.ntfy_token",
            kind: Kind::Secret { optional: false },
            default: serde_json::json!(""),
            apply: Restart,
            class: Sensitive,
            env: &[],
            cli: None,
            group: "notifications",
            label: "ntfy access token",
            granular: false,
        }),
        setting(Draft {
            key: "dav.enabled",
            kind: Kind::Bool,
            default: serde_json::json!(dav.enabled),
            apply: Restart,
            class: Normal,
            env: &["RUSTNZB_DAV_ENABLED"],
            cli: None,
            group: "dav",
            label: "Mount the WebDAV library",
            granular: false,
        }),
        setting(Draft {
            key: "dav.auto_send_all",
            kind: Kind::Bool,
            default: serde_json::json!(dav.auto_send_all),
            apply: Live,
            class: Normal,
            env: &[],
            cli: None,
            group: "dav",
            label: "Send every completed download to DAV",
            granular: false,
        }),
        setting(Draft {
            key: "dav.category_rules",
            kind: Kind::StringList,
            default: serde_json::json!(dav.category_rules),
            apply: Live,
            class: Normal,
            env: &[],
            cli: None,
            group: "dav",
            label: "Categories sent to DAV",
            granular: false,
        }),
        setting(Draft {
            key: "dav.username",
            kind: Kind::String {
                optional: true,
                max_len: Some(256),
            },
            default: serde_json::Value::Null,
            apply: Restart,
            class: Normal,
            env: &[],
            cli: None,
            group: "dav",
            label: "DAV username",
            granular: false,
        }),
        setting(Draft {
            key: "dav.password",
            kind: Kind::Secret { optional: true },
            default: serde_json::Value::Null,
            apply: Restart,
            class: Sensitive,
            env: &[],
            cli: None,
            group: "dav",
            label: "DAV password",
            granular: false,
        }),
        setting(Draft {
            key: "dav.api_key",
            kind: Kind::Secret { optional: true },
            default: serde_json::Value::Null,
            apply: Restart,
            class: Sensitive,
            env: &[],
            cli: None,
            group: "dav",
            label: "DAV API key",
            granular: false,
        }),
        setting(Draft {
            key: "dav.playback_reserve_pct",
            kind: Kind::U8 { min: 0, max: 90 },
            default: serde_json::json!(dav.playback_reserve_pct),
            apply: Restart,
            class: Normal,
            env: &[],
            cli: None,
            group: "dav",
            label: "Playback connection reserve (percent)",
            granular: false,
        }),
        setting(Draft {
            key: "par2.defer_volumes",
            kind: Kind::Bool,
            default: serde_json::json!(par2.defer_volumes),
            apply: Live,
            class: Normal,
            env: &[],
            cli: None,
            group: "post",
            label: "Defer PAR2 volumes until damage is known",
            granular: false,
        }),
        setting(Draft {
            key: "par2.trust_inflight_verify",
            kind: Kind::Bool,
            default: serde_json::json!(par2.trust_inflight_verify),
            apply: Live,
            class: Normal,
            env: &[],
            cli: None,
            group: "post",
            label: "Trust the in-flight PAR2 verify",
            granular: false,
        }),
        setting(Draft {
            key: "schedule.enabled",
            kind: Kind::Bool,
            default: serde_json::json!(defaults.schedule.enabled),
            apply: Live,
            class: Normal,
            env: &[],
            cli: None,
            group: "schedule",
            label: "Enable the schedule",
            granular: false,
        }),
    ]
}

/// Look up one key.
pub fn get(key: &str) -> Option<Setting> {
    settings().into_iter().find(|setting| setting.key == key)
}

/// A config with every `Option` set, so TOML keeps the key.
///
/// `None` is omitted on serialise, which would hide real fields from the
/// leaf walk.
fn filled() -> AppConfig {
    let mut config = AppConfig::default();
    let path = |name: &str| std::path::PathBuf::from(name);
    config.general.api_key = Some("key".into());
    config.general.log_level_before_debug = Some("info".into());
    config.general.log_file = Some(path("/tmp/rustnzb.log"));
    config.general.history_retention = Some(1);
    config.general.watch_dir = Some(path("/tmp/watch"));
    config.general.rss_history_limit = Some(1);
    config.general.rss_downloaded_item_expiry_days = Some(1);
    config.general.local_import_dir = Some(path("/tmp/import"));
    config.general.scripts_dir = Some(path("/tmp/scripts"));
    config.general.script_success = Some(path("/tmp/scripts/ok.sh"));
    config.general.script_failure = Some(path("/tmp/scripts/bad.sh"));
    config.otel.logs_enabled = Some(false);
    config.otel.logs_endpoint = Some("http://127.0.0.1:4317".into());
    config.otel.metrics_enabled = Some(false);
    config.otel.metrics_endpoint = Some("http://127.0.0.1:4317".into());
    config.dav.username = Some("user".into());
    config.dav.password = Some("secret".into());
    config.dav.api_key = Some("key".into());
    config
}

/// `true` when `key` is a dotted path that exists on [`AppConfig`].
pub fn config_has_leaf(key: &str) -> bool {
    let value = toml::Value::try_from(filled()).expect("AppConfig serialises");
    let mut current = &value;
    for segment in key.split('.') {
        match current {
            toml::Value::Table(table) => {
                let Some(next) = table.get(segment) else {
                    return false;
                };
                current = next;
            }
            _ => return false,
        }
    }
    !matches!(current, toml::Value::Table(_))
}

/// Keys that name no real field.
pub fn unknown_keys() -> Vec<&'static str> {
    settings()
        .into_iter()
        .filter(|setting| !config_has_leaf(setting.key))
        .map(|setting| setting.key)
        .collect()
}

/// Leaf keys of the default config that the registry does not name.
///
/// Lists (`servers`, `categories`, `rss_feeds`, `schedule.rules`) are
/// collections with their own editors, so their contents are not leaves.
pub fn uncovered_leaves() -> Vec<String> {
    let value = toml::Value::try_from(filled()).expect("AppConfig serialises");
    let known: std::collections::BTreeSet<&str> =
        settings().iter().map(|setting| setting.key).collect();
    let mut found = Vec::new();
    walk(&value, "", &mut found);
    found.retain(|key| !known.contains(key.as_str()));
    found
}

fn walk(value: &toml::Value, prefix: &str, out: &mut Vec<String>) {
    // Lists have their own editors (servers, categories, feeds, rules).
    if matches!(value, toml::Value::Array(_)) {
        return;
    }
    let toml::Value::Table(table) = value else {
        if !prefix.is_empty() {
            out.push(prefix.to_string());
        }
        return;
    };
    for (name, child) in table {
        let key = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}.{name}")
        };
        walk(child, &key, out);
    }
}

/// Restart-required flag, for callers that only need the boolean.
pub fn restart_required(setting: &Setting) -> bool {
    setting.apply == Apply::Restart
}

/// Check one JSON value against a setting's [`Kind`].
///
/// On success the value is the shape `AppConfig` deserializes: numbers stay
/// numbers, paths and secrets are strings, and a missing optional value is
/// JSON null. The error names the constraint, not the key.
pub fn validate_value(kind: &Kind, value: &serde_json::Value) -> Result<serde_json::Value, String> {
    match kind {
        Kind::Bool => value
            .as_bool()
            .map(serde_json::Value::Bool)
            .ok_or_else(|| "expected a boolean".to_string()),
        Kind::U64 { min, max } => bounded_u64(value, *min, *max),
        Kind::Usize { min, max } => bounded_u64(value, *min, *max),
        Kind::U16 { min, max } => bounded_u64(value, u64::from(*min), u64::from(*max)),
        Kind::U8 { min, max } => bounded_u64(value, u64::from(*min), u64::from(*max)),
        Kind::F64 { min, max } => {
            let number = value
                .as_f64()
                .ok_or_else(|| "expected a number".to_string())?;
            if !number.is_finite() || number < *min || number > *max {
                return Err(format!("must be between {min} and {max}"));
            }
            Ok(serde_json::json!(number))
        }
        Kind::Path { optional } | Kind::String { optional, .. } | Kind::Secret { optional } => {
            text_value(kind, value, *optional)
        }
        Kind::Enum { values } => {
            let text = value
                .as_str()
                .ok_or_else(|| "expected a string".to_string())?;
            if values.contains(&text) {
                Ok(serde_json::Value::String(text.to_string()))
            } else {
                Err(format!("must be one of {}", values.join(", ")))
            }
        }
        Kind::StringList => {
            let list = value
                .as_array()
                .ok_or_else(|| "expected a list of strings".to_string())?;
            let mut out = Vec::with_capacity(list.len());
            for (index, item) in list.iter().enumerate() {
                let text = item
                    .as_str()
                    .ok_or_else(|| format!("item {index} expected a string"))?;
                out.push(serde_json::Value::String(text.to_string()));
            }
            Ok(serde_json::Value::Array(out))
        }
    }
}

fn bounded_u64(value: &serde_json::Value, min: u64, max: u64) -> Result<serde_json::Value, String> {
    let number = value
        .as_u64()
        .ok_or_else(|| "expected an integer".to_string())?;
    if number < min || number > max {
        return Err(format!("must be between {min} and {max}"));
    }
    Ok(serde_json::json!(number))
}

fn text_value(
    kind: &Kind,
    value: &serde_json::Value,
    optional: bool,
) -> Result<serde_json::Value, String> {
    if value.is_null() {
        return if optional {
            Ok(serde_json::Value::Null)
        } else {
            Err("must not be null".to_string())
        };
    }
    let text = value
        .as_str()
        .ok_or_else(|| "expected a string".to_string())?;
    if text.is_empty() {
        return if optional {
            Ok(serde_json::Value::Null)
        } else {
            Err("must not be empty".to_string())
        };
    }
    if let Kind::String {
        max_len: Some(max), ..
    } = kind
        && text.chars().count() > *max
    {
        return Err(format!("must be at most {max} characters"));
    }
    Ok(serde_json::Value::String(text.to_string()))
}

/// Replace one dotted leaf inside a JSON object. Missing objects along the
/// path are created. Returns the previous value, or null when the leaf was
/// absent.
pub fn set_leaf(
    root: &mut serde_json::Value,
    dotted: &str,
    value: serde_json::Value,
) -> Result<serde_json::Value, String> {
    let mut parts = dotted.split('.').peekable();
    let mut current = root;
    while let Some(part) = parts.next() {
        if parts.peek().is_none() {
            let object = current
                .as_object_mut()
                .ok_or_else(|| format!("{dotted} is not an object path"))?;
            let previous = object
                .insert(part.to_string(), value)
                .unwrap_or(serde_json::Value::Null);
            return Ok(previous);
        }
        let next = current
            .as_object_mut()
            .ok_or_else(|| format!("{dotted} is not an object path"))?
            .entry(part)
            .or_insert_with(|| serde_json::json!({}));
        if !next.is_object() {
            return Err(format!("{dotted} is not an object path"));
        }
        current = next;
    }
    Err("empty key".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_registry_key_is_a_real_config_field() {
        let unknown = unknown_keys();
        assert!(
            unknown.is_empty(),
            "registry keys missing from AppConfig: {unknown:?}"
        );
    }

    #[test]
    fn every_config_leaf_is_in_the_registry() {
        let missing = uncovered_leaves();
        assert!(
            missing.is_empty(),
            "AppConfig leaves missing from the registry: {missing:?}"
        );
    }

    #[test]
    fn schema_serialises_and_auth_class_matches_the_sensitive_list() {
        let body = serde_json::to_value(settings()).expect("schema json");
        let entries = body.as_array().expect("array");
        assert!(!entries.is_empty());
        let sensitive = [
            "general.api_key",
            "dav.password",
            "dav.api_key",
            "general.fetch_allow_private",
            "general.fetch_allowed_hosts",
            "general.scripts_dir",
            "general.script_success",
            "general.script_failure",
            "general.script_timeout_secs",
            "general.script_max_output_bytes",
            "general.listen_addr",
            "general.port",
            "general.data_dir",
        ];
        for key in sensitive {
            let entry = entries
                .iter()
                .find(|entry| entry["key"] == key)
                .unwrap_or_else(|| panic!("{key} missing"));
            assert_eq!(entry["class"], "sensitive", "{key}");
        }
        assert_eq!(
            entries
                .iter()
                .find(|entry| entry["key"] == "general.log_level")
                .unwrap()["class"],
            "normal"
        );
        assert!(entries.iter().any(|entry| entry["granular"] == false));
        assert!(entries.iter().any(|entry| entry["apply"] == "restart"));
    }

    #[test]
    fn validate_value_enforces_type_and_range() {
        let port = Kind::U16 { min: 1, max: 65535 };
        assert!(validate_value(&port, &serde_json::json!(8085)).is_ok());
        assert!(validate_value(&port, &serde_json::json!(0)).is_err());
        assert!(validate_value(&port, &serde_json::json!("nope")).is_err());
        let flag = Kind::Bool;
        assert!(validate_value(&flag, &serde_json::json!(true)).is_ok());
        let name = Kind::String {
            optional: false,
            max_len: Some(4),
        };
        assert!(validate_value(&name, &serde_json::json!("abcd")).is_ok());
        assert!(validate_value(&name, &serde_json::json!("abcde")).is_err());
        let secret = Kind::Secret { optional: true };
        assert_eq!(
            validate_value(&secret, &serde_json::Value::Null).unwrap(),
            serde_json::Value::Null
        );
    }
}
