use std::path::PathBuf;
use std::sync::Arc;

use crate::nzb_core::config::AppConfig;
use arc_swap::ArcSwap;
use parking_lot::Mutex;

use crate::auth::{CredentialStore, TokenStore};
use crate::log_buffer::LogBuffer;
use crate::queue_manager::QueueManager;

/// Shared application state, accessible from all HTTP handlers.
pub struct AppState {
    pub config: Arc<ArcSwap<AppConfig>>,
    pub config_path: PathBuf,
    pub queue_manager: Arc<QueueManager>,
    pub log_buffer: LogBuffer,
    pub token_store: Arc<TokenStore>,
    pub credential_store: Arc<CredentialStore>,
    /// When the application state was created at startup; the basis for the
    /// SABnzbd-compatible `uptime`.
    pub started_at: std::time::Instant,
    /// Serialises config writers so a read-modify-write cycle (including
    /// persisting the TOML) cannot interleave with another one.
    config_write: Mutex<()>,
    /// Signalled after every committed config change so the RSS monitor
    /// re-reads its feeds at once instead of finishing its current sleep.
    /// Single consumer: `notify_one` stores a permit if the monitor is busy.
    pub rss_monitor_wake: Arc<tokio::sync::Notify>,
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
        Self {
            config,
            config_path,
            queue_manager,
            log_buffer,
            token_store,
            credential_store,
            started_at: std::time::Instant::now(),
            config_write: Mutex::new(()),
            rss_monitor_wake: Arc::new(tokio::sync::Notify::new()),
        }
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
        let _guard = self.config_write.lock();
        self.commit_config(config)
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
        let _guard = self.config_write.lock();
        let mut config = (*self.config.load_full()).clone();
        let result = f(&mut config)?;
        self.commit_config(config)?;
        Ok(result)
    }

    fn commit_config(&self, config: AppConfig) -> anyhow::Result<()> {
        config.save(&self.config_path)?;
        self.config.store(Arc::new(config));
        self.rss_monitor_wake.notify_one();
        Ok(())
    }
}
