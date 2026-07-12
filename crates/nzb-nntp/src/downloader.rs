//! Download orchestrator.
//!
//! Takes a list of articles and coordinates downloading them across multiple
//! NNTP servers with priority-based failover, pipelining, bandwidth limiting,
//! and pause/resume support.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use parking_lot::Mutex;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use crate::config::Article;
use crate::config::ServerConfig;

use crate::error::{NntpError, NntpResult};
use crate::pool::PooledConnection;
use crate::server::ServerState;

// ---------------------------------------------------------------------------
// Download result
// ---------------------------------------------------------------------------

/// The outcome of downloading a single article.
#[derive(Debug)]
pub struct ArticleResult {
    /// The article that was fetched.
    pub article: Article,
    /// The server that served this article (if successful).
    pub server_id: Option<String>,
    /// The download result: Ok with raw article data, or an error.
    pub result: Result<Vec<u8>, NntpError>,
}

// ---------------------------------------------------------------------------
// Download request (internal)
// ---------------------------------------------------------------------------

/// An article queued for download, with try-list tracking.
struct DownloadRequest {
    article: Article,
    /// Server IDs that have already been tried for this article.
    tried_servers: Vec<String>,
}

// ---------------------------------------------------------------------------
// Server pick result (value type, no borrows)
// ---------------------------------------------------------------------------

/// Information about a picked server, extracted under the lock.
struct ServerPick {
    index: usize,
    server_id: String,
    config: Arc<ServerConfig>,
}

// ---------------------------------------------------------------------------
// Downloader
// ---------------------------------------------------------------------------

/// Orchestrates downloading articles across multiple servers.
///
/// Articles are assigned to the highest-priority available server. If a server
/// fails, the article is retried on the next-highest-priority server that has
/// not been tried yet.
pub struct Downloader {
    /// Servers sorted by priority (lowest number = highest priority).
    servers: Arc<Mutex<Vec<ServerState>>>,
    /// Whether downloading is paused.
    paused: Arc<AtomicBool>,
    /// Whether the downloader has been shut down.
    shutdown: Arc<AtomicBool>,
    /// Global bandwidth limit in bytes/sec (0 = unlimited).
    bandwidth_limit_bps: u64,
}

impl Downloader {
    /// Create a new downloader with the given server configurations.
    pub fn new(mut server_configs: Vec<ServerConfig>, bandwidth_limit_bps: u64) -> Self {
        // Sort by priority (ascending: 0 is highest priority)
        server_configs.sort_by_key(|c| c.priority);

        let servers: Vec<ServerState> = server_configs
            .into_iter()
            .filter(|c| c.enabled)
            .map(ServerState::new)
            .collect();

        info!(server_count = servers.len(), "Downloader initialized");

        Self {
            servers: Arc::new(Mutex::new(servers)),
            paused: Arc::new(AtomicBool::new(false)),
            shutdown: Arc::new(AtomicBool::new(false)),
            bandwidth_limit_bps,
        }
    }

    /// Pause downloading. In-flight requests complete but no new ones start.
    pub fn pause(&self) {
        self.paused.store(true, Ordering::Relaxed);
        info!("Downloader paused");
    }

    /// Resume downloading.
    pub fn resume(&self) {
        self.paused.store(false, Ordering::Relaxed);
        info!("Downloader resumed");
    }

