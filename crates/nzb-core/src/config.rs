use std::path::PathBuf;

use anyhow::Context;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Top-level application configuration.
///
/// Unknown keys are rejected at parse time (`deny_unknown_fields`), so a typo
/// in `config.toml` fails loudly instead of silently keeping the default.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(default, deny_unknown_fields)]
pub struct AppConfig {
    /// Schema version of the file, stamped on every save. A file without it
    /// is version 0 and is migrated on load (see `config_store::migrate`).
    #[serde(default)]
    pub config_version: u32,
    pub general: GeneralConfig,
    pub servers: Vec<ServerConfig>,
    pub categories: Vec<CategoryConfig>,
    #[serde(default)]
    pub otel: OtelConfig,
    #[serde(default)]
    pub rss_feeds: Vec<RssFeedConfig>,
    #[serde(default)]
    pub dav: DavConfig,
    #[serde(default)]
    pub notifications: NotificationConfig,
    #[serde(default)]
    pub par2: Par2Config,
    #[serde(default)]
    pub schedule: crate::schedule::ScheduleConfig,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            config_version: crate::config_store::CONFIG_VERSION,
            general: GeneralConfig::default(),
            servers: Vec::new(),
            categories: vec![CategoryConfig::default()],
            otel: OtelConfig::default(),
            rss_feeds: Vec::new(),
            dav: DavConfig::default(),
            notifications: NotificationConfig::default(),
            par2: Par2Config::default(),
            schedule: crate::schedule::ScheduleConfig::default(),
        }
    }
}

/// PAR2 handling during download.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(default, deny_unknown_fields)]
pub struct Par2Config {
    /// Hold PAR2 recovery volumes until the in-flight slice map shows
    /// damage, then fetch only as many as the damage needs. Off fetches every
    /// volume up front.
    pub defer_volumes: bool,
    /// Accept a PAR2 set as verified from the in-flight slice map when it
    /// covers every file, skipping the disk verify pass. Off always verifies
    /// from disk.
    pub trust_inflight_verify: bool,
}

impl Default for Par2Config {
    fn default() -> Self {
        Self {
            defer_volumes: true,
            trust_inflight_verify: true,
        }
    }
}

/// What admission does when an incoming NZB matches a queued or history job
/// by name or by content digest.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum DuplicatePolicy {
    /// Admit the NZB without checking for a match.
    Allow,
    /// Admit the NZB and record the match in the log (default).
    #[default]
    Warn,
    /// Refuse the NZB; the existing job is left as it is.
    Skip,
}

impl std::fmt::Display for DuplicatePolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Allow => "allow",
            Self::Warn => "warn",
            Self::Skip => "skip",
        })
    }
}

impl std::str::FromStr for DuplicatePolicy {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "allow" => Ok(Self::Allow),
            "warn" => Ok(Self::Warn),
            "skip" => Ok(Self::Skip),
            other => Err(format!(
                "duplicate_policy must be allow, warn or skip, not `{other}`"
            )),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(default, deny_unknown_fields)]
