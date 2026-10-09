//! Server health tracking, penalties, connection management, and speed tracking.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tracing::{debug, info, warn};

use crate::config::ServerConfig;

use crate::error::NntpResult;
use crate::pool::{ConnectionPool, PooledConnection};

/// Penalty durations for various error conditions.
const PENALTY_UNKNOWN: Duration = Duration::from_secs(3 * 60);
const PENALTY_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const PENALTY_AUTH: Duration = Duration::from_secs(10 * 60);

/// Number of speed samples to keep for the rolling average.
const SPEED_WINDOW_SIZE: usize = 30;

// ---------------------------------------------------------------------------
// Speed tracker
// ---------------------------------------------------------------------------

/// A sample of bytes downloaded during a time period.
struct SpeedSample {
    bytes: u64,
    timestamp: Instant,
}

/// Tracks download speed as a rolling average over recent samples.
struct SpeedTracker {
    samples: VecDeque<SpeedSample>,
    window: Duration,
}

impl SpeedTracker {
    fn new() -> Self {
        Self {
            samples: VecDeque::with_capacity(SPEED_WINDOW_SIZE + 1),
            window: Duration::from_secs(10),
        }
    }

    /// Record that `bytes` were downloaded at this moment.
    fn record(&mut self, bytes: u64) {
        let now = Instant::now();
        self.samples.push_back(SpeedSample {
            bytes,
            timestamp: now,
        });

        // Remove samples older than the window
        let cutoff = now - self.window;
        while self.samples.front().is_some_and(|s| s.timestamp < cutoff) {
            self.samples.pop_front();
        }
    }

    /// Current speed in bytes per second.
    fn bytes_per_second(&self) -> f64 {
        if self.samples.len() < 2 {
            return 0.0;
        }

        let first = self.samples.front().unwrap();
        let last = self.samples.back().unwrap();

        let elapsed = last.timestamp.duration_since(first.timestamp).as_secs_f64();
        if elapsed < 0.001 {
            return 0.0;
        }

        let total_bytes: u64 = self.samples.iter().map(|s| s.bytes).sum();
        total_bytes as f64 / elapsed
    }
}

// ---------------------------------------------------------------------------
// ServerState
// ---------------------------------------------------------------------------

/// Runtime state for a configured NNTP server.
pub struct ServerState {
    pub config: Arc<ServerConfig>,
    pub active: bool,
    pub connections_active: u16,
    pub articles_tried: u64,
    pub articles_failed: u64,
    pub bytes_downloaded: u64,
    /// When the server penalty expires (None = no penalty).
    pub penalty_until: Option<Instant>,
    pub last_error: Option<String>,
    /// Connection pool for this server. Shared (`Arc`) so callers that keep
    /// `ServerState` behind a synchronous lock can acquire without holding it
    /// across an `.await`.
    pool: Arc<ConnectionPool>,
    /// Download speed tracker.
    speed: SpeedTracker,
}

impl ServerState {
    /// Create a new server state, including its connection pool.
    pub fn new(config: ServerConfig) -> Self {
        let config = Arc::new(config);
        let pool = Arc::new(ConnectionPool::new(Arc::clone(&config)));
        Self {
            active: config.enabled,
            connections_active: 0,
            articles_tried: 0,
            articles_failed: 0,
            bytes_downloaded: 0,
            penalty_until: None,
            last_error: None,
            pool,
            speed: SpeedTracker::new(),
            config,
        }
    }

    /// Check if the server is available (active and not penalized).
    pub fn is_available(&self) -> bool {
        self.active
            && self
                .penalty_until
                .is_none_or(|until| Instant::now() > until)
    }

    /// Apply a penalty to the server.
    pub fn penalize(&mut self, reason: &str, duration: Duration) {
        self.penalty_until = Some(Instant::now() + duration);
        self.last_error = Some(reason.to_string());
        warn!(
            server = %self.config.name,
            reason = %reason,
            duration_secs = duration.as_secs(),
            "Server penalized"
        );
    }

