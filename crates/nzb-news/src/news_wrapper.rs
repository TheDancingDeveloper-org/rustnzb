//! `NewsWrapper` — single NNTP connection + protocol-level state machine.
//!
//! Responsibilities:
//!
//! - **Owns** one `NntpConnection` and drives its lifecycle (connect, fetch,
//!   reset, quit).
//! - **Tracks** per-connection bookkeeping: the article currently in flight,
//!   total bytes received, the last fetch timestamp (for idle-eviction), and
//!   the consecutive failure count (`bad_cons`, bubbled up to `Server`).
//! - **Exposes** two reset operations:
//!   - [`NewsWrapper::hard_reset`] — tear the socket down, set state to
//!     `Disconnected`, zero the in-flight article. Used after any unrecoverable
//!     error (socket closed, protocol violation, auth expired).
//!   - [`NewsWrapper::soft_reset`] — keep the socket open, drop in-flight
//!     article state. Used when we want to reuse the TCP connection but
//!     abandon the current request (e.g. caller cancelled mid-fetch).
//!
//! All I/O errors and protocol violations bubble up as `NntpError`; the caller
//! decides whether to retire the wrapper, retry the article on another
//! server, or increment the server's `bad_cons`.
//!
//! No public async fetch methods yet — those land together with the rest of
//! the downloader plumbing in a later phase. This module defines the shape
//! of a wrapper so [`crate::server::Server`] can own a pool of them.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use nzb_nntp::config::ServerConfig;
use nzb_nntp::connection::{ConnectionState, NntpConnection};
use nzb_nntp::error::{NntpError, NntpResult};
use tracing::{debug, warn};

/// Opaque per-wrapper identifier. Usually a small integer produced by the
/// owning `Server` (1..=connections_per_server).
pub type WrapperId = u32;

/// A single NNTP connection + the state needed to drive it in the dispatcher.
pub struct NewsWrapper {
    /// Server this wrapper belongs to (matches [`ServerConfig::id`]).
    pub server_id: String,
    /// Short per-server integer id for logging (1..=N, where N is the
    /// server's connection cap).
    pub id: WrapperId,

    /// Underlying NNTP connection. `Option` because we may not have a
    /// connected socket after a hard reset.
    pub(crate) conn: Option<NntpConnection>,

    /// Message-id of the article currently being fetched, if any. Set when
    /// the wrapper enters the `Busy` state; cleared on success, failure, or
    /// reset.
    pub in_flight: Option<String>,

    /// Wall-clock moment of the last successful read from the socket. Used
    /// by the [`super::server::Server`]'s idle-eviction watchdog.
    last_activity: AtomicU64,
    activity_epoch: Instant,

    /// Cumulative bytes received through this wrapper (observability only).
    pub bytes_rx: AtomicU64,

    /// Consecutive failure count. Reset to zero after every successful fetch
    /// of a live article. Incremented on reconnect failure, socket close,
    /// unexpected protocol response, etc. The [`super::server::Server`] uses
    /// this + a per-server threshold to decide when to pause the server.
    pub(crate) bad_cons: AtomicU32,
}

impl std::fmt::Debug for NewsWrapper {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NewsWrapper")
            .field("server_id", &self.server_id)
            .field("id", &self.id)
            .field("has_conn", &self.conn.is_some())
            .field("conn_state", &self.state())
            .field("in_flight", &self.in_flight)
            .field("bytes_rx", &self.bytes_rx.load(Ordering::Relaxed))
            .field("bad_cons", &self.bad_cons.load(Ordering::Relaxed))
            .finish()
    }
}

impl NewsWrapper {
    /// Construct a fresh wrapper in the `Disconnected` state.
    pub fn new(server_id: impl Into<String>, id: WrapperId) -> Self {
        let activity_epoch = Instant::now();
        Self {
            server_id: server_id.into(),
            id,
            conn: None,
            in_flight: None,
            last_activity: AtomicU64::new(0),
            activity_epoch,
            bytes_rx: AtomicU64::new(0),
            bad_cons: AtomicU32::new(0),
        }
    }

    /// Current connection state (or `Disconnected` if no conn is attached).
    pub fn state(&self) -> ConnectionState {
        self.conn
            .as_ref()
            .map(|c| c.state)
            .unwrap_or(ConnectionState::Disconnected)
    }