pub struct GeneralConfig {
    /// HTTP API listen address
    pub listen_addr: String,
    /// HTTP API port
    pub port: u16,
    /// API key for authentication
    pub api_key: Option<String>,
    /// Directory for incomplete downloads
    #[schema(value_type = String)]
    pub incomplete_dir: PathBuf,
    /// Directory for completed downloads
    #[schema(value_type = String)]
    pub complete_dir: PathBuf,
    /// Directory for application data (DB, logs)
    #[schema(value_type = String)]
    pub data_dir: PathBuf,
    /// Download speed limit in bytes/sec (0 = unlimited)
    pub speed_limit_bps: u64,
    /// Article cache size in bytes
    pub cache_size: u64,
    /// Log level
    pub log_level: String,
    /// The level the debug-logging toggle replaced, restored when it is
    /// turned off again. Unset while the toggle is off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log_level_before_debug: Option<String>,
    /// Log file path (None = stdout only)
    #[schema(value_type = Option<String>)]
    pub log_file: Option<PathBuf>,
    /// History retention: how many NZBs to keep in history.
    /// `None` or `Some(0)` both mean keep all; see [`normalize_history_retention`].
    pub history_retention: Option<usize>,
    /// Keep the History row when an *arr removes a job through the SABnzbd
    /// API (the files are still deleted as asked). Off restores SABnzbd's
    /// behaviour of dropping the row too.
    pub keep_history_on_arr_delete: bool,
    /// Max number of NZBs downloading simultaneously (default 1).
    /// `0` means unlimited: every queued job may download at once.
    pub max_active_downloads: usize,
    /// Max number of independent jobs in post-processing at once.
    #[serde(default = "default_max_post_processing_jobs")]
    pub max_post_processing_jobs: usize,
    /// Max number of concurrent PAR2 verify/repair workers.
    #[serde(default = "default_max_repair_workers")]
    pub max_repair_workers: usize,
    /// Max number of concurrent archive extraction workers.
    #[serde(default = "default_max_extract_workers")]
    pub max_extract_workers: usize,
    /// Minimum free disk space in bytes before pausing downloads (default 1 GB)
    #[serde(default = "default_min_free_space")]
    pub min_free_space_bytes: u64,
    /// Directory to watch for new .nzb files to auto-enqueue
    #[schema(value_type = Option<String>)]
    pub watch_dir: Option<PathBuf>,
    /// RSS feed history limit: how many feed items to keep (None = keep all, default 500)
    #[serde(default = "default_rss_history_limit")]
    pub rss_history_limit: Option<usize>,
    /// Begin extracting RAR volumes during download instead of waiting for the
    /// job to complete. Requires `unrar` on PATH. Falls back to normal
    /// post-processing if articles fail or unrar is unavailable.
    #[serde(default = "default_true")]
    pub direct_unpack: bool,
    /// Maximum number of archive layers to extract after the outer archive.
    /// `0` extracts only the outer archive; the default of `5` permits five
    /// nested layers while preventing unbounded recursive extraction.
    #[serde(default = "default_max_nested_archive_depth")]
    pub max_nested_archive_depth: u8,
    /// Abort downloads that cannot possibly complete (too many missing articles).
    /// When enabled, the engine checks article failure rates and cancels jobs
    /// that have no chance of success. Default: true.
    #[serde(default = "default_true")]
    pub abort_hopeless: bool,
    /// Quick initial check: after the first N articles have been attempted,
    /// abort if the failure rate exceeds 80%. Catches completely dead NZBs
    /// within seconds instead of grinding through thousands of articles.
    /// Requires `abort_hopeless` to also be enabled. Default: true.
    #[serde(default = "default_true")]
    pub early_failure_check: bool,
    /// Minimum effective completion percentage required to keep downloading.
    /// Effective completion includes available content plus usable PAR2
    /// recovery capacity; values above 100 reserve a repair safety margin.
    /// Range: 100.0–200.0. Default: 100.2.
    #[serde(default = "default_required_completion_pct")]
    pub required_completion_pct: f64,
    /// Maximum time in seconds to wait for a single NNTP article response
    /// before treating the connection as stalled and reconnecting.
    /// 0 = no timeout. Default: 30.
    #[serde(default = "default_article_timeout_secs")]
    pub article_timeout_secs: u64,
    /// Sort queued jobs by remaining percentage whenever progress changes.
    #[serde(default)]
    pub auto_sort_remaining_pct: bool,
    /// Remove downloaded RSS records after this many days. None keeps them.
    #[serde(default)]
    pub rss_downloaded_item_expiry_days: Option<u64>,
    /// What to do when an incoming NZB matches a queued or history job by
    /// name or content digest: `allow`, `warn` (default) or `skip`.
    #[serde(default)]
    pub duplicate_policy: DuplicatePolicy,
    /// Directory that SABnzbd `addlocalfile` may read NZBs from. Unset refuses
    /// the mode; a file outside this directory (after resolving symlinks and
    /// `..`) is refused too.
    #[serde(default)]
    #[schema(value_type = Option<String>)]
    pub local_import_dir: Option<PathBuf>,
    /// Keep the SABnzbd `change_complete_action` after it has run once. When
    /// false (SABnzbd's default) the action is reset after each run.
    #[serde(default)]
    pub queue_complete_pers: bool,
    /// Optional directory containing post-processing scripts.
    #[serde(default)]
    #[schema(value_type = Option<String>)]
    pub scripts_dir: Option<PathBuf>,
    /// Script run after a successful post-processing job.
    #[serde(default)]
    #[schema(value_type = Option<String>)]
    pub script_success: Option<PathBuf>,
    /// Script run after a failed post-processing job.
    #[serde(default)]
    #[schema(value_type = Option<String>)]
    pub script_failure: Option<PathBuf>,
    /// Maximum runtime for a post-processing script.
    #[serde(default = "default_script_timeout_secs")]
    pub script_timeout_secs: u64,
    /// Maximum captured output retained from a post-processing script.
    #[serde(default = "default_script_output_bytes")]
    pub script_max_output_bytes: usize,
    /// Allow server-side URL fetches (add-url, SABnzbd `addurl`, RSS feeds,
    /// SABnzbd import) to reach private LAN addresses: RFC 1918,
    /// carrier-grade NAT (100.64.0.0/10) and IPv6 unique-local. Loopback and
    /// link-local addresses (127.0.0.1, ::1, the 169.254.169.254
    /// cloud-metadata endpoint) stay blocked unless listed in
    /// `fetch_allowed_hosts`. Default: false.
    #[serde(default)]
    pub fetch_allow_private: bool,
    /// Hosts that server-side URL fetches may always reach, even when they
    /// resolve to a non-public address. Each entry is an IP address, a CIDR
    /// block (`172.16.0.0/12`, `fd00::/8`) or a hostname matched exactly
    /// against the URL host (`prowlarr`, `nzbhydra.lan`). Default: empty.
    #[serde(default)]
    pub fetch_allowed_hosts: Vec<String>,
    /// Most headers kept per group when fetching a group's headers; the
    /// oldest are pruned first. 0 keeps all fetched headers. Default: 100000.
    #[serde(default = "default_group_max_headers")]
    pub group_max_headers: usize,
    /// Seconds to wait on shutdown for open HTTP connections and in-flight
    /// downloads to finish before the process exits anyway. 0 waits forever.
    /// Default: 30.
    #[serde(default = "default_shutdown_timeout_secs")]
    pub shutdown_timeout_secs: u64,
    /// Reverse proxies whose `X-Forwarded-For` header is believed, as IP
    /// addresses or CIDR blocks. Wrong web-login passwords are delayed per
    /// client address; behind a proxy every request comes from the proxy, so
    /// list it here to count the real clients separately. Empty (the default)
    /// ignores `X-Forwarded-For`.
    #[serde(default)]
    pub trusted_proxies: Vec<String>,
    /// How long a confirmed password authorises further sensitive writes,
    /// in seconds. Default 300 (five minutes). `0` means every sensitive
    /// write must present the password again.
    #[serde(default = "default_step_up_ttl_secs")]
    pub step_up_ttl_secs: u64,
}

/// Parse `addr` or `addr/prefix` (IPv4 or IPv6, optionally in brackets) into
/// a network address and prefix length. `None` if it is not one.
pub fn parse_ip_network(entry: &str) -> Option<(std::net::IpAddr, u8)> {
    let (addr, prefix) = match entry.split_once('/') {
        Some((addr, prefix)) => (addr, Some(prefix)),
        None => (entry, None),
    };
    let addr = addr.trim_start_matches('[').trim_end_matches(']');
    let ip: std::net::IpAddr = addr.parse().ok()?;
    let max = if ip.is_ipv4() { 32 } else { 128 };
    let prefix = match prefix {
        Some(p) => p.parse::<u8>().ok().filter(|p| *p <= max)?,
        None => max,
    };
    Some((ip, prefix))
}

