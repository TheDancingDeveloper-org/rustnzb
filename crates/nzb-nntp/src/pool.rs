//! Per-server connection pool.
//!
//! Manages a configurable number of NNTP connections for a single server.
//! Connections are created on demand up to the configured limit, returned
//! to the pool after use, and replaced when they become unhealthy.
//!
//! A pool built with [`ConnectionPool::with_budget`] also takes a
//! [`BudgetPermit`] from the server's shared [`ServerBudget`] for every
//! socket it opens and holds it until the socket closes, idle or not, so the
//! pool and any other consumer of the same budget stay within the provider's
//! allowance together. A released socket is closed instead of parked only
//! when a waiter at another tier could actually take its permit.
//! A reader waiting for the budget also takes a socket parked in its own
//! pool by a reader at the same tier, which the budget would not hand on.

use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use tokio::sync::Semaphore;
use tracing::{debug, info, trace, warn};

use crate::budget::{BudgetPermit, BudgetTier, ServerBudget};
use crate::config::ServerConfig;

use crate::connection::{ConnectionState, NntpConnection};
use crate::error::{NntpError, NntpResult};

/// How long an idle connection can sit before we health-check it.
const IDLE_TIMEOUT: Duration = Duration::from_secs(60);

/// How long to wait for a connection from the pool before giving up.
const ACQUIRE_TIMEOUT: Duration = Duration::from_secs(30);

// ---------------------------------------------------------------------------
// Pooled connection wrapper
// ---------------------------------------------------------------------------

/// A connection checked out from the pool, with metadata.
///
/// When dropped without being returned via `release()` or `discard()`, the
/// semaphore permit is automatically freed so the pool slot is not leaked.
pub struct PooledConnection {
    /// The underlying NNTP connection.
    pub conn: NntpConnection,
    /// When this connection last completed an operation.
    pub last_used: Instant,
    /// Semaphore reference for automatic permit release on drop.
    semaphore: Option<Arc<Semaphore>>,
    /// The shared-budget permit this socket holds while it is open.
    budget_permit: Option<BudgetPermit>,
}

impl PooledConnection {
    /// Create a new pooled connection not tied to any pool's semaphore.
    /// Used by the downloader which manages its own connection lifecycle.
    pub fn unmanaged(conn: NntpConnection) -> Self {
        Self {
            conn,
            last_used: Instant::now(),
            semaphore: None,
            budget_permit: None,
        }
    }
}

impl Drop for PooledConnection {
    fn drop(&mut self) {
        if let Some(sem) = self.semaphore.take() {
            warn!(
                conn_id = %self.conn.server_id,
                "PooledConnection dropped without release/discard — freeing permit"
            );
            sem.add_permits(1);
        }
    }
}

// ---------------------------------------------------------------------------
// Connection pool
// ---------------------------------------------------------------------------

/// Per-server connection pool.
///
/// The pool holds up to `max_connections` connections. Callers `acquire()` a
/// connection, use it, then `release()` it back. Connections that have entered
/// an error state are discarded rather than returned to the pool.
pub struct ConnectionPool {
    /// Server configuration (immutable reference data).
    config: Arc<ServerConfig>,
    /// Idle connections ready to be handed out.
    idle: Mutex<Vec<PooledConnection>>,
    /// Semaphore limiting total connections (idle + checked-out).
    semaphore: Arc<Semaphore>,
    /// Total connections ever created (for naming/debug).
    created_count: Mutex<u32>,
    /// Timestamp of the last new connection creation (for ramp-up staggering).
    /// Uses `tokio::sync::Mutex` because it is held across an `.await` (the sleep).
    last_connect: tokio::sync::Mutex<Instant>,
    /// Minimum delay between new connection creations.
    ramp_up_delay: Duration,
    /// Shared per-server budget and the tier this pool draws from.
    budget: Option<(Arc<ServerBudget>, BudgetTier)>,
    /// Fired when a socket is parked, so an `acquire` waiting for the
    /// budget can take it instead.
    idle_ready: tokio::sync::Notify,
}