    /// Is this wrapper currently holding an open, idle connection?
    pub fn is_idle(&self) -> bool {
        matches!(self.state(), ConnectionState::Ready)
    }

    /// Is this wrapper currently transferring data?
    pub fn is_busy(&self) -> bool {
        matches!(self.state(), ConnectionState::Busy)
    }

    /// Has this wrapper got an open (even if busy) socket?
    pub fn is_connected(&self) -> bool {
        matches!(
            self.state(),
            ConnectionState::Ready | ConnectionState::Busy | ConnectionState::Authenticating
        )
    }

    /// Seconds since the last successful byte was read. Returns the total
    /// elapsed time since construction if the connection has never produced
    /// data (that's a stronger signal than zero — a brand-new wrapper has
    /// not been asked to do anything yet and shouldn't be evicted immediately).
    pub fn idle_for(&self) -> Duration {
        let last = self.last_activity.load(Ordering::Relaxed);
        let now_ms = self.activity_epoch.elapsed().as_millis() as u64;
        if last == 0 {
            // Never active: idle since construction.
            self.activity_epoch.elapsed()
        } else {
            Duration::from_millis(now_ms.saturating_sub(last))
        }
    }

    /// Refresh the idle watchdog timestamp. Internal hook used by fetch/
    /// receive paths after every successful line read. External callers can
    /// also poke this when returning a wrapper to the idle set.
    pub(crate) fn touch_activity(&self) {
        let now_ms = self.activity_epoch.elapsed().as_millis() as u64;
        // Never store zero — zero is our "never active" sentinel.
        let stamp = now_ms.max(1);
        self.last_activity.store(stamp, Ordering::Relaxed);
    }

    /// Current consecutive-failure count for this connection.
    pub fn bad_cons(&self) -> u32 {
        self.bad_cons.load(Ordering::Relaxed)
    }

    /// Reset the consecutive-failure count — call after any successful fetch.
    pub fn clear_bad_cons(&self) {
        self.bad_cons.store(0, Ordering::Relaxed);
    }