fn default_shutdown_timeout_secs() -> u64 {
    30
}

fn default_step_up_ttl_secs() -> u64 {
    300
}

fn default_group_max_headers() -> usize {
    100_000
}

fn default_rss_history_limit() -> Option<usize> {
    Some(500)
}

fn default_min_free_space() -> u64 {
    1_073_741_824 // 1 GB
}

fn default_max_post_processing_jobs() -> usize {
    2
}

fn default_max_repair_workers() -> usize {
    1
}

fn default_max_extract_workers() -> usize {
    1
}

fn default_required_completion_pct() -> f64 {
    100.2
}

fn default_max_nested_archive_depth() -> u8 {
    5
}

fn default_article_timeout_secs() -> u64 {
    30
}

/// Normalize a history retention limit so that `0` means "keep all".
///
/// SABnzbd users (and this codebase's own `speed_limit_bps`) treat `0` as
/// unlimited. Enforcing a literal limit of zero would delete every history
/// row immediately after each completion (GH #136), so a zero is folded into
/// `None` at every entry point before it reaches the database.
pub fn normalize_history_retention(limit: Option<usize>) -> Option<usize> {
    limit.filter(|max| *max > 0)
}

/// Upper bound for `rss_downloaded_item_expiry_days`: 100 years.
///
/// The value is subtracted from `Utc::now()` when downloaded RSS items are
/// pruned; a large enough span overflows chrono's `DateTime` range (about
/// ±262 000 years) and panics. A hundred years is already far past any useful
/// retention.
pub const MAX_RSS_EXPIRY_DAYS: u64 = 36_500;

/// Clamp a configured RSS expiry so the cutoff can always be computed.
pub fn normalize_rss_expiry_days(days: Option<u64>) -> Option<u64> {
    days.map(|days| days.min(MAX_RSS_EXPIRY_DAYS))
}

fn default_script_timeout_secs() -> u64 {
    300
}

fn default_script_output_bytes() -> usize {
    1024 * 1024
}

impl Default for GeneralConfig {
    fn default() -> Self {
        Self {
            listen_addr: "0.0.0.0".into(),
            port: 9090,
            step_up_ttl_secs: default_step_up_ttl_secs(),
            api_key: None,
            incomplete_dir: PathBuf::from("/downloads/incomplete"),
            complete_dir: PathBuf::from("/downloads/complete"),
            data_dir: PathBuf::from("/data"),
            speed_limit_bps: 0,
            cache_size: 500 * 1024 * 1024, // 500 MB
            log_level: "info".into(),
            log_level_before_debug: None,
            log_file: None,
            history_retention: None, // keep all
            keep_history_on_arr_delete: true,
            max_active_downloads: 1,
            max_post_processing_jobs: default_max_post_processing_jobs(),
            max_repair_workers: default_max_repair_workers(),
            max_extract_workers: default_max_extract_workers(),
            min_free_space_bytes: default_min_free_space(),
            watch_dir: None,
            rss_history_limit: default_rss_history_limit(),
            direct_unpack: true,
            max_nested_archive_depth: default_max_nested_archive_depth(),
            abort_hopeless: true,
            early_failure_check: true,
            required_completion_pct: default_required_completion_pct(),
            article_timeout_secs: default_article_timeout_secs(),
            auto_sort_remaining_pct: false,
            duplicate_policy: DuplicatePolicy::Warn,
            rss_downloaded_item_expiry_days: None,
            local_import_dir: None,
            queue_complete_pers: false,
            scripts_dir: None,
            script_success: None,
            script_failure: None,
            script_timeout_secs: default_script_timeout_secs(),
            script_max_output_bytes: default_script_output_bytes(),
            fetch_allow_private: false,
            fetch_allowed_hosts: Vec::new(),
            group_max_headers: default_group_max_headers(),
            shutdown_timeout_secs: default_shutdown_timeout_secs(),
            trusted_proxies: Vec::new(),
        }
    }
}

/// OpenTelemetry configuration. All values can be overridden via env vars.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(default, deny_unknown_fields)]
pub struct OtelConfig {
    /// Enable both OTEL logs and metrics. Retained as a shared fallback for
    /// existing configs; prefer the per-signal fields below for new setups.
    pub enabled: bool,
    /// Shared OTLP endpoint for both logs and metrics when the signal-specific
    /// endpoints below are unset.
    pub endpoint: String,
    /// Override for OTEL log export enablement.
    pub logs_enabled: Option<bool>,
    /// OTLP endpoint for log export.
    pub logs_endpoint: Option<String>,
    /// Override for OTEL metrics export enablement.
    pub metrics_enabled: Option<bool>,
    /// OTLP endpoint for metrics export.
    pub metrics_endpoint: Option<String>,
    /// Service name reported to the collector
    pub service_name: String,
}

impl Default for OtelConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            endpoint: "http://localhost:4317".into(),
            logs_enabled: None,
            logs_endpoint: None,
            metrics_enabled: None,
            metrics_endpoint: None,
            service_name: "rustnzb".into(),
        }
    }
}

impl OtelConfig {
    pub fn logs_enabled(&self) -> bool {
        self.logs_enabled.unwrap_or(self.enabled)
    }

    pub fn metrics_enabled(&self) -> bool {
        self.metrics_enabled.unwrap_or(self.enabled)
    }

    pub fn logs_endpoint(&self) -> &str {
        self.logs_endpoint.as_deref().unwrap_or(&self.endpoint)
    }

    pub fn metrics_endpoint(&self) -> &str {
        self.metrics_endpoint.as_deref().unwrap_or(&self.endpoint)
    }
}

/// NNTP server configuration — re-exported from the `nzb-nntp` crate.
pub use nzb_nntp::ServerConfig;

