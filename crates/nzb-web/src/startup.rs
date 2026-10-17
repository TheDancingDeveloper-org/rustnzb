use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context;
use arc_swap::ArcSwap;
use tracing::{info, warn};

use crate::nzb_core::config::AppConfig;
use crate::nzb_core::db::Database;
use crate::nzb_core::overrides::{OverrideSet, OverrideSource, OverrideValue};

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

fn flag_value(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

/// Build the overrides in force for this process from the startup values
/// and from the environment (`env` reads one variable).
///
/// A value given to a startup field is labelled `env:NAME` when its variable
/// is set, because the CLI reads the variable itself, and `cli` otherwise.
/// Values from a desktop shell are labelled `desktop`.
pub fn build_overrides(
    listen_addr: Option<&str>,
    port: Option<u16>,
    data_dir: Option<&Path>,
    from_desktop: bool,
    env: impl Fn(&str) -> Option<String>,
) -> OverrideSet {
    let mut set = OverrideSet::default();
    let startup_source = |name: &'static str| {
        if from_desktop {
            OverrideSource::Desktop
        } else if env(name).is_some() {
            OverrideSource::Env(name)
        } else {
            OverrideSource::Cli
        }
    };
    if let Some(addr) = listen_addr {
        let source = startup_source("RUSTNZB_LISTEN_ADDR");
        set.set(OverrideValue::ListenAddr(addr.to_string()), source);
    }
    if let Some(port) = port {
        let source = startup_source("RUSTNZB_PORT");
        set.set(OverrideValue::Port(port), source);
    }
    if let Some(dir) = data_dir {
        let source = startup_source("RUSTNZB_DATA_DIR");
        set.set(OverrideValue::DataDir(dir.to_path_buf()), source);
    }

    let flag = |name: &'static str| env(name).map(|v| (name, flag_value(&v)));
    let text = |name: &'static str| env(name).map(|v| (name, v));
    let env_source = |name: &'static str| OverrideSource::Env(name);

    if let Some((name, val)) = flag("OTEL_ENABLED") {
        set.set(OverrideValue::OtelEnabled(val), env_source(name));
    }
    if let Some((name, val)) = text("OTEL_EXPORTER_OTLP_ENDPOINT") {
        set.set(OverrideValue::OtelEndpoint(val), env_source(name));
    }
    if let Some((name, val)) = flag("OTEL_LOGS_ENABLED") {
        set.set(OverrideValue::OtelLogsEnabled(val), env_source(name));
    }
    if let Some((name, val)) = text("OTEL_EXPORTER_OTLP_LOGS_ENDPOINT") {
        set.set(OverrideValue::OtelLogsEndpoint(val), env_source(name));
    }
    if let Some((name, val)) = flag("OTEL_METRICS_ENABLED") {
        set.set(OverrideValue::OtelMetricsEnabled(val), env_source(name));
    }
    if let Some((name, val)) = text("OTEL_EXPORTER_OTLP_METRICS_ENDPOINT") {
        set.set(OverrideValue::OtelMetricsEndpoint(val), env_source(name));
    }
    if let Some((name, val)) = text("OTEL_SERVICE_NAME") {
        set.set(OverrideValue::OtelServiceName(val), env_source(name));
    }

    // DAV is only forced on: a falsy variable leaves the file's choice alone.
    for name in ["RUSTNZB_DAV_ENABLED", "ENABLE_DAV"] {
        if flag(name).is_some_and(|(_, on)| on) {
            set.set(OverrideValue::DavEnabled(true), env_source(name));
            break;
        }
    }
    set
}

/// Create a data directory (e.g. `data_dir`/`incomplete_dir`/`complete_dir`), attaching
/// the failing path and a permission hint to any error so failures are actionable
/// instead of a bare `Permission denied (os error 13)`.
/// A hand-edited config.toml can carry a category the API would refuse.
/// Starting anyway keeps the rest of the service usable; the warning
/// explains why jobs in that category fail to enqueue.
fn warn_unsafe_categories(config: &AppConfig) {
    for category in &config.categories {
        if let Err(reason) = category.validate() {
            warn!(
                category = %category.name,
                output_dir = ?category.output_dir,
                reason,
                "Unsafe category in config; jobs in it will fail to enqueue until it is fixed"
            );
        }
    }
}

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
    /// Set by the desktop shell, whose listen address and data directory are
    /// fixed by the shell rather than by the user. Labels them `desktop`.
    pub from_desktop_shell: bool,
}

/// Result of engine initialization — everything needed to run the server.
pub struct StartupResult {
    pub state: Arc<AppState>,
    pub queue_manager: Arc<QueueManager>,
    pub log_buffer: LogBuffer,
    /// Held for the lifetime of the process so a second instance on the
    /// same data_dir refuses to start.
    pub instance_lock: InstanceLock,
}