impl ConnectionPool {
    /// Create a new pool for the given server. No connections are opened yet.
    pub fn new(config: Arc<ServerConfig>) -> Self {
        let max_conns = config.connections as usize;
        let ramp_up_delay = Duration::from_millis(config.ramp_up_delay_ms as u64);
        Self {
            idle: Mutex::new(Vec::with_capacity(max_conns)),
            semaphore: Arc::new(Semaphore::new(max_conns)),
            created_count: Mutex::new(0),
            // Allow the first connection immediately
            last_connect: tokio::sync::Mutex::new(Instant::now() - Duration::from_secs(60)),
            ramp_up_delay,
            config,
            budget: None,
            idle_ready: tokio::sync::Notify::new(),
        }
    }

    /// Create a pool whose sockets also count against `budget`, taking
    /// permits at `tier`. The pool's own limit stays `connections`. While
    /// the pool exists the budget counts it as a second consumer
    /// ([`ServerBudget::has_pools`]).
    pub fn with_budget(
        config: Arc<ServerConfig>,
        budget: Arc<ServerBudget>,
        tier: BudgetTier,
    ) -> Self {
        budget.attach_pool();
        let mut pool = Self::new(config);
        pool.budget = Some((budget, tier));
        pool
    }

    /// Acquire a connected, ready connection from the pool.
    ///
    /// If an idle connection is available it is returned immediately (after a
    /// health check). Otherwise a new connection is created, up to the server
    /// limit. If all connection slots are in use, this waits until one is
    /// released.
    pub async fn acquire(&self) -> NntpResult<PooledConnection> {
        let available_permits = self.semaphore.available_permits();
        let idle_count = self.idle.lock().len();
        debug!(
            server = %self.config.name,
            available_permits,
            idle_count,
            max_conns = self.config.connections,
            "Pool acquire: waiting for slot"
        );

        // Wait for a connection slot
        let deadline = tokio::time::Instant::now() + ACQUIRE_TIMEOUT;
        let permit = tokio::time::timeout_at(deadline, self.semaphore.clone().acquire_owned())
            .await
            .map_err(|_| {
                warn!(
                    server = %self.config.name,
                    available_permits,
                    idle_count,
                    max_conns = self.config.connections,
                    "Pool acquire TIMED OUT after {}s — all {} slots busy",
                    ACQUIRE_TIMEOUT.as_secs(),
                    self.config.connections
                );
                NntpError::Timeout(format!(
                    "Timed out waiting for connection to {}",
                    self.config.name
                ))
            })?
            .map_err(|_| {
                NntpError::Connection(format!(
                    "Connection pool for {} is closed",
                    self.config.name
                ))
            })?;

        // Reuse a parked socket, or wait for a budget permit to open one.
        // While waiting, a socket parked in this pool by a reader at the same
        // tier is taken as soon as it is parked: the budget would not hand
        // that reader's permit to a same-tier waiter, so without this a
        // second concurrent reader would wait for some other consumer.
        // The budget wait is kept across wake-ups so the place in line holds.
        let mut budget_wait = None;
        let budget_permit = loop {
            let parked = self.idle_ready.notified();
            tokio::pin!(parked);
            parked.as_mut().enable();
            let maybe_idle = { self.idle.lock().pop() };
            if let Some(pooled) = maybe_idle {
                match self.check_idle(pooled).await {
                    Ok(mut pooled) => {
                        permit.forget(); // slot is now checked out
                        pooled.semaphore = Some(Arc::clone(&self.semaphore));
                        return Ok(pooled);
                    }
                    // A broken idle socket hands its budget permit to its
                    // replacement.
                    Err(Some(carried)) => break Some(carried),
                    Err(None) if self.budget.is_none() => break None,
                    Err(None) => {}
                }
            }
            let Some((budget, tier)) = &self.budget else {
                break None;
            };
            let wait = budget_wait.get_or_insert_with(|| Box::pin(budget.acquire(*tier)));
            tokio::select! {
                permit = wait => break Some(permit),
                () = &mut parked => {}
                () = tokio::time::sleep_until(deadline) => {
                    warn!(
                        server = %self.config.name,
                        tier = ?tier,
                        held = budget.held_total(),
                        total = budget.total(),
                        "Pool acquire TIMED OUT waiting for the shared connection budget"
                    );
                    return Err(NntpError::Timeout(format!(
                        "Timed out waiting for the connection budget of {}",
                        self.config.name
                    )));
                }
            }
        };
        drop(budget_wait);

        // Create a new connection
        info!(
            server = %self.config.name,
            "Pool: no reusable connection, creating new"
        );
        let conn = self.create_connection().await?;
        permit.forget(); // slot is now checked out

        Ok(PooledConnection {
            conn,
            last_used: Instant::now(),
            semaphore: Some(Arc::clone(&self.semaphore)),
            budget_permit,
        })
    }