/// Category configuration for organizing downloads.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct CategoryConfig {
    /// Category name
    pub name: String,
    /// Output directory override (relative to complete_dir)
    #[schema(value_type = Option<String>)]
    pub output_dir: Option<PathBuf>,
    /// Post-processing level: 0=none, 1=repair, 2=unpack, 3=repair+unpack
    pub post_processing: u8,
    /// Filename or relative-path glob patterns to remove after unpacking.
    #[serde(default)]
    pub cleanup_patterns: Vec<String>,
    /// Extensions to remove after unpacking, including or omitting the dot.
    #[serde(default)]
    pub unwanted_extensions: Vec<String>,
}

impl Default for CategoryConfig {
    fn default() -> Self {
        Self {
            name: "Default".into(),
            output_dir: None,
            post_processing: 3,
            cleanup_patterns: Vec::new(),
            unwanted_extensions: Vec::new(),
        }
    }
}

impl CategoryConfig {
    /// Check the name and `output_dir` against the rules
    /// `QueueManager::output_dir_for` applies when a job is enqueued, so an
    /// entry that would make every job in the category fail can be refused
    /// when it is saved. Returns a message suitable for an API client.
    pub fn validate(&self) -> std::result::Result<(), &'static str> {
        if crate::path::safe_component(&self.name).is_none() {
            return Err("category name must be a single non-empty path component \
                 without '/', '\\', '..' or control characters");
        }
        if let Some(output_dir) = &self.output_dir {
            // Only the shape of the override matters here, not where the
            // complete directory lives.
            if crate::path::category_output_root(std::path::Path::new("/"), output_dir).is_none() {
                return Err(
                    "category output_dir must be an absolute path, or a relative \
                     path below the complete directory, without '..' or control characters",
                );
            }
        }
        Ok(())
    }
}

/// RSS feed configuration for automatic NZB downloading.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct RssFeedConfig {
    /// Display name for the feed
    pub name: String,
    /// Feed URL (RSS 2.0 or Atom)
    pub url: String,
    /// How often to poll, in seconds (default 900 = 15 minutes, minimum 60;
    /// lower values are rejected by the API and clamped by the RSS monitor)
    #[serde(default = "default_poll_interval")]
    pub poll_interval_secs: u64,
    /// Category to assign to downloaded NZBs
    #[serde(default)]
    pub category: Option<String>,
    /// Regex pattern to filter feed entries by title (Rust `regex` syntax,
    /// e.g. `(?i)ubuntu|debian`; no `/.../flags` delimiters)
    #[serde(default)]
    pub filter_regex: Option<String>,
    /// Whether this feed is active
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Auto-download items from this feed without needing a rule: every item
    /// that passes `filter_regex`, or every item when no filter is set.
    #[serde(default)]
    pub auto_download: bool,
    /// Ignore entries older than this many days. None disables age filtering.
    #[serde(default)]
    pub max_age_days: Option<u64>,
}

fn default_poll_interval() -> u64 {
    900
}

fn default_true() -> bool {
    true
}

/// Outbound notifications for completed, failed and disk-full events.
///
/// Each target is off until its URL or topic is set. Targets are read at
/// startup, so a change takes effect after a restart.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
#[serde(default)]
pub struct NotificationConfig {
    /// Generic webhook: each event is POSTed as a JSON document.
    pub webhook_url: String,
    /// Apprise API notify endpoint, for example `http://apprise:8000/notify`
    /// or `http://apprise:8000/notify/<key>`. Empty disables Apprise.
    pub apprise_api_url: String,
    /// Apprise notification URLs (comma-separated) sent with each message.
    /// Leave empty when the endpoint already carries a configuration key.
    pub apprise_urls: String,
    /// ntfy server base URL.
    pub ntfy_server: String,
    /// ntfy topic to publish to. Empty disables ntfy.
    pub ntfy_topic: String,
    /// Optional ntfy access token, sent as a bearer token.
    pub ntfy_token: String,
}

impl Default for NotificationConfig {
    fn default() -> Self {
        Self {
            webhook_url: String::new(),
            apprise_api_url: String::new(),
            apprise_urls: String::new(),
            ntfy_server: "https://ntfy.sh".to_string(),
            ntfy_topic: String::new(),
            ntfy_token: String::new(),
        }
    }
}

/// DAV pipeline auto-send configuration.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(default, deny_unknown_fields)]
pub struct DavConfig {
    /// Whether the WebDAV media library is mounted at runtime.
    pub enabled: bool,
    /// Automatically queue every completed download into the DAV pipeline.
    /// When true, `category_rules` is ignored.
    pub auto_send_all: bool,
    /// Categories whose completed downloads are automatically sent to DAV.
    /// Only used when `auto_send_all` is false.
    #[serde(default)]
    pub category_rules: Vec<String>,
    /// Basic-auth username for /dav. When all three credential fields are
    /// unset, /dav is served unauthenticated.
    #[serde(default)]
    pub username: Option<String>,
    /// Basic-auth password for /dav.
    #[serde(default)]
    pub password: Option<String>,
    /// X-Api-Key for /dav (alternative to Basic auth — useful for tools that
    /// don't support Basic but accept a custom header).
    #[serde(default)]
    pub api_key: Option<String>,
    /// Percent of each server's `connections` set aside for WebDAV playback
    /// and import while the library is enabled (rounded up; the downloader
    /// always keeps at least one). The downloader and WebDAV share one
    /// allowance per server, so this is what keeps playback responsive
    /// during a download. Playback may borrow more while downloads are idle.
    pub playback_reserve_pct: u8,
}

/// Default for [`DavConfig::playback_reserve_pct`].
pub const DEFAULT_DAV_PLAYBACK_RESERVE_PCT: u8 = 25;

impl Default for DavConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            auto_send_all: false,
            category_rules: Vec::new(),
            username: None,
            password: None,
            api_key: None,
            playback_reserve_pct: DEFAULT_DAV_PLAYBACK_RESERVE_PCT,
        }
    }
}

