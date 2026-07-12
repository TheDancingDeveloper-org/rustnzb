use std::path::PathBuf;

use anyhow::Context;
use serde::{Deserialize, Serialize};

/// Top-level application configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    pub general: GeneralConfig,
    pub servers: Vec<ServerConfig>,
    pub categories: Vec<CategoryConfig>,
    #[serde(default)]
    pub otel: OtelConfig,
    #[serde(default)]
    pub rss_feeds: Vec<RssFeedConfig>,
    #[serde(default)]
    pub dav: DavConfig,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            general: GeneralConfig::default(),
            servers: Vec::new(),
            categories: vec![CategoryConfig::default()],
            otel: OtelConfig::default(),
            rss_feeds: Vec::new(),
            dav: DavConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct GeneralConfig {
    /// HTTP API listen address
    pub listen_addr: String,
    /// HTTP API port
    pub port: u16,
    /// API key for authentication
    pub api_key: Option<String>,
    /// Directory for incomplete downloads
    pub incomplete_dir: PathBuf,
    /// Directory for completed downloads
    pub complete_dir: PathBuf,
    /// Directory for application data (DB, logs)
    pub data_dir: PathBuf,
    /// Download speed limit in bytes/sec (0 = unlimited)
    pub speed_limit_bps: u64,
    /// Article cache size in bytes
    pub cache_size: u64,
    /// Log level
    pub log_level: String,
    /// Log file path (None = stdout only)
    pub log_file: Option<PathBuf>,
    /// History retention: how many NZBs to keep in history (None = keep all)
    pub history_retention: Option<usize>,
    /// Max number of NZBs downloading simultaneously (default 1)
    pub max_active_downloads: usize,
    /// Minimum free disk space in bytes before pausing downloads (default 1 GB)
    #[serde(default = "default_min_free_space")]
    pub min_free_space_bytes: u64,
    /// Directory to watch for new .nzb files to auto-enqueue
    pub watch_dir: Option<PathBuf>,
    /// RSS feed history limit: how many feed items to keep (None = keep all, default 500)
    #[serde(default = "default_rss_history_limit")]
    pub rss_history_limit: Option<usize>,
    /// Begin extracting RAR volumes during download instead of waiting for the
    /// job to complete. Requires `unrar` on PATH. Falls back to normal
    /// post-processing if articles fail or unrar is unavailable.
    #[serde(default = "default_true")]
    pub direct_unpack: bool,
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
}

fn default_rss_history_limit() -> Option<usize> {
    Some(500)
}

fn default_min_free_space() -> u64 {
    1_073_741_824 // 1 GB
}

fn default_required_completion_pct() -> f64 {
    100.2
}

fn default_article_timeout_secs() -> u64 {
    30
}

impl Default for GeneralConfig {
    fn default() -> Self {
        Self {
            listen_addr: "0.0.0.0".into(),
            port: 9090,
            api_key: None,
            incomplete_dir: PathBuf::from("/downloads/incomplete"),
            complete_dir: PathBuf::from("/downloads/complete"),
            data_dir: PathBuf::from("/data"),
            speed_limit_bps: 0,
            cache_size: 500 * 1024 * 1024, // 500 MB
            log_level: "info".into(),
            log_file: None,
            history_retention: None, // keep all
            max_active_downloads: 1,
            min_free_space_bytes: default_min_free_space(),
            watch_dir: None,
            rss_history_limit: default_rss_history_limit(),
            direct_unpack: true,
            abort_hopeless: true,
            early_failure_check: true,
            required_completion_pct: default_required_completion_pct(),
            article_timeout_secs: default_article_timeout_secs(),
        }
    }
}

/// OpenTelemetry configuration. All values can be overridden via env vars.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
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
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CategoryConfig {
    /// Category name
    pub name: String,
    /// Output directory override (relative to complete_dir)
    pub output_dir: Option<PathBuf>,
    /// Post-processing level: 0=none, 1=repair, 2=unpack, 3=repair+unpack
    pub post_processing: u8,
}

impl Default for CategoryConfig {
    fn default() -> Self {
        Self {
            name: "Default".into(),
            output_dir: None,
            post_processing: 3,
        }
    }
}

/// RSS feed configuration for automatic NZB downloading.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RssFeedConfig {
    /// Display name for the feed
    pub name: String,
    /// Feed URL (RSS 2.0 or Atom)
    pub url: String,
    /// How often to poll, in seconds (default 900 = 15 minutes)
    #[serde(default = "default_poll_interval")]
    pub poll_interval_secs: u64,
    /// Category to assign to downloaded NZBs
    #[serde(default)]
    pub category: Option<String>,
    /// Regex pattern to filter feed entries by title
    #[serde(default)]
    pub filter_regex: Option<String>,
    /// Whether this feed is active
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Auto-download all items from this feed (no rules needed).
    /// Ignored when filter_regex is set (use download rules instead).
    #[serde(default)]
    pub auto_download: bool,
}

fn default_poll_interval() -> u64 {
    900
}

fn default_true() -> bool {
    true
}

/// DAV pipeline auto-send configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
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
}

impl AppConfig {
    /// Load config from a TOML file, creating default if it doesn't exist.
    pub fn load(path: &std::path::Path) -> anyhow::Result<Self> {
        if path.exists() {
            let contents = std::fs::read_to_string(path)
                .with_context(|| format!("Failed to read config file: {}", path.display()))?;
            let config: AppConfig = toml::from_str(&contents)?;
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

    /// Save config to a TOML file.
    pub fn save(&self, path: &std::path::Path) -> anyhow::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).with_context(|| {
                format!("Failed to create config directory: {}", parent.display())
            })?;
        }
        let contents = toml::to_string_pretty(self)?;
        std::fs::write(path, &contents)
            .with_context(|| format!("Failed to write config file: {}", path.display()))?;
        Ok(())
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
        assert_eq!(cfg.pipelining, 1);
        assert!(!cfg.optional);
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
        assert_eq!(cfg.min_free_space_bytes, 1_073_741_824);
        assert!(cfg.watch_dir.is_none());
        assert_eq!(cfg.rss_history_limit, Some(500));
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
    fn test_find_category_or_default() {
        let mut cfg = AppConfig::default();
        cfg.categories.push(CategoryConfig {
            name: "movies".into(),
            output_dir: None,
            post_processing: 3,
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
