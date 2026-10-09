#![recursion_limit = "256"]

pub use nzb_decode;
pub use nzb_postproc;
pub use nzb_postproc::nzb_core;

pub mod auth;
pub mod dir_watcher;
pub mod direct_unpack;
pub mod error;
pub mod fetch_guard;
pub mod log_buffer;
pub mod nzb_archive;
pub mod queue_manager;
pub mod rss_monitor;
pub mod sabnzbd_compat;
pub mod startup;
pub mod state;
pub mod util;

pub use log_buffer::{LogBuffer, LogBufferLayer};
pub use nzb_dispatch::{ArticleFailure, ArticleFailureKind};
pub use queue_manager::{
    DailyStatisticsData, GlobalStatisticsData, HistoryRetryOutcome, QueueManager, QueueSortField,
    ServerStatsData, StatisticsPeriodData,
};
pub use startup::{StartupConfig, StartupResult};
pub use state::AppState;

pub(crate) fn increment_counter(name: &'static str) {
    opentelemetry::global::meter_provider()
        .meter("rustnzb")
        .u64_counter(name)
        .build()
        .add(1, &[]);
}