impl AppConfig {
    /// The config path used when none is given: an absolute path under the
    /// user's config directory (`$XDG_CONFIG_HOME/rustnzb/config.toml`, else
    /// `~/.config/rustnzb/config.toml`). Falls back to `config.toml` in the
    /// current directory, made absolute, when neither variable is usable.
    pub fn default_path() -> PathBuf {
        let xdg = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .filter(|dir| dir.is_absolute());
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .filter(|dir| dir.is_absolute())
            .map(|dir| dir.join(".config"));
        match xdg.or(home) {
            Some(dir) => dir.join("rustnzb").join("config.toml"),
            None => std::path::absolute("config.toml").unwrap_or_else(|_| "config.toml".into()),
        }
    }

    /// Read and parse an existing config file. Unknown keys and malformed
    /// values fail here, and the error names the file. Does not create the
    /// file and does not validate values; see [`AppConfig::validate`].
    pub fn read(path: &std::path::Path) -> anyhow::Result<Self> {
        let contents = std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read config file: {}", path.display()))?;
        let mut config: AppConfig = toml::from_str(&contents)
            .with_context(|| format!("Failed to parse config file: {}", path.display()))?;
        config.assign_missing_server_ids();
        config.general.rss_downloaded_item_expiry_days =
            normalize_rss_expiry_days(config.general.rss_downloaded_item_expiry_days);
        Ok(config)
    }

    /// Check values that the parser cannot: each returned string is one
    /// problem, phrased with the setting's path. An empty list means the
    /// config can run. Category rules are checked separately, by
    /// [`CategoryConfig::validate`], because startup only warns about them.
    pub fn validate(&self) -> Vec<String> {
        let mut problems = Vec::new();
        if self.general.port == 0 {
            problems.push("general.port must be between 1 and 65535".to_string());
        }
        if self.general.listen_addr.trim().is_empty() {
            problems.push("general.listen_addr must not be empty".to_string());
        }
        for (i, entry) in self.general.trusted_proxies.iter().enumerate() {
            if parse_ip_network(entry.trim()).is_none() {
                problems.push(format!(
                    "general.trusted_proxies[{i}] ({entry}) must be an IP address or CIDR block"
                ));
            }
        }
        let mut seen_ids = std::collections::HashSet::new();
        for (i, server) in self.servers.iter().enumerate() {
            let label = format!("servers[{i}] ({})", server.id);
            if server.host.trim().is_empty() {
                problems.push(format!("{label}: host must not be empty"));
            }
            if server.port == 0 {
                problems.push(format!("{label}: port must be between 1 and 65535"));
            }
            if server.connections == 0 {
                problems.push(format!("{label}: connections must be at least 1"));
            }
            if !seen_ids.insert(server.id.as_str()) {
                problems.push(format!("{label}: duplicate server id"));
            }
        }
        if self.dav.playback_reserve_pct > 100 {
            problems.push("dav.playback_reserve_pct must be between 0 and 100".to_string());
        }
        for (i, rule) in self.schedule.rules.iter().enumerate() {
            if let Some(problem) = crate::schedule::rule_problem(rule) {
                problems.push(format!("schedule.rules[{i}] ({}): {problem}", rule.id));
            }
        }
        for (i, feed) in self.rss_feeds.iter().enumerate() {
            if let Some(pattern) = feed.filter_regex.as_deref()
                && let Err(e) = regex::Regex::new(pattern)
            {
                problems.push(format!(
                    "rss_feeds[{i}] ({}): filter_regex does not compile: {e}",
                    feed.name
                ));
            }
        }
        problems
    }

    /// Load config from a TOML file, creating default if it doesn't exist.
    pub fn load(path: &std::path::Path) -> anyhow::Result<Self> {
        if path.exists() {
            let mut config = Self::read(path)?;
            if config.config_version < crate::config_store::CONFIG_VERSION {
                // Saving backs up the old file first. A read-only file still
                // loads; the upgrade is then applied on the next save.
                crate::config_store::migrate(&mut config);
                if let Err(e) = config.save(path) {
                    tracing::warn!(error = %e, "could not write upgraded config file");
                }
            }
            Ok(config)
        } else {
            let config = AppConfig::default();
            config.save(path).with_context(|| {
                format!(
                    "Failed to create default config at {}. \
                     Check that the directory is writable by the current user. \
                     If using Docker with 'user:', ensure volume directories are owned by that user.",
                    path.display()
                )
            })?;
            Ok(config)
        }
    }

    /// Give every server without an `id` a stable one derived from its host
    /// and port, so a hand-written `[[servers]]` block may omit it.
    pub fn assign_missing_server_ids(&mut self) {
        for i in 0..self.servers.len() {
            if !self.servers[i].id.trim().is_empty() {
                continue;
            }
            let base = format!("{}-{}", self.servers[i].host.trim(), self.servers[i].port);
            let mut candidate = base.clone();
            let mut n = 2;
            while self.servers.iter().any(|s| s.id == candidate) {
                candidate = format!("{base}-{n}");
                n += 1;
            }
            self.servers[i].id = candidate;
        }
    }

    /// Save this config as the file layer of `path`.
    ///
    /// Comments and formatting already in the file are kept, the write is
    /// atomic and private, and the previous file is backed up. See
    /// [`crate::config_store::write_file_layer`].
    pub fn save(&self, path: &std::path::Path) -> anyhow::Result<()> {
        crate::config_store::write_file_layer(self, path)
    }

    /// Find a category by name.
    pub fn category(&self, name: &str) -> Option<&CategoryConfig> {
        self.categories.iter().find(|c| c.name == name)
    }

    /// Find a category by name, falling back to the first configured category if unknown.
    /// Callers can rely on the result always being valid — the default config always has
    /// at least one category ("Default").
    pub fn find_category_or_default(&self, name: &str) -> &CategoryConfig {
        self.categories
            .iter()
            .find(|c| c.name == name)
            .or_else(|| self.categories.first())
            .expect("AppConfig always has at least one category")
    }