    /// Increment the consecutive-failure count. Returns the new value.
    pub fn bump_bad_cons(&self) -> u32 {
        self.bad_cons.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// Connect (or reconnect) this wrapper to the given server.
    ///
    /// If an existing connection is attached, it is replaced — callers
    /// should normally [`hard_reset`](Self::hard_reset) first to make the
    /// intent explicit.
    pub async fn connect(&mut self, config: &ServerConfig) -> NntpResult<()> {
        let mut conn = NntpConnection::new(self.server_id.clone());
        conn.connect(config).await.inspect_err(|e| {
            let n = self.bump_bad_cons();
            warn!(
                server = %self.server_id,
                wrapper_id = self.id,
                bad_cons = n,
                error = %e,
                "connect failed"
            );
        })?;
        self.conn = Some(conn);
        self.in_flight = None;
        // First successful socket event — start the idle clock.
        self.touch_activity();
        Ok(())
    }

    /// Hard reset: close the socket, drop the connection, zero the in-flight
    /// article and any NNTP state. The wrapper is left in the `Disconnected`
    /// state — a subsequent `connect()` is required before it can be used
    /// again.
    ///
    /// This is the path for unrecoverable errors — protocol violations,
    /// socket close, auth expired, or explicit server disable.
    pub async fn hard_reset(&mut self) {
        if let Some(mut conn) = self.conn.take() {
            // Best-effort QUIT — we don't care if it fails, the goal is to
            // drop the socket cleanly when possible so servers can release
            // the slot immediately instead of waiting for their idle timeout.
            if matches!(conn.state, ConnectionState::Ready | ConnectionState::Busy) {
                let _ = tokio::time::timeout(Duration::from_secs(2), conn.quit()).await;
            }
            debug!(
                server = %self.server_id,
                wrapper_id = self.id,
                "NewsWrapper hard_reset"
            );
            drop(conn);
        }
        self.in_flight = None;
    }

    /// Soft reset: keep the socket open, drop any in-flight request state
    /// and return the underlying `NntpConnection` to the `Ready` state.
    /// Used when the caller has decided to abandon the current article
    /// (e.g. the job was cancelled or paused) but wants to reuse the TCP
    /// connection for future work.
    ///
    /// If the connection is not currently in a recoverable state
    /// (`Disconnected`, `Error`), this function falls through to a hard
    /// reset so callers get consistent post-conditions.
    pub async fn soft_reset(&mut self) {
        let Some(conn) = self.conn.as_mut() else {
            return;
        };
        match conn.state {
            ConnectionState::Ready => {
                // Already idle, nothing to discard other than the
                // wrapper-side in-flight record.
                self.in_flight = None;
            }
            ConnectionState::Busy => {
                // A fetch is in flight. Trying to read the remainder on the
                // wire is complex and not worth the risk — we may be deep
                // inside a multi-line body, and discarding unknown bytes
                // leaves the NNTP state machine desynced. Promote to hard
                // reset.
                self.hard_reset().await;
            }
            _ => {
                self.hard_reset().await;
            }
        }
    }

    /// Borrow the inner NNTP connection mutably, if attached.
    /// Fetching / receiving happens on the returned reference.
    pub fn conn_mut(&mut self) -> Option<&mut NntpConnection> {
        self.conn.as_mut()
    }

    /// Borrow the inner NNTP connection immutably, if attached.
    pub fn conn(&self) -> Option<&NntpConnection> {
        self.conn.as_ref()
    }

    /// Wrap a raw protocol error from a fetch attempt: bump `bad_cons`,
    /// clear the in-flight slot, and force a hard reset on the connection
    /// if the error is unrecoverable. Returns the error unchanged so callers
    /// can still `?` it.
    pub async fn on_fetch_error(&mut self, err: NntpError) -> NntpError {
        self.in_flight = None;
        match &err {
            NntpError::Io(_)
            | NntpError::Connection(_)
            | NntpError::Tls(_)
            | NntpError::AuthRequired(_)
            | NntpError::PermissionDenied(_)
            | NntpError::ServiceUnavailable(_)
            | NntpError::Protocol(_)
            | NntpError::Timeout(_) => {
                self.bump_bad_cons();
                self.hard_reset().await;
            }
            // Article-not-found / no-article-selected / NoSuchGroup are
            // article-level failures — the socket is fine.
            _ => {}
        }
        err
    }

    /// Mark this wrapper as having completed a successful fetch of
    /// `message_id` delivering `bytes` of data. Clears in-flight state,
    /// updates the activity watchdog, and zeroes `bad_cons`.
    pub fn on_fetch_success(&self, bytes: u64) {
        self.bytes_rx.fetch_add(bytes, Ordering::Relaxed);
        self.touch_activity();
        self.clear_bad_cons();
    }

    /// Helper for `Server`: attach an external socket-liveness heartbeat
    /// (shared with a supervisor that checks for zombied connections).
    pub fn set_io_heartbeat(&mut self, timestamp_ms: Arc<AtomicU64>, epoch: Instant) {
        if let Some(conn) = self.conn.as_mut() {
            conn.set_io_heartbeat(timestamp_ms, epoch);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_disconnected_with_zero_counters() {
        let w = NewsWrapper::new("srv1", 1);
        assert_eq!(w.server_id, "srv1");
        assert_eq!(w.id, 1);
        assert_eq!(w.state(), ConnectionState::Disconnected);
        assert!(!w.is_idle());
        assert!(!w.is_busy());
        assert!(!w.is_connected());
        assert_eq!(w.bad_cons(), 0);
        assert_eq!(w.bytes_rx.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn bad_cons_accumulates_then_clears() {
        let w = NewsWrapper::new("srv1", 1);
        assert_eq!(w.bump_bad_cons(), 1);
        assert_eq!(w.bump_bad_cons(), 2);
        assert_eq!(w.bump_bad_cons(), 3);
        w.clear_bad_cons();
        assert_eq!(w.bad_cons(), 0);
    }

    #[test]
    fn idle_clock_starts_positive_and_ticks_only_after_touch() {
        let w = NewsWrapper::new("srv1", 1);
        let first = w.idle_for();
        assert!(first >= Duration::ZERO);
        w.touch_activity();
        // Immediately after a touch the watchdog should see near-zero
        // idle time — at most a few millis of drift.
        let near_zero = w.idle_for();
        assert!(near_zero < Duration::from_millis(50));
    }

    #[test]
    fn on_fetch_success_refreshes_activity_and_bytes() {
        let w = NewsWrapper::new("srv1", 1);
        w.bump_bad_cons();
        w.on_fetch_success(4096);
        assert_eq!(w.bad_cons(), 0);
        assert_eq!(w.bytes_rx.load(Ordering::Relaxed), 4096);
    }
}