    /// Health-check a socket taken from the idle list. Returns it ready for
    /// use, or, when it is broken, the budget permit it held.
    async fn check_idle(
        &self,
        mut pooled: PooledConnection,
    ) -> Result<PooledConnection, Option<BudgetPermit>> {
        // The drop guard stays disarmed until the connection is actually
        // handed out: if it is discarded here, the caller's owned slot
        // permit is what carries the slot (to the new connection, or back
        // to the semaphore on error).
        if pooled.conn.state == ConnectionState::Ready && pooled.conn.is_connected() {
            // If idle too long, do a quick liveness check
            if pooled.last_used.elapsed() > IDLE_TIMEOUT {
                info!(
                    server = %self.config.name,
                    conn_id = %pooled.conn.server_id,
                    idle_secs = pooled.last_used.elapsed().as_secs(),
                    "Pool: idle connection stale — health checking"
                );
                // Real round-trip (DATE); any reply = alive, I/O error = dead
                match pooled.conn.ping().await {
                    Ok(()) => {
                        debug!(
                            server = %self.config.name,
                            conn_id = %pooled.conn.server_id,
                            "Pool: health check passed, reusing"
                        );
                        pooled.last_used = Instant::now();
                        return Ok(pooled);
                    }
                    Err(e) => {
                        warn!(
                            server = %self.config.name,
                            conn_id = %pooled.conn.server_id,
                            error = %e,
                            "Pool: idle connection FAILED health check — creating new"
                        );
                    }
                }
            } else {
                debug!(
                    server = %self.config.name,
                    conn_id = %pooled.conn.server_id,
                    idle_secs = pooled.last_used.elapsed().as_secs(),
                    "Pool: reusing idle connection"
                );
                return Ok(pooled);
            }
        } else {
            warn!(
                server = %self.config.name,
                conn_id = %pooled.conn.server_id,
                state = ?pooled.conn.state,
                connected = pooled.conn.is_connected(),
                "Pool: idle connection in bad state — creating new"
            );
        }
        Err(pooled.budget_permit.take())
    }

    /// Return a connection to the pool after use.
    ///
    /// If the connection is still healthy it goes back to the idle list.
    /// If it is in an error state it is dropped and the slot is freed.
    pub fn release(&self, mut pooled: PooledConnection) {
        // Disarm the drop guard — we're handling the permit ourselves.
        pooled.semaphore = None;
        // A waiter at another tier could take this socket's budget permit:
        // close the socket so the permit passes on. Otherwise keep it open
        // for reuse; parking it costs no one anything.
        let yield_budget = pooled.budget_permit.is_some()
            && self
                .budget
                .as_ref()
                .is_some_and(|(budget, tier)| budget.wanted_elsewhere(*tier));
        if yield_budget {
            debug!(
                server = %self.config.name,
                "Pool: closing connection on release to hand its budget permit on"
            );
            drop(pooled);
            self.semaphore.add_permits(1);
        } else if pooled.conn.state == ConnectionState::Ready && pooled.conn.is_connected() {
            pooled.last_used = Instant::now();
            let idle_after = {
                let mut idle = self.idle.lock();
                idle.push(pooled);
                idle.len()
            };
            // Restore the semaphore permit so the next acquire() can proceed.
            self.semaphore.add_permits(1);
            // A reader of this pool waiting for the budget takes it.
            self.idle_ready.notify_one();
            debug!(
                server = %self.config.name,
                idle_count = idle_after,
                "Pool: connection released back to idle"
            );
        } else {
            warn!(
                server = %self.config.name,
                conn_id = %pooled.conn.server_id,
                state = ?pooled.conn.state,
                connected = pooled.conn.is_connected(),
                "Pool: discarding unhealthy connection on release"
            );
            // Drop the connection; free the semaphore slot
            drop(pooled);
            self.semaphore.add_permits(1);
        }
    }