    /// Find a server by ID.
    pub fn server(&self, id: &str) -> Option<&ServerConfig> {
        self.servers.iter().find(|s| s.id == id)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn par2_trust_inflight_verify_defaults_on_and_can_be_disabled() {
        assert!(Par2Config::default().trust_inflight_verify);
        let off: Par2Config = toml::from_str("trust_inflight_verify = false").unwrap();
        assert!(!off.trust_inflight_verify);
        assert!(off.defer_volumes);
    }

    use super::*;

    #[test]
    fn test_server_config_defaults() {
        let cfg = ServerConfig::default();
        assert_eq!(cfg.port, 563);
        assert!(cfg.ssl);
        assert!(cfg.ssl_verify);
        assert!(cfg.username.is_none());
        assert!(cfg.password.is_none());
        assert_eq!(cfg.connections, 8);
        assert_eq!(cfg.priority, 0);
        assert!(cfg.enabled);
        assert_eq!(cfg.retention, 0);
        assert_eq!(cfg.pipelining, 4);
        assert!(!cfg.optional);
    }

    #[test]
    fn config_with_minimal_server_block_loads() {
        // `retention`, `ssl_verify`, `id` etc. omitted: used to fail startup
        // with "missing field `retention`".
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.toml");
        std::fs::write(
            &path,
            r#"
[[servers]]
name = "Primary"
host = "news.example.com"

[[servers]]
host = "news.example.com"

[[servers]]
id = "backup"
host = "backup.example.com"
port = 119
ssl = false
"#,
        )
        .unwrap();
        let config = AppConfig::load(&path).unwrap();
        assert_eq!(config.servers.len(), 3);
        assert_eq!(config.servers[0].id, "news.example.com-563");
        assert_eq!(config.servers[1].id, "news.example.com-563-2");
        assert_eq!(config.servers[2].id, "backup");
        assert!(config.servers[0].ssl_verify);
        assert!(config.servers[0].enabled);
        assert_eq!(config.servers[0].retention, 0);
        assert_eq!(config.servers[2].port, 119);
    }

    #[test]
    fn test_general_config_defaults() {
        let cfg = GeneralConfig::default();
        assert_eq!(cfg.listen_addr, "0.0.0.0");
        assert_eq!(cfg.port, 9090);
        assert!(cfg.api_key.is_none());
        assert_eq!(cfg.speed_limit_bps, 0);
        assert_eq!(cfg.cache_size, 500 * 1024 * 1024);
        assert_eq!(cfg.log_level, "info");
        assert!(cfg.log_file.is_none());
        assert!(cfg.history_retention.is_none());
        assert_eq!(cfg.max_active_downloads, 1);
        assert_eq!(cfg.max_post_processing_jobs, 2);
        assert_eq!(cfg.max_repair_workers, 1);
        assert_eq!(cfg.max_extract_workers, 1);
        assert_eq!(cfg.min_free_space_bytes, 1_073_741_824);
        assert!(cfg.watch_dir.is_none());
        assert_eq!(cfg.rss_history_limit, Some(500));
        assert!(cfg.direct_unpack);
        assert_eq!(cfg.max_nested_archive_depth, 5);
    }

    #[test]
    fn duplicate_policy_defaults_to_warn_and_parses_known_values() {
        assert_eq!(
            GeneralConfig::default().duplicate_policy,
            DuplicatePolicy::Warn
        );
        assert_eq!("Skip".parse::<DuplicatePolicy>(), Ok(DuplicatePolicy::Skip));
        assert_eq!(
            " allow ".parse::<DuplicatePolicy>(),
            Ok(DuplicatePolicy::Allow)
        );
        assert!("ignore".parse::<DuplicatePolicy>().is_err());

        let general: GeneralConfig = toml::from_str("duplicate_policy = \"skip\"").unwrap();
        assert_eq!(general.duplicate_policy, DuplicatePolicy::Skip);
        let omitted: GeneralConfig = toml::from_str("").unwrap();
        assert_eq!(omitted.duplicate_policy, DuplicatePolicy::Warn);
    }

    #[test]
    fn rss_expiry_is_clamped_to_a_computable_cutoff() {
        assert_eq!(normalize_rss_expiry_days(None), None);
        assert_eq!(normalize_rss_expiry_days(Some(0)), Some(0));
        assert_eq!(normalize_rss_expiry_days(Some(30)), Some(30));
        assert_eq!(
            normalize_rss_expiry_days(Some(u64::MAX)),
            Some(MAX_RSS_EXPIRY_DAYS)
        );
    }

    #[test]
    fn read_clamps_an_absurd_rss_expiry() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            format!(
                "[general]\nrss_downloaded_item_expiry_days = {}\n",
                u64::MAX
            ),
        )
        .unwrap();

