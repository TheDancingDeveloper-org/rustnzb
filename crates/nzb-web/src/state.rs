use std::path::PathBuf;
use std::sync::Arc;

use crate::nzb_core::config::AppConfig;
use arc_swap::ArcSwap;

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
        }
    }

    /// Get current config snapshot.
    pub fn config(&self) -> Arc<AppConfig> {
        self.config.load_full()
    }

    /// Update config in memory and save to file.
    pub fn update_config(&self, config: AppConfig) -> anyhow::Result<()> {
        config.save(&self.config_path)?;
        self.config.store(Arc::new(config));
        Ok(())
    }
}