    /// Apply the appropriate penalty for a given error condition.
    pub fn penalize_for(&mut self, reason: &str) {
        let duration = if reason.contains("auth") || reason.contains("Auth") {
            info!(
                server = %self.config.name,
                host = %self.config.host,
                reason = %reason,
                "Applying AUTH penalty (10 min)"
            );
            PENALTY_AUTH
        } else if reason.contains("timeout") || reason.contains("Timeout") {
            info!(
                server = %self.config.name,
                host = %self.config.host,
                reason = %reason,
                "Applying TIMEOUT penalty (10 min)"
            );
            PENALTY_TIMEOUT
        } else {
            info!(
                server = %self.config.name,
                host = %self.config.host,
                reason = %reason,
                "Applying UNKNOWN penalty (3 min)"
            );
            PENALTY_UNKNOWN
        };
        self.penalize(reason, duration);
    }

    /// Clear the penalty.
    pub fn clear_penalty(&mut self) {
        if self.penalty_until.is_some() {
            info!(
                server = %self.config.name,
                host = %self.config.host,
                "Server penalty cleared"
            );
        }
        self.penalty_until = None;
        self.last_error = None;
    }

    /// Record a successful article download.
    pub fn record_success(&mut self, bytes: u64) {
        self.articles_tried += 1;
        self.bytes_downloaded += bytes;
        self.speed.record(bytes);
    }

    /// Record a failed article download.
    pub fn record_failure(&mut self) {
        self.articles_tried += 1;
        self.articles_failed += 1;
    }

    /// Failure ratio (0.0 to 1.0).
    pub fn failure_ratio(&self) -> f64 {
        if self.articles_tried == 0 {
            0.0
        } else {
            self.articles_failed as f64 / self.articles_tried as f64
        }
    }

    /// Current download speed in bytes per second (rolling average).
    pub fn speed_bps(&self) -> f64 {
        self.speed.bytes_per_second()
    }

    // ------------------------------------------------------------------
    // Pool integration
    // ------------------------------------------------------------------

    /// Acquire a connection from this server's pool.
    pub async fn acquire_connection(&mut self) -> NntpResult<PooledConnection> {
        debug!(
            server = %self.config.name,
            connections_active = self.connections_active,
            max_conns = self.config.connections,
            "Server: acquiring connection"
        );
        let conn = self.pool.acquire().await?;
        self.connections_active += 1;
        debug!(
            server = %self.config.name,
            connections_active = self.connections_active,
            "Server: connection acquired"
        );
        Ok(conn)
    }

    /// Return a connection to this server's pool.
    pub fn release_connection(&mut self, conn: PooledConnection) {
        self.pool.release(conn);
        self.connections_active = self.connections_active.saturating_sub(1);
        debug!(
            server = %self.config.name,
            connections_active = self.connections_active,
            "Server: connection released"
        );
    }

    /// Discard a broken connection (frees the pool slot).
    pub fn discard_connection(&mut self, conn: PooledConnection) {
        info!(
            server = %self.config.name,
            connections_active = self.connections_active,
            "Server: discarding broken connection"
        );
        self.pool.discard(conn);
        self.connections_active = self.connections_active.saturating_sub(1);
    }

    /// Close all idle connections in the pool.
    pub async fn close_idle_connections(&self) {
        self.pool.close_idle().await;
    }

    /// Number of idle connections in the pool.
    pub fn idle_connection_count(&self) -> usize {
        self.pool.idle_count()
    }

    /// Wait for the ramp-up delay (for callers creating connections outside the pool).
    pub async fn wait_for_ramp_up(&self) {
        self.pool.wait_for_ramp_up().await;
    }

    /// Access the underlying pool directly.
    pub fn pool(&self) -> &ConnectionPool {
        &self.pool
    }

