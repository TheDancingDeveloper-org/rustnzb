//! nzb-dispatch — article-level download dispatcher for the nzb-* engine.
//!
//! Responsibilities:
//! - Per-server worker pool (spawn/retire workers as server config changes)
//! - Priority-aware article-fetch scheduling with retry across servers
//! - Connection tracking, circuit breaker, byte-level heartbeat
//! - Job context lifecycle (register on submit, drain on completion)
//! - Hopeless-job detection hooks exposed via progress events
//!
//! Boundary: the [`dispatch_engine::DispatchEngine`] trait is the contract
//! consumed by the job queue / orchestrator layer. Progress is reported
//! back via a `mpsc::Sender<dispatch_engine::ProgressUpdate>` channel per job.
//!
//! This crate does NOT own job persistence, post-processing, or the public
//! HTTP API. Those live in `nzb-queue`, `nzb-postproc`, and `nzb-engine`.

pub mod article_failure;
pub mod bandwidth;
pub mod dispatch_engine;
pub mod download_engine;
mod held_volumes;
pub mod slice_layout;
mod slice_tracker;
pub mod telemetry;
pub mod util;

// Convenience re-exports — the types downstream crates reach for.
pub use article_failure::{ArticleFailure, ArticleFailureKind};
pub use bandwidth::{BandwidthConfig, BandwidthLimiter};
pub use dispatch_engine::{DispatchEngine, DispatchHandle};
pub use download_engine::{
    ConnectionSlot, ConnectionTracker, ProgressUpdate, ServerHealth, SlotStatus, WorkerPool,
    has_known_extension,
};
pub use slice_layout::slice_layout_from_par2;
