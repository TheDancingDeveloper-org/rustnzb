use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context;
use arc_swap::ArcSwap;
use tracing::info;

use crate::nzb_core::config::AppConfig;
use crate::nzb_core::db::Database;

use crate::auth::{CredentialStore, TokenStore};
use crate::log_buffer::LogBuffer;
use crate::queue_manager::QueueManager;
use crate::state::AppState;
use nzb_postproc::PostProcLimits;

fn sanitize_loaded_config(config: &mut AppConfig) {
    for server in &mut config.servers {
        let trim = |value: &mut String| {
            let trimmed = value.trim();
            if trimmed.len() != value.len() {
                *value = trimmed.to_string();
            }
        };
        let trim_opt = |value: &mut Option<String>| {
            if let Some(inner) = value.as_mut() {
                trim(inner);
            }
        };

        trim(&mut server.host);
        trim(&mut server.name);
        trim_opt(&mut server.username);
        trim_opt(&mut server.password);
        trim_opt(&mut server.proxy_url);
        trim_opt(&mut server.trusted_fingerprint);
    }
}

fn env_flag_enabled(name: &str) -> Option<bool> {
    std::env::var(name).ok().map(|value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

/// Create a data directory (e.g. `data_dir`/`incomplete_dir`/`complete_dir`), attaching
/// the failing path and a permission hint to any error so failures are actionable
/// instead of a bare `Permission denied (os error 13)`.
fn create_data_dir(path: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(path).with_context(|| {
        format!(
            "Failed to create directory {}. \
             Check that the directory (and its parent) is writable by the current user. \
             If using Docker, ensure the volume is owned by the container's user.",
            path.display()
        )
    })
}

/// Configuration for engine initialization.
///
/// All fields except `config_path` are optional overrides —
/// when `None`, values from the TOML config file are used.
pub struct StartupConfig {
    /// Path to the TOML config file.
    pub config_path: PathBuf,
    /// Override listen address (e.g. "0.0.0.0").
    pub listen_addr: Option<String>,
    /// Override listen port.
    pub port: Option<u16>,
    /// Override data directory.
    pub data_dir: Option<PathBuf>,
    /// Log level filter string (e.g. "info", "debug").
    pub log_level: Option<String>,
}

/// Result of engine initialization — everything needed to run the server.
pub struct StartupResult {
    pub state: Arc<AppState>,
    pub queue_manager: Arc<QueueManager>,
    pub log_buffer: LogBuffer,
}

/// Initialize the rustnzb engine: load config, open database,
/// create QueueManager, spawn background services, build AppState.
///
/// Does **not** start the HTTP server or initialize logging/tracing —
/// callers are responsible for those.
///
/// Pass an existing `LogBuffer` if one was already created (e.g. for a
/// tracing layer that must be installed before this function runs).
/// If `None`, a new one is created.
pub async fn initialize(
    startup: StartupConfig,
    log_buffer: Option<LogBuffer>,
) -> anyhow::Result<StartupResult> {
    let config_path = startup.config_path;
    let mut config = AppConfig::load(&config_path)?;
    sanitize_loaded_config(&mut config);

    // Apply overrides
    if let Some(addr) = startup.listen_addr {
        config.general.listen_addr = addr;
    }
    if let Some(port) = startup.port {
        config.general.port = port;
    }
    if let Some(data_dir) = startup.data_dir {
        config.general.data_dir = data_dir;
    }

    // Apply env var overrides for OpenTelemetry
    if let Some(val) = env_flag_enabled("OTEL_ENABLED") {
        config.otel.enabled = val;
    }
    if let Ok(val) = std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT") {
        config.otel.endpoint = val;
    }
    if let Some(val) = env_flag_enabled("OTEL_LOGS_ENABLED") {
        config.otel.logs_enabled = Some(val);
    }
    if let Ok(val) = std::env::var("OTEL_EXPORTER_OTLP_LOGS_ENDPOINT") {
        config.otel.logs_endpoint = Some(val);
    }
    if let Some(val) = env_flag_enabled("OTEL_METRICS_ENABLED") {
        config.otel.metrics_enabled = Some(val);
    }
    if let Ok(val) = std::env::var("OTEL_EXPORTER_OTLP_METRICS_ENDPOINT") {
        config.otel.metrics_endpoint = Some(val);
    }
    if let Ok(val) = std::env::var("OTEL_SERVICE_NAME") {
        config.otel.service_name = val;
    }

    // Ensure directories exist
    create_data_dir(&config.general.data_dir)?;
    create_data_dir(&config.general.incomplete_dir)?;
    create_data_dir(&config.general.complete_dir)?;

    // Open database
    let db_path = config.general.data_dir.join("rustnzb.db");
    let db = Database::open(&db_path)?;
    info!(path = %db_path.display(), "Database opened");

    // Use provided log buffer or create a new one
    let log_buffer = log_buffer.unwrap_or_default();

    // Create the queue manager
    let queue_manager = QueueManager::new_with_postproc_limits(
        config.servers.clone(),
        db,
        config.general.incomplete_dir.clone(),
        config.general.complete_dir.clone(),
        log_buffer.clone(),
        config.general.max_active_downloads,
        PostProcLimits {
            pipelines: config.general.max_post_processing_jobs,
            repair: config.general.max_repair_workers,
            extract: config.general.max_extract_workers,
        },
        config.categories.clone(),
        config.general.min_free_space_bytes,
        config.general.speed_limit_bps,
        config.general.direct_unpack,
        config.general.max_nested_archive_depth,
        config.general.abort_hopeless,
        config.general.early_failure_check,
        config.general.required_completion_pct,
        config.general.article_timeout_secs,
    );

    // Set history retention
    if let Some(retention) = config.general.history_retention {
        queue_manager.set_history_retention(Some(retention));
    }

    // Restore any in-progress jobs from the database
    if let Err(e) = queue_manager.restore_from_db() {
        tracing::warn!("Failed to restore queue from database: {e}");
    }

    // Spawn the speed tracker background task
    queue_manager.spawn_speed_tracker();

    info!(servers = config.servers.len(), "Queue manager initialized");

    // Start directory watcher if configured
    if let Some(ref watch_dir) = config.general.watch_dir {
        let watcher =
            crate::dir_watcher::DirWatcher::new(watch_dir.clone(), Arc::clone(&queue_manager));
        tokio::spawn(async move { watcher.run().await });
        info!(dir = %watch_dir.display(), "Directory watcher started");
    }

    // Create auth stores
    let credential_store = Arc::new(CredentialStore::new(config.general.data_dir.clone()));
    let token_store = Arc::new(TokenStore::new());

    if credential_store.has_credentials() {
        info!("Authentication enabled (credentials configured)");
    } else {
        info!("Authentication not yet configured; first-boot setup required");
    }

    // Build shared config (ArcSwap) so the RSS monitor and AppState share
    // the same live config — feeds added/removed via the API are picked up
    // without a restart.
    let shared_config = Arc::new(ArcSwap::new(Arc::new(config)));

    // Always start the RSS monitor so feeds added later via the API are polled.
    let data_dir_for_rss = shared_config.load().general.data_dir.clone();
    let monitor = crate::rss_monitor::RssMonitor::new(
        Arc::clone(&shared_config),
        Arc::clone(&queue_manager),
        data_dir_for_rss,
    );
    tokio::spawn(async move { monitor.run().await });

    // Build shared application state
    let state = Arc::new(AppState::new(
        shared_config,
        config_path,
        Arc::clone(&queue_manager),
        log_buffer.clone(),
        token_store,
        credential_store,
    ));

    Ok(StartupResult {
        state,
        queue_manager,
        log_buffer,
    })
}

#[cfg(test)]
mod tests {
    use super::{create_data_dir, sanitize_loaded_config};
    use crate::nzb_core::config::AppConfig;
    use crate::nzb_core::config::ServerConfig;

    #[test]
    fn create_data_dir_creates_nested_directories() {
        let tmp = tempfile::tempdir().unwrap();
        let nested = tmp.path().join("a").join("b").join("c");

        create_data_dir(&nested).expect("nested directory creation should succeed");

        assert!(nested.is_dir());
    }

    #[cfg(unix)]
    #[test]
    fn create_data_dir_wraps_permission_denied_with_context() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let locked_parent = tmp.path().join("locked");
        std::fs::create_dir_all(&locked_parent).unwrap();
        std::fs::set_permissions(&locked_parent, std::fs::Permissions::from_mode(0o000)).unwrap();

        let target = locked_parent.join("data");
        let result = create_data_dir(&target);

        // Restore permissions so the tempdir can be cleaned up.
        std::fs::set_permissions(&locked_parent, std::fs::Permissions::from_mode(0o755)).unwrap();

        let err = match result {
            Err(e) => e,
            // Running as root (e.g. CI containers) bypasses the permission
            // check entirely, so there's nothing to assert.
            Ok(()) => return,
        };
        let debug_text = format!("{err:?}");

        assert!(
            debug_text.contains(&target.display().to_string()),
            "error should mention the failing path, got: {debug_text}"
        );
        assert!(
            debug_text.contains("Caused by"),
            "error should retain the underlying io::Error in the chain, got: {debug_text}"
        );
    }

    #[test]
    fn sanitize_loaded_config_trims_server_fields() {
        let mut config = AppConfig::default();
        let mut server = ServerConfig::new("srv-1", " news.example.com \n");
        server.name = " Primary ".into();
        server.username = Some(" user ".into());
        server.password = Some(" pass ".into());
        server.proxy_url = Some(" socks5://proxy ".into());
        server.trusted_fingerprint = Some(" abc123 ".into());
        config.servers.push(server);

        sanitize_loaded_config(&mut config);

        let server = &config.servers[0];
        assert_eq!(server.host, "news.example.com");
        assert_eq!(server.name, "Primary");
        assert_eq!(server.username.as_deref(), Some("user"));
        assert_eq!(server.password.as_deref(), Some("pass"));
        assert_eq!(server.proxy_url.as_deref(), Some("socks5://proxy"));
        assert_eq!(server.trusted_fingerprint.as_deref(), Some("abc123"));
    }
}
