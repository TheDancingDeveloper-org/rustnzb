use std::path::PathBuf;
use std::sync::Arc;

use crate::nzb_core::config::AppConfig;
use crate::nzb_core::overrides::OverrideSet;
use arc_swap::ArcSwap;
use parking_lot::Mutex;

use crate::auth::{CredentialStore, TokenStore};
use crate::log_buffer::LogBuffer;
use crate::queue_manager::QueueManager;

/// Shared application state, accessible from all HTTP handlers.
///
/// Two configurations are kept. `config` is the effective one, what the rest
/// of the process reads: the file layer with the overrides applied. The file
/// layer is what is on disk, and it is the only thing that is saved. An
/// override therefore never reaches `config.toml`.
pub struct AppState {
    /// Effective configuration, published after every committed change.
    pub config: Arc<ArcSwap<AppConfig>>,
    pub config_path: PathBuf,
    pub queue_manager: Arc<QueueManager>,
    pub log_buffer: LogBuffer,
    pub token_store: Arc<TokenStore>,
    pub credential_store: Arc<CredentialStore>,
    /// Set once shutdown begins, so `/api/ready` stops reporting ready while
    /// the process drains and a load balancer can stop routing to it.
    pub shutting_down: std::sync::atomic::AtomicBool,
    /// When the application state was created at startup; the basis for the
    /// SABnzbd-compatible `uptime`.
    pub started_at: std::time::Instant,
    /// The file layer. Holding this lock serialises config writers, so a
    /// read-modify-write cycle (including persisting the TOML) cannot
    /// interleave with another one.
    file: Mutex<AppConfig>,
    /// Overrides from CLI, environment and desktop, applied over `file`.
    overrides: OverrideSet,
    /// Signalled after every committed config change so the RSS monitor
    /// re-reads its feeds at once instead of finishing its current sleep.
    /// Single consumer: `notify_one` stores a permit if the monitor is busy.
    pub rss_monitor_wake: Arc<tokio::sync::Notify>,
    /// Signalled after a committed change to the download schedule, so the
    /// schedule monitor applies it at once instead of at the next minute
    /// boundary.
    pub schedule_wake: Arc<tokio::sync::Notify>,
}

impl AppState {
    pub fn new(
        config: Arc<ArcSwap<AppConfig>>,
        config_path: PathBuf,
        queue_manager: Arc<QueueManager>,
        log_buffer: LogBuffer,
        token_store: Arc<TokenStore>,
        credential_store: Arc<CredentialStore>,
    ) -> Self {
        let file = (*config.load_full()).clone();
        Self {
            config,
            config_path,
            queue_manager,
            log_buffer,
            token_store,
            credential_store,
            shutting_down: std::sync::atomic::AtomicBool::new(false),
            started_at: std::time::Instant::now(),
            file: Mutex::new(file),
            overrides: OverrideSet::default(),
            rss_monitor_wake: Arc::new(tokio::sync::Notify::new()),
            schedule_wake: Arc::new(tokio::sync::Notify::new()),
        }
    }

    /// Set the file layer and the overrides that apply over it. `config` is
    /// recomputed from them, so it becomes the effective configuration.
    pub fn with_overrides(mut self, file: AppConfig, overrides: OverrideSet) -> Self {
        self.config.store(Arc::new(overrides.overlay(&file)));
        self.file = Mutex::new(file);
        self.overrides = overrides;
        self
    }

    /// Get current config snapshot.
    pub fn config(&self) -> Arc<AppConfig> {
        self.config.load_full()
    }

    /// Replace the whole config in memory and on disk.
    ///
    /// This is a blind write: anything another writer changed since `config`
    /// was read is lost. Handlers that modify part of the config must use
    /// [`AppState::update_config_with`] instead.
    pub fn update_config(&self, config: AppConfig) -> anyhow::Result<()> {
        let mut file = self.file.lock();
        self.commit_config(&mut file, config)
    }