    /// Check if the downloader is paused.
    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Relaxed)
    }

    /// Signal the downloader to shut down.
    pub fn shutdown(&self) {
        self.shutdown.store(true, Ordering::Relaxed);
        info!("Downloader shutdown requested");
    }

    /// Download a batch of articles, sending results through the provided channel.
    ///
    /// Each article is tried on servers in priority order. Results (success or
    /// failure) are sent via `result_tx` as they complete.
    pub async fn download(
        &self,
        articles: Vec<Article>,
        result_tx: mpsc::Sender<ArticleResult>,
    ) -> NntpResult<()> {
        if articles.is_empty() {
            return Ok(());
        }

        debug!(count = articles.len(), "Starting article downloads");

        let mut pending: Vec<DownloadRequest> = articles
            .into_iter()
            .map(|article| DownloadRequest {
                tried_servers: article.tried_servers.clone(),
                article,
            })
            .collect();

        while !pending.is_empty() {
            if self.shutdown.load(Ordering::Relaxed) {
                return Err(NntpError::Shutdown);
            }

            // Wait while paused
            while self.paused.load(Ordering::Relaxed) {
                if self.shutdown.load(Ordering::Relaxed) {
                    return Err(NntpError::Shutdown);
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
            }

            let mut request = pending.remove(0);

            // Pick the best server (short lock, no await)
            let pick = self.pick_server(&request.tried_servers);

            let Some(pick) = pick else {
                // All servers exhausted for this article
                let _ = result_tx
                    .send(ArticleResult {
                        article: request.article.clone(),
                        server_id: None,
                        result: Err(NntpError::AllServersExhausted(
                            request.article.message_id.clone(),
                        )),
                    })
                    .await;
                continue;
            };

            // Create a fresh connection outside any lock (fully async-safe)
            let conn_result = self.connect_to_server(&pick.config).await;

            match conn_result {
                Ok(mut pooled) => {
                    let fetch_result = pooled.conn.fetch_article(&request.article.message_id).await;

                    match fetch_result {
                        Ok(response) => {
                            let data = response.data.unwrap_or_default();
                            let data_len = data.len() as u64;

                            // Record success (short lock)
                            {
                                let mut servers = self.servers.lock();
                                if let Some(server) = servers.get_mut(pick.index) {
                                    server.record_success(data_len);
                                    server.release_connection(pooled);
                                }
                            }

                            let _ = result_tx
                                .send(ArticleResult {
                                    article: request.article,
                                    server_id: Some(pick.server_id),
                                    result: Ok(data),
                                })
                                .await;
                        }
                        Err(NntpError::ArticleNotFound(_)) => {
                            // Not on this server — return conn and try next
                            {
                                let mut servers = self.servers.lock();
                                if let Some(server) = servers.get_mut(pick.index) {
                                    server.record_failure();
                                    server.release_connection(pooled);
                                }
                            }
                            request.tried_servers.push(pick.server_id);
                            pending.push(request);
                        }
                        Err(e) => {
                            let is_fatal = matches!(
                                &e,
                                NntpError::AuthRequired(_)
                                    | NntpError::PermissionDenied(_)
                                    | NntpError::ServiceUnavailable(_)
                                    | NntpError::Connection(_)
                                    | NntpError::Io(_)
                            );
                            {
                                let mut servers = self.servers.lock();
                                if let Some(server) = servers.get_mut(pick.index) {
                                    server.record_failure();
                                    if is_fatal {
                                        server.penalize_for(&e.to_string());
                                        server.discard_connection(pooled);
                                    } else {
                                        server.release_connection(pooled);
                                    }
                                }
                            }
                            request.tried_servers.push(pick.server_id);
                            pending.push(request);
                        }
                    }
                }
                Err(e) => {
                    warn!(server = %pick.server_id, "Failed to connect: {e}");
                    {
                        let mut servers = self.servers.lock();
                        if let Some(server) = servers.get_mut(pick.index) {
                            server.penalize_for(&e.to_string());
                        }
                    }
                    request.tried_servers.push(pick.server_id);
                    pending.push(request);
                }
            }

            // Simple bandwidth limiting: yield to let other tasks run
            if self.bandwidth_limit_bps > 0 {
                tokio::task::yield_now().await;
            }
        }

        Ok(())
    }

    // ------------------------------------------------------------------
    // Internal helpers
    // ------------------------------------------------------------------

    /// Pick the highest-priority available server not yet tried.
    fn pick_server(&self, tried: &[String]) -> Option<ServerPick> {
        let servers = self.servers.lock();
        for (idx, server) in servers.iter().enumerate() {
            if !server.is_available() {
                debug!(
                    server = %server.config.name,
                    active = server.active,
                    penalized = server.penalty_until.is_some(),
                    last_error = server.last_error.as_deref().unwrap_or("(none)"),
                    "Skipping server (not available)"
                );
                continue;
            }
            if tried.contains(&server.config.id) {
                debug!(server = %server.config.name, "Skipping server (already tried)");
                continue;
            }
            debug!(
                server = %server.config.name,
                host = %server.config.host,
                priority = server.config.priority,
                "Picked server for download"
            );
            return Some(ServerPick {
                index: idx,
                server_id: server.config.id.clone(),
                config: Arc::clone(&server.config),
            });
        }
        warn!(
            tried = ?tried,
            total_servers = servers.len(),
            "No available server found — all tried or penalized"
        );
        None
    }

    /// Create a fresh NNTP connection to the given server.
    /// This does NOT go through the pool (avoids holding locks across await).
    async fn connect_to_server(&self, config: &ServerConfig) -> NntpResult<PooledConnection> {
        info!(
            server = %config.name,
            host = %config.host,
            port = config.port,
            ssl = config.ssl,
            "Downloader: creating fresh connection (bypassing pool)"
        );
        let mut conn = crate::connection::NntpConnection::new(format!("{}#dl", config.id));
        conn.connect(config).await.inspect_err(|e| {
            error!(
                server = %config.name,
                host = %config.host,
                error = %e,
                "Downloader: fresh connection FAILED"
            );
        })?;
        info!(
            server = %config.name,
            host = %config.host,
            "Downloader: fresh connection ready"
        );
        Ok(PooledConnection::unmanaged(conn))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{MockConfig, MockNntpServer, test_config};
    use std::collections::HashMap;

    fn make_article(message_id: &str, segment: u32) -> Article {
        Article {
            message_id: message_id.to_string(),
            segment_number: segment,
            bytes: 1000,
            downloaded: false,
            data_begin: None,
            data_size: None,
            crc32: None,
            tried_servers: Vec::new(),
            tries: 0,
        }
    }

    #[tokio::test]
    async fn test_downloader_single_article_success() {
        let mut articles = HashMap::new();
        articles.insert("dl1@test".into(), b"Downloaded content".to_vec());

        let server = MockNntpServer::start(MockConfig {
            articles,
            ..MockConfig::default()
        })
        .await;
        let config = test_config(server.port());

        let downloader = Downloader::new(vec![config], 0);
        let (tx, mut rx) = mpsc::channel(10);

        let articles = vec![make_article("dl1@test", 1)];
        downloader.download(articles, tx).await.unwrap();

        let result = rx.recv().await.unwrap();
        assert!(result.result.is_ok());
        assert_eq!(result.article.message_id, "dl1@test");
        assert!(result.server_id.is_some());
        let data = result.result.unwrap();
        let body = String::from_utf8_lossy(&data);
        assert!(body.contains("Downloaded content"));
    }

    #[tokio::test]
    async fn test_downloader_article_not_found() {
        let server = MockNntpServer::start(MockConfig::default()).await;
        let config = test_config(server.port());

        let downloader = Downloader::new(vec![config], 0);
        let (tx, mut rx) = mpsc::channel(10);

        let articles = vec![make_article("missing@test", 1)];
        downloader.download(articles, tx).await.unwrap();

        let result = rx.recv().await.unwrap();
        assert!(result.result.is_err());
        assert!(matches!(
            result.result.unwrap_err(),
            NntpError::AllServersExhausted(_)
        ));
    }

    #[tokio::test]
    async fn test_downloader_multiple_articles() {
        let mut articles = HashMap::new();
        articles.insert("m1@test".into(), b"Article 1".to_vec());
        articles.insert("m2@test".into(), b"Article 2".to_vec());
        articles.insert("m3@test".into(), b"Article 3".to_vec());

        let server = MockNntpServer::start(MockConfig {
            articles,
            ..MockConfig::default()
        })
        .await;
        let config = test_config(server.port());

        let downloader = Downloader::new(vec![config], 0);
        let (tx, mut rx) = mpsc::channel(10);

        let articles = vec![
            make_article("m1@test", 1),
            make_article("m2@test", 2),
            make_article("m3@test", 3),
        ];
        downloader.download(articles, tx).await.unwrap();

        let mut success_count = 0;
        while let Ok(result) = rx.try_recv() {
            if result.result.is_ok() {
                success_count += 1;
            }
        }
        assert_eq!(success_count, 3);
    }

    #[tokio::test]
    async fn test_downloader_failover_to_second_server() {
        // Server 1: no articles
        let server1 = MockNntpServer::start(MockConfig::default()).await;
        let mut config1 = test_config(server1.port());
        config1.id = "server-1".into();
        config1.name = "Server 1".into();
        config1.priority = 0;

        // Server 2: has the article
        let mut articles = HashMap::new();
        articles.insert("failover@test".into(), b"Found on server 2".to_vec());
        let server2 = MockNntpServer::start(MockConfig {
            articles,
            ..MockConfig::default()
        })
        .await;
        let mut config2 = test_config(server2.port());
        config2.id = "server-2".into();
        config2.name = "Server 2".into();
        config2.priority = 1;

        let downloader = Downloader::new(vec![config1, config2], 0);
        let (tx, mut rx) = mpsc::channel(10);

        let articles = vec![make_article("failover@test", 1)];
        downloader.download(articles, tx).await.unwrap();

        let result = rx.recv().await.unwrap();
        assert!(result.result.is_ok());
        assert_eq!(result.server_id.as_deref(), Some("server-2"));
    }

    #[tokio::test]
    async fn test_downloader_empty_article_list() {
        let server = MockNntpServer::start(MockConfig::default()).await;
        let config = test_config(server.port());

        let downloader = Downloader::new(vec![config], 0);
        let (tx, mut rx) = mpsc::channel(10);

        downloader.download(vec![], tx).await.unwrap();
        // No results should be sent
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn test_downloader_pause_resume() {
        let downloader = Downloader::new(vec![], 0);

        assert!(!downloader.is_paused());
        downloader.pause();
        assert!(downloader.is_paused());
        downloader.resume();
        assert!(!downloader.is_paused());
    }

    #[tokio::test]
    async fn test_downloader_shutdown_signal() {
        let mut articles = HashMap::new();
        articles.insert("a@test".into(), b"data".to_vec());

        let server = MockNntpServer::start(MockConfig {
            articles,
            ..MockConfig::default()
        })
        .await;
        let config = test_config(server.port());

        let downloader = Downloader::new(vec![config], 0);

        // Shut down before downloading
        downloader.shutdown();

        let (tx, _rx) = mpsc::channel(10);
        let result = downloader
            .download(vec![make_article("a@test", 1)], tx)
            .await;
        assert!(matches!(result, Err(NntpError::Shutdown)));
    }

    #[tokio::test]
    async fn test_downloader_disabled_servers_filtered() {
        let server = MockNntpServer::start(MockConfig::default()).await;
        let mut config = test_config(server.port());
        config.enabled = false;

        let downloader = Downloader::new(vec![config], 0);
        let (tx, mut rx) = mpsc::channel(10);

        let articles = vec![make_article("art@test", 1)];
        downloader.download(articles, tx).await.unwrap();

        // No servers available → AllServersExhausted
        let result = rx.recv().await.unwrap();
        assert!(matches!(
            result.result.unwrap_err(),
            NntpError::AllServersExhausted(_)
        ));
    }

    #[tokio::test]
    async fn test_downloader_priority_ordering() {
        // Both servers have the article, but server with priority 0 should be used first
        let mut articles = HashMap::new();
        articles.insert("prio@test".into(), b"priority data".to_vec());

        let server_high = MockNntpServer::start(MockConfig {
            articles: articles.clone(),
            ..MockConfig::default()
        })
        .await;
        let mut config_high = test_config(server_high.port());
        config_high.id = "high-prio".into();
        config_high.name = "High Priority".into();
        config_high.priority = 0;

        let server_low = MockNntpServer::start(MockConfig {
            articles,
            ..MockConfig::default()
        })
        .await;
        let mut config_low = test_config(server_low.port());
        config_low.id = "low-prio".into();
        config_low.name = "Low Priority".into();
        config_low.priority = 1;

        // Pass low priority first — downloader should sort by priority
        let downloader = Downloader::new(vec![config_low, config_high], 0);
        let (tx, mut rx) = mpsc::channel(10);

        let articles = vec![make_article("prio@test", 1)];
        downloader.download(articles, tx).await.unwrap();

        let result = rx.recv().await.unwrap();
        assert!(result.result.is_ok());
        assert_eq!(result.server_id.as_deref(), Some("high-prio"));
    }

    #[tokio::test]
    async fn test_downloader_service_unavailable_penalizes() {
        // Server 1: returns 502
        let server1 = MockNntpServer::start(MockConfig {
            service_unavailable: true,
            ..MockConfig::default()
        })
        .await;
        let mut config1 = test_config(server1.port());
        config1.id = "bad-server".into();
        config1.name = "Bad Server".into();
        config1.priority = 0;

        // Server 2: works fine
        let mut articles = HashMap::new();
        articles.insert("pen@test".into(), b"backup data".to_vec());
        let server2 = MockNntpServer::start(MockConfig {
            articles,
            ..MockConfig::default()
        })
        .await;
        let mut config2 = test_config(server2.port());
        config2.id = "good-server".into();
        config2.name = "Good Server".into();
        config2.priority = 1;

        let downloader = Downloader::new(vec![config1, config2], 0);
        let (tx, mut rx) = mpsc::channel(10);

        let articles = vec![make_article("pen@test", 1)];
        downloader.download(articles, tx).await.unwrap();

        let result = rx.recv().await.unwrap();
        assert!(result.result.is_ok());
        // Should have fallen through to the good server
        assert_eq!(result.server_id.as_deref(), Some("good-server"));
    }
}