    /// A shared handle to the pool, for acquiring outside a lock on `self`.
    pub(crate) fn shared_pool(&self) -> Arc<ConnectionPool> {
        Arc::clone(&self.pool)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ServerConfig;

    fn make_config() -> ServerConfig {
        ServerConfig {
            id: "srv-1".into(),
            name: "Test NNTP".into(),
            host: "127.0.0.1".into(),
            port: 563,
            ssl: false,
            ssl_verify: false,
            username: None,
            password: None,
            connections: 4,
            priority: 0,
            enabled: true,
            retention: 0,
            pipelining: 1,
            optional: false,
            compress: false,
            ramp_up_delay_ms: 0,
            recv_buffer_size: 0,
            proxy_url: None,
            trusted_fingerprint: None,
            connect_timeout_secs: 30,
        }
    }

    #[test]
    fn test_new_server_state() {
        let state = ServerState::new(make_config());
        assert!(state.active);
        assert_eq!(state.connections_active, 0);
        assert_eq!(state.articles_tried, 0);
        assert_eq!(state.articles_failed, 0);
        assert_eq!(state.bytes_downloaded, 0);
        assert!(state.penalty_until.is_none());
        assert!(state.last_error.is_none());
        assert_eq!(state.config.id, "srv-1");
    }

    #[test]
    fn test_new_disabled_server() {
        let mut cfg = make_config();
        cfg.enabled = false;
        let state = ServerState::new(cfg);
        assert!(!state.active);
        assert!(!state.is_available());
    }

    #[test]
    fn test_is_available_active_no_penalty() {
        let state = ServerState::new(make_config());
        assert!(state.is_available());
    }

    #[test]
    fn test_is_available_penalized() {
        let mut state = ServerState::new(make_config());
        state.penalize("test error", Duration::from_secs(60));
        assert!(!state.is_available());
        assert!(state.penalty_until.is_some());
        assert_eq!(state.last_error.as_deref(), Some("test error"));
    }

    #[test]
    fn test_penalize_expired() {
        let mut state = ServerState::new(make_config());
        // Penalize with zero duration — immediately expired
        state.penalty_until = Some(Instant::now() - Duration::from_secs(1));
        assert!(state.is_available());
    }

    #[test]
    fn test_penalize_for_auth() {
        let mut state = ServerState::new(make_config());
        state.penalize_for("Authentication failed");
        assert!(!state.is_available());
        // Auth penalty should be 10 minutes
        let until = state.penalty_until.unwrap();
        let remaining = until.duration_since(Instant::now());
        assert!(remaining > Duration::from_secs(500)); // ~10 min
    }

    #[test]
    fn test_penalize_for_timeout() {
        let mut state = ServerState::new(make_config());
        state.penalize_for("Connection timeout");
        let until = state.penalty_until.unwrap();
        let remaining = until.duration_since(Instant::now());
        assert!(remaining > Duration::from_secs(500));
    }

    #[test]
    fn test_penalize_for_unknown() {
        let mut state = ServerState::new(make_config());
        state.penalize_for("some random error");
        let until = state.penalty_until.unwrap();
        let remaining = until.duration_since(Instant::now());
        // Unknown = 3 minutes
        assert!(remaining > Duration::from_secs(150));
        assert!(remaining < Duration::from_secs(200));
    }

    #[test]
    fn test_clear_penalty() {
        let mut state = ServerState::new(make_config());
        state.penalize("error", Duration::from_secs(600));
        assert!(!state.is_available());

        state.clear_penalty();
        assert!(state.is_available());
        assert!(state.penalty_until.is_none());
        assert!(state.last_error.is_none());
    }

    #[test]
    fn test_record_success() {
        let mut state = ServerState::new(make_config());
        state.record_success(50000);
        assert_eq!(state.articles_tried, 1);
        assert_eq!(state.articles_failed, 0);
        assert_eq!(state.bytes_downloaded, 50000);

        state.record_success(30000);
        assert_eq!(state.articles_tried, 2);
        assert_eq!(state.bytes_downloaded, 80000);
    }

    #[test]
    fn test_record_failure() {
        let mut state = ServerState::new(make_config());
        state.record_failure();
        assert_eq!(state.articles_tried, 1);
        assert_eq!(state.articles_failed, 1);
    }

    #[test]
    fn test_failure_ratio() {
        let mut state = ServerState::new(make_config());
        assert_eq!(state.failure_ratio(), 0.0);

        state.record_success(1000);
        state.record_success(1000);
        state.record_failure();
        // 1 failure out of 3 tries
        let ratio = state.failure_ratio();
        assert!((ratio - 1.0 / 3.0).abs() < 0.001);
    }

    #[test]
    fn test_failure_ratio_all_failed() {
        let mut state = ServerState::new(make_config());
        state.record_failure();
        state.record_failure();
        assert_eq!(state.failure_ratio(), 1.0);
    }

    #[test]
    fn test_speed_bps_no_samples() {
        let state = ServerState::new(make_config());
        assert_eq!(state.speed_bps(), 0.0);
    }

    #[test]
    fn test_idle_connection_count() {
        let state = ServerState::new(make_config());
        assert_eq!(state.idle_connection_count(), 0);
    }
}