    /// Atomically read, modify and persist the config.
    ///
    /// `f` runs on a copy of the latest config while holding the config write
    /// lock, which stays held until the TOML is saved and the new config is
    /// published, so concurrent updates cannot overwrite each other. If `f`
    /// returns an error nothing is written.
    pub fn update_config_with<R, E>(
        &self,
        f: impl FnOnce(&mut AppConfig) -> Result<R, E>,
    ) -> Result<R, E>
    where
        E: From<anyhow::Error>,
    {
        let mut file = self.file.lock();
        let mut next = file.clone();
        let result = f(&mut next)?;
        self.commit_config(&mut file, next)?;
        Ok(result)
    }

    /// Save `next` as the file layer, then publish it with the overrides
    /// applied. Nothing changes if the save fails.
    fn commit_config(&self, file: &mut AppConfig, mut next: AppConfig) -> anyhow::Result<()> {
        // Overridden keys keep their file value: a save never persists an
        // override, whichever handler built `next`.
        self.overrides.keep_file_values(&mut next, file);
        next.save(&self.config_path)?;
        self.config.store(Arc::new(self.overrides.overlay(&next)));
        *file = next;
        self.rss_monitor_wake.notify_one();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nzb_core::db::Database;
    use crate::nzb_core::overrides::{OverrideSource, OverrideValue};
    use proptest::prelude::*;

    /// Run the queue manager's background setup inside a runtime for the
    /// lifetime of the returned guard.
    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build tokio runtime")
    }

    /// A state whose file layer is `file`, saved to `dir/config.toml`, with
    /// `overrides` applied on top. Must be called inside a runtime.
    fn state_with(dir: &std::path::Path, file: &AppConfig, overrides: OverrideSet) -> AppState {
        let log_buffer = LogBuffer::default();
        let queue_manager = QueueManager::new(
            Vec::new(),
            Database::open_memory().expect("open in-memory database"),
            dir.join("incomplete"),
            dir.join("complete"),
            log_buffer.clone(),
            1,
            Vec::new(),
            0,
            0,
            false,
            5,
            false,
            false,
            100.0,
            30,
        );
        let config_path = dir.join("config.toml");
        file.save(&config_path).unwrap();
        AppState::new(
            Arc::new(ArcSwap::from_pointee(file.clone())),
            config_path,
            queue_manager,
            log_buffer,
            Arc::new(TokenStore::new()),
            Arc::new(CredentialStore::new(dir.to_path_buf())),
        )
        .with_overrides(file.clone(), overrides)
    }

    /// The file layer used by the tests: values that no override uses by
    /// default, so a leak into the file is visible.
    fn file_layer() -> AppConfig {
        let mut file = AppConfig::default();
        file.general.port = 9090;
        file.general.listen_addr = "127.0.0.1".into();
        file.general.data_dir = "/srv/rustnzb-file".into();
        file.otel.endpoint = "http://file-collector:4317".into();
        file
    }