/// Exclusive lock on `<data_dir>/rustnzb.lock` ensuring only one rustnzb
/// instance uses a given data directory at a time. Released on drop.
pub struct InstanceLock {
    #[cfg(unix)]
    _file: std::fs::File,
    #[cfg(windows)]
    _file: std::fs::File,
}

#[cfg(unix)]
impl InstanceLock {
    fn acquire(data_dir: &Path) -> anyhow::Result<Self> {
        use std::os::unix::io::AsRawFd;

        let file = open_lock_options()
            .open(lock_path(data_dir))
            .with_context(|| format!("failed to open {}", lock_path(data_dir).display()))?;
        let fd = file.as_raw_fd();
        let ret = unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) };
        if ret != 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::WouldBlock {
                anyhow::bail!(
                    "another rustnzb instance is using {}; stop it or choose a different data_dir",
                    data_dir.display()
                );
            }
            return Err(err).context(format!("failed to lock {}", lock_path(data_dir).display()));
        }
        Ok(Self { _file: file })
    }
}

#[cfg(windows)]
impl InstanceLock {
    fn acquire(data_dir: &Path) -> anyhow::Result<Self> {
        use std::os::windows::fs::OpenOptionsExt;

        // share_mode(0) denies all sharing: a second open fails outright.
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .share_mode(0)
            .open(lock_path(data_dir))
            .map_err(|err| {
                if err.raw_os_error() == Some(32) {
                    anyhow::anyhow!(
                        "another rustnzb instance is using {}; stop it or choose a different data_dir",
                        data_dir.display()
                    )
                } else {
                    anyhow::Error::new(err).context(format!(
                        "failed to lock {}",
                        lock_path(data_dir).display()
                    ))
                }
            })?;
        Ok(Self { _file: file })
    }
}

fn lock_path(data_dir: &Path) -> PathBuf {
    data_dir.join("rustnzb.lock")
}

fn open_lock_options() -> std::fs::OpenOptions {
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create(true);
    // Deliberately no truncate: the lock file's contents are irrelevant and
    // truncating would churn the file on every start.
    options
}

#[cfg(unix)]
impl Drop for InstanceLock {
    fn drop(&mut self) {
        use std::os::unix::io::AsRawFd;
        unsafe { libc::flock(self._file.as_raw_fd(), libc::LOCK_UN) };
    }
}

#[cfg(windows)]
impl Drop for InstanceLock {
    fn drop(&mut self) {
        // Closing the file handle releases the exclusive share.
    }
}

/// Acquire the single-instance lock for `data_dir`.
pub fn acquire_instance_lock(data_dir: &Path) -> anyhow::Result<InstanceLock> {
    InstanceLock::acquire(data_dir)
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
    let mut file = AppConfig::load(&config_path)?;
    sanitize_loaded_config(&mut file);
    let problems = file.validate();
    if !problems.is_empty() {
        anyhow::bail!(
            "invalid config {}:\n  - {}",
            config_path.display(),
            problems.join("\n  - ")
        );
    }

    // The file layer is what gets saved. Overrides from the command line,
    // the environment and the desktop shell apply on top of it, for this
    // process only.
    let overrides = build_overrides(
        startup.listen_addr.as_deref(),
        startup.port,
        startup.data_dir.as_deref(),
        startup.from_desktop_shell,
        |name| std::env::var(name).ok(),
    );
    let config = overrides.overlay(&file);

    warn_unsafe_categories(&config);

    // Ensure directories exist. The instance lock is taken before any
    // side-effect directories are created, so a second instance on the same
    // data_dir fails without leaving anything behind.
    create_data_dir(&config.general.data_dir)?;
    let instance_lock = acquire_instance_lock(&config.general.data_dir)
        .context("failed to acquire single-instance lock")?;
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

    // Set history retention (the setter folds 0 into "keep all").
    queue_manager.set_history_retention(config.general.history_retention);
    queue_manager.set_auto_sort_remaining_pct(config.general.auto_sort_remaining_pct);
    queue_manager.set_duplicate_policy(config.general.duplicate_policy);
    queue_manager.set_postproc_scripts(
        config.general.scripts_dir.clone(),
        config.general.script_success.clone(),
        config.general.script_failure.clone(),
        config.general.script_timeout_secs,
        config.general.script_max_output_bytes,
    );
    queue_manager.set_notifications(&config.notifications);
    queue_manager.set_queue_complete_persist(config.general.queue_complete_pers);

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

    // Build shared application state
    let state = Arc::new(
        AppState::new(
            Arc::clone(&shared_config),
            config_path,
            Arc::clone(&queue_manager),
            log_buffer.clone(),
            token_store,
            credential_store,
        )
        .with_overrides(file, overrides),
    );

    // Always start the RSS monitor so feeds added later via the API are polled.
    // Every committed config change wakes it, so a new or re-enabled feed is
    // checked promptly instead of after the monitor's current sleep.
    let data_dir_for_rss = shared_config.load().general.data_dir.clone();
    let monitor = crate::rss_monitor::RssMonitor::new(
        shared_config,
        Arc::clone(&queue_manager),
        data_dir_for_rss,
    )
    .with_wake(Arc::clone(&state.rss_monitor_wake));
    tokio::spawn(async move { monitor.run().await });

    Ok(StartupResult {
        state,
        queue_manager,
        log_buffer,
        instance_lock,
    })
}

