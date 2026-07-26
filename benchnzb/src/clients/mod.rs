pub mod rustnzb;
pub mod sabnzbd;

#[derive(Default)]
pub struct StageTiming {
    pub download_sec: f64,
    pub par2_sec: f64,
    pub unpack_sec: f64,
}

// ---------------------------------------------------------------------------
// Stress test client abstraction (enum dispatch to avoid async_trait dep)
// ---------------------------------------------------------------------------

pub use rustnzb::StatusSummary;

/// Unified client for stress tests — wraps either RustNZB or SABnzbd.
pub enum StressClient {
    Rustnzb(rustnzb::RustnzbClient),
    Sabnzbd(sabnzbd::SabnzbdClient),
}

impl StressClient {
    pub async fn add_nzb(&self, data: &[u8], filename: &str) -> anyhow::Result<()> {
        match self {
            Self::Rustnzb(c) => c.add_nzb(data, filename).await,
            Self::Sabnzbd(c) => c.add_nzb(data, filename).await,
        }
    }

    pub async fn queue_size(&self) -> anyhow::Result<usize> {
        match self {
            Self::Rustnzb(c) => c.queue_size().await,
            Self::Sabnzbd(c) => c.queue_size().await,
        }
    }

    pub async fn get_status(&self) -> anyhow::Result<StatusSummary> {
        match self {
            Self::Rustnzb(c) => c.get_status().await,
            Self::Sabnzbd(c) => c.get_status().await,
        }
    }

    pub async fn history_count(&self) -> anyhow::Result<u64> {
        match self {
            Self::Rustnzb(c) => c.history_count().await,
            Self::Sabnzbd(c) => c.history_count().await,
        }
    }

    pub async fn clear_history(&self) -> anyhow::Result<()> {
        match self {
            Self::Rustnzb(c) => c.clear_history().await,
            Self::Sabnzbd(c) => c.clear_history().await,
        }
    }

    pub async fn clear_all(&self) {
        match self {
            Self::Rustnzb(c) => c.clear_all().await,
            Self::Sabnzbd(c) => c.clear_all().await,
        }
    }

    pub fn clone_client(&self) -> Self {
        match self {
            Self::Rustnzb(c) => Self::Rustnzb(c.clone_client()),
            Self::Sabnzbd(c) => Self::Sabnzbd(c.clone_client()),
        }
    }
}
