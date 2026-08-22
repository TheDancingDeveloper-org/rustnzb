//! Async NNTP client with TLS, pipelining, connection pooling, and multi-server support.
//!
//! Modules:
//! - `config` — Server and article configuration types
//! - `error` — NNTP-specific error types
//! - `connection` — Single NNTP connection state machine (TCP/TLS, auth, article fetch)
//! - `pipeline` — Request pipelining (ARTICLE and STAT commands)
//! - `pool` — Per-server async connection pool
//! - `server` ��� Server health tracking, penalties, speed measurement
//! - `downloader` — Download orchestrator (assigns articles to servers with failover)

pub mod capabilities;
pub mod config;
pub mod connection;
pub mod downloader;
pub mod error;
pub mod overview;
pub mod pipeline;
pub mod pool;
pub mod server;

/// Test utilities: in-process mock NNTP server. Only available with the `test-support` feature.
#[cfg(any(test, feature = "test-support"))]
pub mod testutil;

pub use capabilities::NntpCapabilities;
pub use config::{Article, ListActiveEntry, ServerConfig};
pub use connection::{
    ArticleRange, ConnectionState, GroupResponse, HeaderEntry, NntpConnection, NntpResponse,
    XoverEntry,
};
pub use downloader::{ArticleResult, Downloader};
pub use error::{NntpError, NntpResult};
pub use overview::{
    DefectiveOverviewRow, DefectiveOverviewRowCode, LosslessOverviewRow, LosslessOverviewRows,
    OverviewFormat,
};
pub use pipeline::{Pipeline, StatPipeline, StatResult};
pub use pool::ConnectionPool;
pub use server::ServerState;