#[cfg(test)]
mod tests {
    use super::{acquire_instance_lock, create_data_dir, sanitize_loaded_config};
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
    fn second_instance_on_same_data_dir_fails_before_side_effects() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("data");
        // initialize always creates the data_dir before acquiring the lock;
        // mirror that here so the lock file has a parent to live in.
        std::fs::create_dir_all(&data_dir).unwrap();

        let first = acquire_instance_lock(&data_dir).unwrap();

        let second = match acquire_instance_lock(&data_dir) {
            Err(e) => e,
            Ok(_) => panic!("second acquire should fail while first lock is held"),
        };
        let debug_text = format!("{second:?}");
        assert!(
            debug_text.contains("another rustnzb instance"),
            "error should mention the other instance, got: {debug_text}"
        );

        // Releasing the first lock lets a later instance acquire it again.
        drop(first);
        acquire_instance_lock(&data_dir).unwrap();
    }

    /// Desktop `start_engine` moves `StartupResult.instance_lock` into process
    /// state and then drops the rest of the result. The moved lock must still
    /// exclude a second instance until that owner drops it.
    #[test]
    fn moved_instance_lock_still_excludes_a_second_instance() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("data");
        std::fs::create_dir_all(&data_dir).unwrap();

        let lock = acquire_instance_lock(&data_dir).unwrap();
        let retained = lock;
        assert!(
            acquire_instance_lock(&data_dir).is_err(),
            "moving the lock out of StartupResult must not release it"
        );
        drop(retained);
        acquire_instance_lock(&data_dir).expect("released on drop");
    }

    #[test]
    fn overrides_are_labelled_by_where_they_came_from() {
        use crate::nzb_core::overrides::{OverrideSource, OverrideValue};

        // The CLI reads RUSTNZB_PORT itself, so a set variable means env wins
        // the label; with no variable the value came from the flag.
        let env = |name: &str| (name == "RUSTNZB_PORT").then(|| "8123".to_string());
        let set = super::build_overrides(Some("0.0.0.0"), Some(8123), None, false, env);
        assert_eq!(
            set.get("general.port").unwrap().source,
            OverrideSource::Env("RUSTNZB_PORT")
        );
        assert_eq!(
            set.get("general.listen_addr").unwrap().source,
            OverrideSource::Cli
        );
        assert!(set.get("general.data_dir").is_none());

        // The desktop shell fixes its own address and data directory.
        let set = super::build_overrides(
            Some("127.0.0.1"),
            None,
            Some(std::path::Path::new("/desk/data")),
            true,
            |_| None,
        );
        let listen = set.get("general.listen_addr").unwrap();
        assert_eq!(listen.value, OverrideValue::ListenAddr("127.0.0.1".into()));
        assert_eq!(listen.source, OverrideSource::Desktop);
        assert_eq!(
            set.get("general.data_dir").unwrap().source,
            OverrideSource::Desktop
        );
    }

    #[test]
    fn otel_and_dav_environment_become_overrides() {
        let env = |name: &str| match name {
            "OTEL_ENABLED" => Some("yes".to_string()),
            "OTEL_EXPORTER_OTLP_ENDPOINT" => Some("http://collector:4317".to_string()),
            "OTEL_SERVICE_NAME" => Some("rustnzb-test".to_string()),
            // A falsy DAV variable leaves the file's choice alone...
            "RUSTNZB_DAV_ENABLED" => Some("0".to_string()),
            // ...but the alias forces it on.
            "ENABLE_DAV" => Some("true".to_string()),
            _ => None,
        };
        let set = super::build_overrides(None, None, None, false, env);
        let mut config = AppConfig::default();
        set.apply(&mut config);
        assert!(config.otel.enabled);
        assert_eq!(config.otel.endpoint, "http://collector:4317");
        assert_eq!(config.otel.service_name, "rustnzb-test");
        assert!(config.dav.enabled);
        assert_eq!(
            set.get("dav.enabled").unwrap().source.to_string(),
            "env:ENABLE_DAV"
        );

        let falsy = |name: &str| (name == "RUSTNZB_DAV_ENABLED").then(|| "off".to_string());
        assert!(
            super::build_overrides(None, None, None, false, falsy)
                .get("dav.enabled")
                .is_none()
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
