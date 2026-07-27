//! Per-host connection rate limiter.
//!
//! Prevents thundering-herd connection storms by limiting how many
//! TCP+TLS+AUTH handshakes can run concurrently to the same NNTP host.
//! Also enforces a minimum delay between connection starts so providers
//! don't see a burst of simultaneous SYNs.
//!
//! This is a global singleton — all callers of `NntpConnection::connect()`
//! share the same gate regardless of which pool or download engine they use.

use std::collections::HashMap;
use std::sync::LazyLock;
use std::time::Duration;

use parking_lot::Mutex;
use tokio::sync::Semaphore;
use tokio::time::Instant;
use tracing::{debug, info};

/// Max concurrent connection attempts per host.
/// With 50 configured connections and ~200ms per handshake,
/// 5 concurrent = ~2s total ramp. Conservative enough for any provider.
const MAX_CONCURRENT_CONNECTS: usize = 5;

/// Minimum gap between successive connection starts to the same host.
/// Spreads SYN packets so the provider doesn't see a spike.
const MIN_CONNECT_INTERVAL: Duration = Duration::from_millis(100);

/// Global connection gate shared by all NNTP connections.
static GATE: LazyLock<ConnectGate> = LazyLock::new(ConnectGate::new);

/// Per-host state: concurrency semaphore + last-connect timestamp.
struct HostState {
    /// Limits concurrent connection attempts.
    semaphore: Semaphore,
    /// When the last connection attempt started (for pacing).
    last_connect: Mutex<Instant>,
}

impl HostState {
    fn new() -> Self {
        Self {
            semaphore: Semaphore::new(MAX_CONCURRENT_CONNECTS),
            last_connect: Mutex::new(Instant::now() - MIN_CONNECT_INTERVAL),
        }
    }
}

/// Manages per-host connection rate limiting.
struct ConnectGate {
    hosts: Mutex<HashMap<String, &'static HostState>>,
}

impl ConnectGate {
    fn new() -> Self {
        Self {
            hosts: Mutex::new(HashMap::new()),
        }
    }

    /// Get or create the state for a host.
    fn host_state(&self, host: &str) -> &'static HostState {
        let mut hosts = self.hosts.lock();
        if let Some(state) = hosts.get(host) {
            return state;
        }
        // Leak the HostState so it lives for 'static (one per unique host, bounded)
        let state: &'static HostState = Box::leak(Box::new(HostState::new()));
        hosts.insert(host.to_string(), state);
        state
    }
}

/// Acquire permission to connect to the given host.
///
/// This will:
/// 1. Wait for a concurrency slot (max `MAX_CONCURRENT_CONNECTS` per host)
/// 2. Enforce `MIN_CONNECT_INTERVAL` between connection starts
///
/// Returns a guard that must be held until the connection handshake completes.
/// Dropping the guard releases the concurrency slot.
pub async fn acquire(host: &str) -> ConnectPermit {
    let state = GATE.host_state(host);

    // 1. Wait for a concurrency slot
    let permit = state
        .semaphore
        .acquire()
        .await
        .expect("connect gate semaphore closed");

    // 2. Enforce minimum interval between connection starts
    let sleep_for = {
        let mut last = state.last_connect.lock();
        let now = Instant::now();
        let elapsed = now.duration_since(*last);
        if elapsed < MIN_CONNECT_INTERVAL {
            let delay = MIN_CONNECT_INTERVAL - elapsed;
            *last = now + delay;
            delay
        } else {
            *last = now;
            Duration::ZERO
        }
    };

    if !sleep_for.is_zero() {
        debug!(
            host = %host,
            delay_ms = sleep_for.as_millis(),
            "Connect gate: pacing connection start"
        );
        tokio::time::sleep(sleep_for).await;
    }

    info!(
        host = %host,
        available = state.semaphore.available_permits(),
        max = MAX_CONCURRENT_CONNECTS,
        "Connect gate: slot acquired"
    );

    ConnectPermit { _permit: permit }
}

/// Guard returned by [`acquire`]. Releasing it frees the concurrency slot.
pub struct ConnectPermit {
    _permit: tokio::sync::SemaphorePermit<'static>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn per_host_gate_paces_successive_connection_starts() {
        let host = format!("gate-test-{}", uuid::Uuid::new_v4());
        let _first = acquire(&host).await;
        let started = Instant::now();
        let _second = acquire(&host).await;
        assert!(started.elapsed() >= MIN_CONNECT_INTERVAL);
    }

    #[tokio::test]
    async fn hosts_do_not_share_pacing_state() {
        let first = format!("gate-test-{}", uuid::Uuid::new_v4());
        let second = format!("gate-test-{}", uuid::Uuid::new_v4());
        let _ = acquire(&first).await;
        let started = Instant::now();
        let _ = acquire(&second).await;
        assert!(started.elapsed() < MIN_CONNECT_INTERVAL);
    }
}