    /// Discard a connection (e.g. after a fatal error) and free the slot.
    pub fn discard(&self, mut pooled: PooledConnection) {
        // Disarm the drop guard — we'll add the permit back explicitly.
        pooled.semaphore = None;
        info!(
            server = %self.config.name,
            conn_id = %pooled.conn.server_id,
            state = ?pooled.conn.state,
            "Pool: discarding connection (fatal error)"
        );
        drop(pooled);
        self.semaphore.add_permits(1);
    }

    /// Number of idle connections currently in the pool.
    pub fn idle_count(&self) -> usize {
        self.idle.lock().len()
    }

    /// Close all idle connections. In-use connections are unaffected.
    pub async fn close_idle(&self) {
        let conns: Vec<PooledConnection> = {
            let mut idle = self.idle.lock();
            idle.drain(..).collect()
        };
        let count = conns.len();
        // Idle connections do not hold a permit (`release` already returned
        // it when parking them), so closing them frees nothing.
        for mut c in conns {
            let _ = c.conn.quit().await;
        }
        if count > 0 {
            debug!(server = %self.config.name, count, "Closed idle connections");
        }
    }

    /// Close idle connections unused for longer than `max_idle`, returning
    /// their budget permits. Returns how many were closed.
    pub async fn close_idle_older_than(&self, max_idle: Duration) -> usize {
        let stale: Vec<PooledConnection> = {
            let mut idle = self.idle.lock();
            let (stale, fresh) = idle
                .drain(..)
                .partition(|c: &PooledConnection| c.last_used.elapsed() > max_idle);
            *idle = fresh;
            stale
        };
        let count = stale.len();
        for mut c in stale {
            let _ = c.conn.quit().await;
        }
        if count > 0 {
            debug!(server = %self.config.name, count, "Closed stale idle connections");
        }
        count
    }

    /// Close parked sockets while a waiter at another tier could take their
    /// budget permits. Returns how many were closed.
    pub async fn yield_idle_to_waiters(&self) -> usize {
        let Some((budget, tier)) = &self.budget else {
            return 0;
        };
        let mut closed = 0;
        loop {
            let conn = {
                let mut idle = self.idle.lock();
                if idle.is_empty() || !budget.wanted_elsewhere(*tier) {
                    break;
                }
                idle.remove(0)
            };
            let mut conn = conn;
            // Dropping the connection returns its permit to the waiter.
            let _ = conn.conn.quit().await;
            drop(conn);
            closed += 1;
        }
        if closed > 0 {
            debug!(
                server = %self.config.name,
                closed,
                "Pool: closed idle connections to hand their budget permits on"
            );
        }
        closed
    }

    /// Look after a budgeted pool's parked sockets until the future is
    /// dropped: hand a parked socket's permit to a waiter at another tier as
    /// soon as one could take it, and close sockets idle longer than
    /// `max_idle`. Spawn it next to the pool and abort it to stop.
    pub async fn manage_idle(&self, max_idle: Duration) {
        const POLL: Duration = Duration::from_secs(1);
        let mut last_sweep = Instant::now();
        loop {
            match &self.budget {
                Some((budget, _)) => {
                    tokio::select! {
                        () = budget.contended() => {}
                        () = tokio::time::sleep(POLL) => {}
                    }
                }
                None => tokio::time::sleep(POLL).await,
            }
            self.yield_idle_to_waiters().await;
            if last_sweep.elapsed() >= max_idle / 2 {
                self.close_idle_older_than(max_idle).await;
                last_sweep = Instant::now();
            }
        }
    }

    /// The server configuration for this pool.
    pub fn config(&self) -> &ServerConfig {
        &self.config
    }

    /// Number of available semaphore permits (for testing).
    #[cfg(test)]
    pub(crate) fn available_permits(&self) -> usize {
        self.semaphore.available_permits()
    }