    fn override_value(choice: u8, n: u16) -> Option<OverrideValue> {
        Some(match choice {
            0 => return None,
            1 => OverrideValue::Port(n),
            2 => OverrideValue::ListenAddr(format!("10.0.{}.{}", n % 250, n % 200)),
            3 => OverrideValue::DataDir(format!("/override/{n}").into()),
            4 => OverrideValue::DavEnabled(n.is_multiple_of(2)),
            5 => OverrideValue::OtelEnabled(!n.is_multiple_of(2)),
            _ => OverrideValue::OtelEndpoint(format!("http://collector-{n}:4317")),
        })
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(12))]

        /// For any set of overrides and any edit, a save writes the file layer
        /// and nothing else: no overridden value reaches `config.toml`, and the
        /// effective config still shows the override.
        #[test]
        fn save_never_persists_an_override(
            choices in proptest::collection::vec((0u8..7, 1u16..60_000), 7),
            speed in 0u64..1_000_000,
        ) {
            let rt = runtime();
            let _entered = rt.enter();
            let tempdir = tempfile::tempdir().unwrap();
            let file = file_layer();
            let mut overrides = OverrideSet::default();
            for (choice, n) in &choices {
                if let Some(value) = override_value(*choice, *n) {
                    overrides.set(value, OverrideSource::Cli);
                }
            }
            let state = state_with(tempdir.path(), &file, overrides.clone());

            state
                .update_config_with(|config| -> anyhow::Result<()> {
                    config.general.speed_limit_bps = speed;
                    Ok(())
                })
                .unwrap();

            let on_disk = AppConfig::load(&tempdir.path().join("config.toml")).unwrap();
            prop_assert_eq!(on_disk.general.speed_limit_bps, speed);
            prop_assert_eq!(on_disk.general.port, file.general.port);
            prop_assert_eq!(&on_disk.general.listen_addr, &file.general.listen_addr);
            prop_assert_eq!(&on_disk.general.data_dir, &file.general.data_dir);
            prop_assert_eq!(&on_disk.otel.endpoint, &file.otel.endpoint);
            prop_assert_eq!(on_disk.otel.enabled, file.otel.enabled);
            prop_assert_eq!(on_disk.dav.enabled, file.dav.enabled);

            let effective = state.config();
            prop_assert_eq!(effective.general.speed_limit_bps, speed);
            prop_assert_eq!(effective.general.port, overrides.overlay(&file).general.port);
            prop_assert_eq!(effective.dav.enabled, overrides.overlay(&file).dav.enabled);
        }
    }

    /// Docker starts the container with `--data-dir /data`. A GUI save must not
    /// copy that into the file, where it would outlive the flag.
    #[test]
    fn docker_data_dir_flag_is_not_saved_by_gui_edits() {
        let rt = runtime();
        let _entered = rt.enter();
        let tempdir = tempfile::tempdir().unwrap();
        let file = file_layer();
        let mut overrides = OverrideSet::default();
        overrides.set(OverrideValue::DataDir("/data".into()), OverrideSource::Cli);
        overrides.set(
            OverrideValue::Port(9090),
            OverrideSource::Env("RUSTNZB_PORT"),
        );
        let state = state_with(tempdir.path(), &file, overrides);

        state
            .update_config_with(|config| -> anyhow::Result<()> {
                config.general.speed_limit_bps = 4096;
                Ok(())
            })
            .unwrap();

        let text = std::fs::read_to_string(tempdir.path().join("config.toml")).unwrap();
        assert!(text.contains("data_dir = \"/srv/rustnzb-file\""), "{text}");
        assert!(
            !text.contains("/data\""),
            "override leaked into the file: {text}"
        );
        assert_eq!(state.config().general.data_dir, PathBuf::from("/data"));
        assert_eq!(state.config().general.speed_limit_bps, 4096);
    }

    /// `RUSTNZB_DAV_ENABLED` forces DAV on for this process only. Startup must
    /// not write the file, and a later save must not persist the forced value.
    #[test]
    fn dav_env_override_does_not_rewrite_the_file() {
        let rt = runtime();
        let _entered = rt.enter();
        let tempdir = tempfile::tempdir().unwrap();
        let file = file_layer();
        let config_path = tempdir.path().join("config.toml");
        file.save(&config_path).unwrap();
        let before = std::fs::read(&config_path).unwrap();

        let overrides = crate::startup::build_overrides(None, None, None, false, |name| {
            (name == "RUSTNZB_DAV_ENABLED").then(|| "1".to_string())
        });
        let state = state_with(tempdir.path(), &file, overrides);

        assert!(state.config().dav.enabled, "env should force DAV on");
        assert_eq!(std::fs::read(&config_path).unwrap(), before);

        state
            .update_config_with(|config| -> anyhow::Result<()> {
                config.general.speed_limit_bps = 1;
                Ok(())
            })
            .unwrap();
        let on_disk = AppConfig::load(&config_path).unwrap();
        assert!(!on_disk.dav.enabled, "forced DAV leaked into the file");
    }
}