        let config = AppConfig::read(&path).unwrap();
        assert_eq!(
            config.general.rss_downloaded_item_expiry_days,
            Some(MAX_RSS_EXPIRY_DAYS)
        );
    }

    #[test]
    fn zero_history_retention_normalizes_to_keep_all() {
        assert_eq!(normalize_history_retention(Some(0)), None);
        assert_eq!(normalize_history_retention(None), None);
        assert_eq!(normalize_history_retention(Some(25)), Some(25));
    }

    #[test]
    fn direct_unpack_can_be_explicitly_disabled() {
        let cfg: GeneralConfig = toml::from_str("direct_unpack = false").unwrap();
        assert!(!cfg.direct_unpack);
    }

    #[test]
    fn nested_archive_depth_can_be_configured() {
        let cfg: GeneralConfig = toml::from_str("max_nested_archive_depth = 2").unwrap();
        assert_eq!(cfg.max_nested_archive_depth, 2);
    }

    #[test]
    fn test_app_config_defaults() {
        let cfg = AppConfig::default();
        assert!(cfg.servers.is_empty());
        assert_eq!(cfg.categories.len(), 1);
        assert_eq!(cfg.categories[0].name, "Default");
        assert_eq!(cfg.categories[0].post_processing, 3);
        assert!(!cfg.otel.enabled);
        assert!(!cfg.otel.logs_enabled());
        assert!(!cfg.otel.metrics_enabled());
        assert_eq!(cfg.otel.logs_endpoint(), "http://localhost:4317");
        assert_eq!(cfg.otel.metrics_endpoint(), "http://localhost:4317");
        assert!(cfg.rss_feeds.is_empty());
    }

    #[test]
    fn test_category_config_defaults() {
        let cat = CategoryConfig::default();
        assert_eq!(cat.name, "Default");
        assert!(cat.output_dir.is_none());
        assert_eq!(cat.post_processing, 3);
    }

    #[test]
    fn test_server_config_toml_roundtrip() {
        let mut original = ServerConfig::new("srv-1", "news.example.com");
        original.name = "Usenet Provider".into();
        original.username = Some("user".into());
        original.password = Some("pass".into());
        original.connections = 20;
        original.retention = 3000;
        original.pipelining = 5;
        original.ramp_up_delay_ms = 0;
        original.recv_buffer_size = 0;

        let toml_str = toml::to_string_pretty(&original).unwrap();
        let restored: ServerConfig = toml::from_str(&toml_str).unwrap();

        assert_eq!(restored.id, original.id);
        assert_eq!(restored.name, original.name);
        assert_eq!(restored.host, original.host);
        assert_eq!(restored.port, original.port);
        assert_eq!(restored.ssl, original.ssl);
        assert_eq!(restored.username, original.username);
        assert_eq!(restored.password, original.password);
        assert_eq!(restored.connections, original.connections);
        assert_eq!(restored.priority, original.priority);
        assert_eq!(restored.retention, original.retention);
        assert_eq!(restored.pipelining, original.pipelining);
        assert_eq!(restored.optional, original.optional);
    }

    #[test]
    fn test_app_config_toml_roundtrip() {
        let mut original = AppConfig::default();
        let mut srv = ServerConfig::new("test-srv", "news.test.com");
        srv.name = "Test".into();
        srv.port = 119;
        srv.ssl = false;
        srv.ssl_verify = false;
        srv.connections = 4;
        srv.priority = 1;
        srv.optional = true;
        srv.ramp_up_delay_ms = 0;
        srv.recv_buffer_size = 0;
        original.servers.push(srv);
        original.general.speed_limit_bps = 1_000_000;
        original.general.api_key = Some("secret-key".into());

        let toml_str = toml::to_string_pretty(&original).unwrap();
        let restored: AppConfig = toml::from_str(&toml_str).unwrap();

        assert_eq!(restored.servers.len(), 1);
        assert_eq!(restored.servers[0].host, "news.test.com");
        assert!(!restored.servers[0].ssl);
        assert!(restored.servers[0].optional);
        assert_eq!(restored.general.speed_limit_bps, 1_000_000);
        assert_eq!(restored.general.api_key.as_deref(), Some("secret-key"));
        assert_eq!(restored.categories.len(), 1);
    }

    #[test]
    fn test_config_save_and_load() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");

        let mut original = AppConfig::default();
        let mut srv = ServerConfig::new("file-srv", "news.file.com");
        srv.name = "File Test".into();
        original.servers.push(srv);
        original.general.port = 8888;

        original.save(&path).unwrap();
        assert!(path.exists());

        let loaded = AppConfig::load(&path).unwrap();
        assert_eq!(loaded.servers.len(), 1);
        assert_eq!(loaded.servers[0].id, "file-srv");
        assert_eq!(loaded.general.port, 8888);
    }

    #[test]
    fn test_config_load_creates_default_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nonexistent.toml");

        let config = AppConfig::load(&path).unwrap();
        assert!(config.servers.is_empty());
        // File should now exist with default config
        assert!(path.exists());
    }

    #[test]
    fn test_config_find_category() {
        let mut cfg = AppConfig::default();
        cfg.categories.push(CategoryConfig {
            name: "movies".into(),
            output_dir: Some("/movies".into()),
            post_processing: 3,
            ..CategoryConfig::default()
        });

        assert!(cfg.category("Default").is_some());
        assert!(cfg.category("movies").is_some());
        assert_eq!(cfg.category("movies").unwrap().post_processing, 3);
        assert!(cfg.category("nonexistent").is_none());
    }

    #[test]
    fn test_config_find_server() {
        let mut cfg = AppConfig::default();
        let mut srv = ServerConfig::new("primary", "news.primary.com");
        srv.name = "Primary".into();
        cfg.servers.push(srv);

        assert!(cfg.server("primary").is_some());
        assert_eq!(cfg.server("primary").unwrap().host, "news.primary.com");
        assert!(cfg.server("nonexistent").is_none());
    }

    #[test]
    fn category_validate_matches_enqueue_rules() {
        let category = |name: &str, output_dir: Option<&str>| CategoryConfig {
            name: name.into(),
            output_dir: output_dir.map(PathBuf::from),
            ..CategoryConfig::default()
        };
        assert!(CategoryConfig::default().validate().is_ok());
        assert!(category("*", None).validate().is_ok());
        assert!(category("tv", Some("shows/tv")).validate().is_ok());
        assert!(category("tv", Some("/srv/media/tv")).validate().is_ok());
        for bad in [
            category("", None),
            category("..", None),
            category("a/b", None),
            category("a\\b", None),
            category("a\tb", None),
            category("tv", Some("")),
            category("tv", Some("../../../etc")),
            category("tv", Some("/srv/../etc")),
        ] {
            assert!(bad.validate().is_err(), "{bad:?}");
        }
    }

    #[test]
    fn test_find_category_or_default() {
        let mut cfg = AppConfig::default();
        cfg.categories.push(CategoryConfig {
            name: "movies".into(),
            output_dir: None,
            post_processing: 3,
            ..CategoryConfig::default()
        });
        assert_eq!(cfg.find_category_or_default("movies").name, "movies");
        assert_eq!(cfg.find_category_or_default("unknown").name, "Default");
        assert_eq!(cfg.find_category_or_default("").name, "Default");
    }

    #[test]
    fn test_dav_config_defaults() {
        let cfg = AppConfig::default();
        assert!(!cfg.dav.auto_send_all);
        assert!(cfg.dav.category_rules.is_empty());
        assert!(cfg.dav.username.is_none());
        assert!(cfg.dav.password.is_none());
        assert!(cfg.dav.api_key.is_none());
        assert_eq!(
            cfg.dav.playback_reserve_pct,
            DEFAULT_DAV_PLAYBACK_RESERVE_PCT
        );
    }

    #[test]
    fn test_dav_config_roundtrip() {
        let mut cfg = AppConfig::default();
        cfg.dav.auto_send_all = false;
        cfg.dav.category_rules = vec!["movies".into(), "tv".into()];
        cfg.dav.username = Some("plex".into());
        cfg.dav.password = Some("hunter2".into());
        cfg.dav.api_key = Some("deadbeef".into());
        let toml_str = toml::to_string_pretty(&cfg).unwrap();
        let restored: AppConfig = toml::from_str(&toml_str).unwrap();
        assert!(!restored.dav.auto_send_all);
        assert_eq!(restored.dav.category_rules, vec!["movies", "tv"]);
        assert_eq!(restored.dav.username.as_deref(), Some("plex"));
        assert_eq!(restored.dav.password.as_deref(), Some("hunter2"));
        assert_eq!(restored.dav.api_key.as_deref(), Some("deadbeef"));
    }

    /// Configs from older versions of rustnzb (no DAV credential fields)
    /// must keep deserializing.
    #[test]
    fn test_dav_config_backward_compat() {
        let toml_str = r#"
            auto_send_all = true
            category_rules = ["movies"]
        "#;
        let dav: DavConfig = toml::from_str(toml_str).unwrap();
        assert!(dav.auto_send_all);
        assert_eq!(dav.category_rules, vec!["movies"]);
        assert!(dav.username.is_none());
        assert!(dav.password.is_none());
        assert!(dav.api_key.is_none());
        assert_eq!(dav.playback_reserve_pct, DEFAULT_DAV_PLAYBACK_RESERVE_PCT);
    }

    #[test]
    fn read_rejects_unknown_keys_and_names_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[general]\nprot = 9090\n").unwrap();

        let err = format!("{:#}", AppConfig::read(&path).unwrap_err());
        assert!(err.contains(&path.display().to_string()), "{err}");
        assert!(err.contains("unknown field `prot`"), "{err}");
    }

    #[test]
    fn read_does_not_create_a_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("absent.toml");

        assert!(AppConfig::read(&path).is_err());
        assert!(!path.exists());
    }

    #[test]
    fn default_config_and_example_config_validate_and_parse_strictly() {
        assert!(AppConfig::default().validate().is_empty());

        let example = include_str!("../../../apps/rustnzb/config.example.toml");
        let config: AppConfig = toml::from_str(example).unwrap();
        assert!(config.validate().is_empty(), "{:?}", config.validate());
    }

    #[test]
    fn validate_checks_each_trusted_proxy_entry() {
        let mut cfg = AppConfig::default();
        cfg.general.trusted_proxies = vec![
            "172.18.0.5".into(),
            "10.0.0.0/8".into(),
            "fd00::/8".into(),
            "caddy".into(),
            "10.0.0.0/33".into(),
        ];
        assert_eq!(
            cfg.validate(),
            vec![
                "general.trusted_proxies[3] (caddy) must be an IP address or CIDR block",
                "general.trusted_proxies[4] (10.0.0.0/33) must be an IP address or CIDR block",
            ]
        );
    }

    #[test]
    fn validate_reports_each_bad_value_with_its_path() {
        let mut cfg = AppConfig::default();
        cfg.general.port = 0;
        cfg.general.listen_addr = "  ".into();
        let mut first = ServerConfig::new("dup", "news.example.com");
        first.connections = 0;
        let mut second = ServerConfig::new("dup", "");
        second.port = 0;
        cfg.servers = vec![first, second];
        cfg.rss_feeds.push(RssFeedConfig {
            name: "Bad".into(),
            url: "https://example.com/rss".into(),
            poll_interval_secs: 900,
            category: None,
            filter_regex: Some("(unclosed".into()),
            enabled: true,
            auto_download: false,
            max_age_days: None,
        });

        let problems = cfg.validate();
        for expected in [
            "general.port",
            "general.listen_addr",
            "servers[0] (dup): connections",
            "servers[1] (dup): host",
            "servers[1] (dup): port",
            "servers[1] (dup): duplicate server id",
            "rss_feeds[0] (Bad): filter_regex",
        ] {
            assert!(
                problems.iter().any(|p| p.contains(expected)),
                "missing {expected:?} in {problems:?}"
            );
        }
    }

    #[test]
    fn validate_names_a_bad_schedule_rule_by_index_and_id() {
        let mut cfg = AppConfig::default();
        cfg.schedule.rules.push(crate::schedule::ScheduleRule {
            id: "quiet".into(),
            days: vec!["funday".into()],
            ..Default::default()
        });

        let problems = cfg.validate();
        assert!(
            problems
                .iter()
                .any(|p| p.starts_with("schedule.rules[0] (quiet):") && p.contains("funday")),
            "{problems:?}"
        );
    }

    #[test]
    fn default_path_is_absolute() {
        assert!(AppConfig::default_path().is_absolute());
    }

    #[cfg(unix)]
    #[test]
    fn save_is_owner_only_even_over_a_looser_existing_file() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        AppConfig::default().save(&path).unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn test_rss_feed_config_defaults() {
        let toml_str = r#"
            name = "Test Feed"
            url = "https://example.com/rss"
        "#;
        let feed: RssFeedConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(feed.name, "Test Feed");
        assert_eq!(feed.poll_interval_secs, 900);
        assert!(feed.enabled);
        assert!(!feed.auto_download);
        assert!(feed.category.is_none());
        assert!(feed.filter_regex.is_none());
    }
}