    /// Wait for the ramp-up delay to elapse since the last connection was opened.
    ///
    /// Call this before creating a connection outside the pool (e.g. in download
    /// engine workers) to respect ramp-up timing and avoid connection bursts.
    pub async fn wait_for_ramp_up(&self) {
        if self.ramp_up_delay.is_zero() {
            return;
        }
        let mut last = self.last_connect.lock().await;
        let elapsed = last.elapsed();
        if elapsed < self.ramp_up_delay {
            let wait = self.ramp_up_delay - elapsed;
            trace!(
                server = %self.config.name,
                wait_ms = wait.as_millis(),
                "Ramp-up: waiting before new connection"
            );
            tokio::time::sleep(wait).await;
        }
        *last = Instant::now();
    }

    /// The configured ramp-up delay between new connections.
    pub fn ramp_up_delay(&self) -> Duration {
        self.ramp_up_delay
    }

    // ------------------------------------------------------------------
    // Internal
    // ------------------------------------------------------------------

    /// Create and connect a new NNTP connection.
    ///
    /// Applies ramp-up delay to stagger connection establishment and avoid
    /// bursting the server with simultaneous TCP+TLS handshakes.
    async fn create_connection(&self) -> NntpResult<NntpConnection> {
        // Enforce ramp-up delay between new connections
        self.wait_for_ramp_up().await;

        let idx = {
            let mut count = self.created_count.lock();
            *count += 1;
            *count
        };

        let conn_id = format!("{}#{}", self.config.id, idx);
        info!(
            server = %self.config.name,
            conn_id = %conn_id,
            host = %self.config.host,
            port = self.config.port,
            total_created = idx,
            "Pool: creating new NNTP connection"
        );

        let mut conn = NntpConnection::new(conn_id.clone());
        conn.connect(&self.config).await.inspect_err(|e| {
            warn!(
                server = %self.config.name,
                conn_id = %conn_id,
                error = %e,
                "Pool: new connection FAILED"
            );
            // The caller's owned permit is dropped on this error path,
            // which frees the slot; adding one here would double-count.
        })?;

        info!(
            server = %self.config.name,
            conn_id = %conn_id,
            "Pool: new connection ready"
        );
        Ok(conn)
    }
}

impl Drop for ConnectionPool {
    fn drop(&mut self) {
        if let Some((budget, _)) = &self.budget {
            budget.detach_pool();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{MockConfig, MockNntpServer, test_config};

    #[tokio::test]
    async fn test_pool_new() {
        let config = Arc::new(test_config(12345));
        let pool = ConnectionPool::new(config.clone());
        assert_eq!(pool.idle_count(), 0);
        assert_eq!(pool.config().id, "test-server");
        assert_eq!(pool.available_permits(), 4); // connections = 4
    }

    #[tokio::test]
    async fn test_pool_acquire_creates_connection() {
        let server = MockNntpServer::start(MockConfig::default()).await;
        let config = Arc::new(test_config(server.port()));
        let pool = ConnectionPool::new(config);

        let pooled = pool.acquire().await.unwrap();
        assert_eq!(pooled.conn.state, ConnectionState::Ready);
        assert!(pooled.conn.is_connected());
        assert_eq!(pool.idle_count(), 0);

        // Release it back
        pool.release(pooled);
        assert_eq!(pool.idle_count(), 1);
    }

    #[tokio::test]
    async fn test_pool_release_and_reuse() {
        let server = MockNntpServer::start(MockConfig::default()).await;
        let config = Arc::new(test_config(server.port()));
        let pool = ConnectionPool::new(config);

        // Acquire and release
        let pooled = pool.acquire().await.unwrap();
        let first_id = pooled.conn.server_id.clone();
        pool.release(pooled);
        assert_eq!(pool.idle_count(), 1);

        // Acquire again — should reuse the idle connection
        let pooled = pool.acquire().await.unwrap();
        assert_eq!(pooled.conn.server_id, first_id);
        assert_eq!(pool.idle_count(), 0);

        pool.release(pooled);
    }

    #[tokio::test]
    async fn test_pool_discard_frees_slot() {
        let server = MockNntpServer::start(MockConfig::default()).await;
        let mut cfg = test_config(server.port());
        cfg.connections = 2;
        let pool = ConnectionPool::new(Arc::new(cfg));

        let c1 = pool.acquire().await.unwrap();
        let c2 = pool.acquire().await.unwrap();
        assert_eq!(pool.available_permits(), 0);

        // Discard one — frees a permit
        pool.discard(c1);
        assert_eq!(pool.available_permits(), 1);
        assert_eq!(pool.idle_count(), 0);

        pool.release(c2);
    }

    #[tokio::test]
    async fn test_pool_health_check_reuses_live_stale_connection() {
        let server = MockNntpServer::start(MockConfig::default()).await;
        let pool = ConnectionPool::new(Arc::new(test_config(server.port())));

        let c1 = pool.acquire().await.unwrap();
        let first_id = c1.conn.server_id.clone();
        pool.release(c1);
        pool.idle.lock()[0].last_used = Instant::now() - IDLE_TIMEOUT - Duration::from_secs(1);

        let c2 = pool.acquire().await.unwrap();
        assert_eq!(c2.conn.server_id, first_id);
        assert_eq!(c2.conn.state, ConnectionState::Ready);
        pool.release(c2);
    }

    #[tokio::test]
    async fn test_pool_health_check_detects_dead_idle_connection_without_stat() {
        // Server closes each session right after the CAPABILITIES exchange,
        // so a parked connection is dead by the time it is health-checked.
        let server = MockNntpServer::start(MockConfig {
            close_after_n_commands: Some(1),
            ..Default::default()
        })
        .await;
        let pool = ConnectionPool::new(Arc::new(test_config(server.port())));

        let mut c1 = pool.acquire().await.unwrap();
        let first_id = c1.conn.server_id.clone();
        // A server that doesn't advertise STAT must still be probed with
        // real I/O rather than "passing" without touching the socket.
        let mut caps = crate::capabilities::NntpCapabilities::default_assumed();
        caps.have_stat = false;
        c1.conn.set_capabilities_for_test(caps);
        pool.release(c1);
        pool.idle.lock()[0].last_used = Instant::now() - IDLE_TIMEOUT - Duration::from_secs(1);

        let c2 = pool.acquire().await.unwrap();
        assert_ne!(
            c2.conn.server_id, first_id,
            "dead idle connection passed the health check and was reused"
        );
        pool.discard(c2);
    }

    #[tokio::test]
    async fn test_pool_close_idle() {
        let server = MockNntpServer::start(MockConfig::default()).await;
        let config = Arc::new(test_config(server.port()));
        let pool = ConnectionPool::new(config);

        // Create and release two connections
        let c1 = pool.acquire().await.unwrap();
        let c2 = pool.acquire().await.unwrap();
        pool.release(c1);
        pool.release(c2);
        assert_eq!(pool.idle_count(), 2);

        assert_eq!(pool.available_permits(), 4);

        // Close all idle
        pool.close_idle().await;
        assert_eq!(pool.idle_count(), 0);
        // Idle connections do not hold permits, so closing them must not
        // add any: the pool still allows exactly `connections` slots.
        assert_eq!(pool.available_permits(), 4);
    }

    #[tokio::test]
    async fn test_pool_failed_connect_does_not_inflate_permits() {
        // Bind then drop a listener so the port refuses connections.
        let port = {
            let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            l.local_addr().unwrap().port()
        };
        let pool = ConnectionPool::new(Arc::new(test_config(port)));
        assert_eq!(pool.available_permits(), 4);

        for _ in 0..3 {
            assert!(pool.acquire().await.is_err());
            assert_eq!(pool.available_permits(), 4);
        }
    }

    #[tokio::test]
    async fn test_pool_bad_idle_connection_does_not_inflate_permits() {
        let server = MockNntpServer::start(MockConfig::default()).await;
        let pool = ConnectionPool::new(Arc::new(test_config(server.port())));

        let c1 = pool.acquire().await.unwrap();
        pool.release(c1);
        assert_eq!(pool.idle_count(), 1);
        assert_eq!(pool.available_permits(), 4);

        // Poison the parked connection so acquire() must replace it.
        pool.idle.lock()[0].conn.state = ConnectionState::Error;

        let c2 = pool.acquire().await.unwrap();
        assert_eq!(c2.conn.state, ConnectionState::Ready);
        assert_eq!(pool.idle_count(), 0);
        // Exactly one connection is checked out.
        assert_eq!(pool.available_permits(), 3);

        pool.release(c2);
        assert_eq!(pool.available_permits(), 4);
    }

    fn open_budget(total: usize) -> Arc<ServerBudget> {
        ServerBudget::new(
            total,
            crate::budget::BudgetPolicy {
                playback_reserve_pct: 0,
                repair_reserve_pct: 0,
            },
        )
    }

    #[tokio::test]
    async fn test_pool_with_budget_holds_permit_while_socket_open() {
        let server = MockNntpServer::start(MockConfig::default()).await;
        let budget = open_budget(2);
        let pool = ConnectionPool::with_budget(
            Arc::new(test_config(server.port())),
            Arc::clone(&budget),
            BudgetTier::Playback,
        );

        let c1 = pool.acquire().await.unwrap();
        assert_eq!(budget.held(BudgetTier::Playback), 1);
        pool.release(c1);
        assert_eq!(
            budget.held_total(),
            1,
            "an idle socket is still open at the provider"
        );

        // Reusing the idle socket takes no second permit.
        let c2 = pool.acquire().await.unwrap();
        assert_eq!(budget.held_total(), 1);
        pool.release(c2);

        assert_eq!(pool.close_idle_older_than(Duration::ZERO).await, 1);
        assert_eq!(budget.held_total(), 0);

        let c3 = pool.acquire().await.unwrap();
        pool.discard(c3);
        assert_eq!(budget.held_total(), 0);
    }

    #[tokio::test]
    async fn test_pool_waits_for_budget_held_by_another_consumer() {
        let server = MockNntpServer::start(MockConfig::default()).await;
        let budget = open_budget(1);
        let pool = Arc::new(ConnectionPool::with_budget(
            Arc::new(test_config(server.port())),
            Arc::clone(&budget),
            BudgetTier::Playback,
        ));
        let elsewhere = budget.acquire(BudgetTier::Download).await;

        let waiter = tokio::spawn({
            let pool = Arc::clone(&pool);
            async move { pool.acquire().await }
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            !waiter.is_finished(),
            "pool opened a socket over the budget"
        );

        drop(elsewhere);
        let conn = tokio::time::timeout(Duration::from_secs(5), waiter)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(budget.held(BudgetTier::Playback), 1);
        pool.release(conn);
    }

    #[tokio::test]
    async fn test_pool_reuses_socket_while_an_ungrantable_download_waits() {
        let server = MockNntpServer::start(MockConfig::default()).await;
        let budget = ServerBudget::new(
            4,
            crate::budget::BudgetPolicy {
                playback_reserve_pct: 25,
                repair_reserve_pct: 0,
            },
        );
        // Downloads hold all they may and one more waits for the floor.
        let mut downloads = Vec::new();
        for _ in 0..3 {
            downloads.push(budget.acquire(BudgetTier::Download).await);
        }
        let queued = tokio::spawn({
            let budget = Arc::clone(&budget);
            async move { budget.acquire(BudgetTier::Download).await }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;

        let pool = ConnectionPool::with_budget(
            Arc::new(test_config(server.port())),
            Arc::clone(&budget),
            BudgetTier::Playback,
        );
        for _ in 0..20 {
            let conn = pool.acquire().await.unwrap();
            pool.release(conn);
        }
        assert_eq!(
            server.accepted_connections(),
            1,
            "playback reconnected although the waiting download could not take its permit"
        );
        assert!(!queued.is_finished());
        queued.abort();
    }

    #[tokio::test]
    async fn test_pool_closes_borrowed_socket_for_a_grantable_waiter() {
        let server = MockNntpServer::start(MockConfig::default()).await;
        let budget = open_budget(1);
        let pool = ConnectionPool::with_budget(
            Arc::new(test_config(server.port())),
            Arc::clone(&budget),
            BudgetTier::Playback,
        );
        let conn = pool.acquire().await.unwrap();
        let queued = tokio::spawn({
            let budget = Arc::clone(&budget);
            async move { budget.acquire(BudgetTier::Download).await }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        pool.release(conn);
        assert_eq!(pool.idle_count(), 0, "the borrowed socket was parked");
        let permit = tokio::time::timeout(Duration::from_secs(5), queued)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(permit.tier(), BudgetTier::Download);
    }

    #[tokio::test]
    async fn test_pool_manage_idle_hands_a_parked_socket_to_a_waiter() {
        let server = MockNntpServer::start(MockConfig::default()).await;
        let budget = open_budget(1);
        let pool = Arc::new(ConnectionPool::with_budget(
            Arc::new(test_config(server.port())),
            Arc::clone(&budget),
            BudgetTier::Import,
        ));
        let conn = pool.acquire().await.unwrap();
        pool.release(conn);
        assert_eq!(pool.idle_count(), 1);
        let manager = tokio::spawn({
            let pool = Arc::clone(&pool);
            async move { pool.manage_idle(Duration::from_secs(60)).await }
        });
        // Let the manager start listening for contention.
        tokio::time::sleep(Duration::from_millis(50)).await;

        let permit = tokio::time::timeout(
            Duration::from_millis(500),
            budget.acquire(BudgetTier::Playback),
        )
        .await
        .expect("the parked socket kept its permit from a waiting tier");
        assert_eq!(permit.tier(), BudgetTier::Playback);
        assert_eq!(pool.idle_count(), 0);
        manager.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_pool_concurrent_readers_share_a_parked_socket_under_download_saturation() {
        let server = MockNntpServer::start(MockConfig::default()).await;
        let budget = ServerBudget::new(
            4,
            crate::budget::BudgetPolicy {
                playback_reserve_pct: 25,
                repair_reserve_pct: 0,
            },
        );
        // Busy downloads hold all they may, and one more waits for a permit
        // the whole time, so no download permit frees up.
        let mut downloads = Vec::new();
        for _ in 0..3 {
            downloads.push(budget.acquire(BudgetTier::Download).await);
        }
        let queued = tokio::spawn({
            let budget = Arc::clone(&budget);
            async move { budget.acquire(BudgetTier::Download).await }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;

        for tier in [BudgetTier::Playback, BudgetTier::Import] {
            let before = server.accepted_connections();
            let pool = Arc::new(ConnectionPool::with_budget(
                Arc::new(test_config(server.port())),
                Arc::clone(&budget),
                tier,
            ));
            let manager = tokio::spawn({
                let pool = Arc::clone(&pool);
                async move { pool.manage_idle(Duration::from_secs(60)).await }
            });
            // Four readers at once, as a stream reads segments in parallel.
            // The floor is one permit, so they take turns on one socket.
            let readers: Vec<_> = (0..4)
                .map(|r| {
                    let pool = Arc::clone(&pool);
                    tokio::spawn(async move {
                        for i in 0..10 {
                            let conn = tokio::time::timeout(Duration::from_secs(2), pool.acquire())
                                .await
                                .unwrap_or_else(|_| panic!("{tier:?} reader {r} read {i} starved"))
                                .unwrap();
                            tokio::time::sleep(Duration::from_millis(2)).await;
                            pool.release(conn);
                        }
                    })
                })
                .collect();
            for reader in readers {
                tokio::time::timeout(Duration::from_secs(10), reader)
                    .await
                    .expect("readers did not finish")
                    .unwrap();
            }
            assert_eq!(
                server.accepted_connections() - before,
                1,
                "{tier:?} readers must take turns on the parked socket, not reconnect"
            );
            assert_eq!(budget.held(BudgetTier::Download), 3);
            assert!(budget.held_total() <= 4);
            manager.abort();
            pool.close_idle_older_than(Duration::ZERO).await;
        }
        assert!(!queued.is_finished());
        queued.abort();
    }

    #[tokio::test]
    async fn test_pool_idle_count_tracking() {
        let server = MockNntpServer::start(MockConfig::default()).await;
        let config = Arc::new(test_config(server.port()));
        let pool = ConnectionPool::new(config);

        assert_eq!(pool.idle_count(), 0);

        let c1 = pool.acquire().await.unwrap();
        assert_eq!(pool.idle_count(), 0);

        let c2 = pool.acquire().await.unwrap();
        assert_eq!(pool.idle_count(), 0);

        pool.release(c1);
        assert_eq!(pool.idle_count(), 1);

        pool.release(c2);
        assert_eq!(pool.idle_count(), 2);
    }
}
