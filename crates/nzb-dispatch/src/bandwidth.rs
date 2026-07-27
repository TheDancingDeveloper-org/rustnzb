use std::num::NonZeroU32;
use std::sync::atomic::{AtomicU32, Ordering};

use arc_swap::ArcSwapOption;
use governor::DefaultDirectRateLimiter as RateLimiter;
use governor::Quota;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Default, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct BandwidthConfig {
    /// Download speed limit in bytes per second (None = unlimited)
    pub download_bps: Option<NonZeroU32>,
}

struct Limit {
    limiter: ArcSwapOption<RateLimiter>,
    current_bps: AtomicU32,
}

impl Limit {
    fn new_inner(bps: Option<NonZeroU32>) -> Option<Arc<RateLimiter>> {
        let bps = bps?;
        Some(Arc::new(RateLimiter::direct(Quota::per_second(bps))))
    }

    fn new(bps: Option<NonZeroU32>) -> Self {
        Self {
            limiter: ArcSwapOption::new(Self::new_inner(bps)),
            current_bps: AtomicU32::new(bps.map(|v| v.get()).unwrap_or(0)),
        }
    }

    async fn acquire(&self, size: NonZeroU32) -> anyhow::Result<()> {
        let lim = self.limiter.load().clone();
        if let Some(rl) = lim.as_ref() {
            rl.until_n_ready(size).await?;
        }
        Ok(())
    }

    fn set(&self, limit: Option<NonZeroU32>) {
        let new = Self::new_inner(limit);
        self.limiter.swap(new);
        self.current_bps
            .store(limit.map(|v| v.get()).unwrap_or(0), Ordering::Relaxed);
    }

    fn get(&self) -> Option<NonZeroU32> {
        NonZeroU32::new(self.current_bps.load(Ordering::Relaxed))
    }
}

pub struct BandwidthLimiter {
    download: Limit,
}

impl BandwidthLimiter {
    pub fn new(config: BandwidthConfig) -> Self {
        Self {
            download: Limit::new(config.download_bps),
        }
    }

    pub async fn acquire_download(&self, len: NonZeroU32) -> anyhow::Result<()> {
        self.download.acquire(len).await
    }

    pub fn set_download_bps(&self, bps: Option<NonZeroU32>) {
        self.download.set(bps);
    }

    pub fn get_download_bps(&self) -> Option<NonZeroU32> {
        self.download.get()
    }

    pub fn get_config(&self) -> BandwidthConfig {
        BandwidthConfig {
            download_bps: self.get_download_bps(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn limiter_can_be_reconfigured_without_recreation() {
        let limiter = BandwidthLimiter::new(BandwidthConfig::default());
        assert_eq!(limiter.get_download_bps(), None);
        limiter
            .acquire_download(NonZeroU32::new(1).unwrap())
            .await
            .unwrap();

        limiter.set_download_bps(NonZeroU32::new(1_000));
        assert_eq!(limiter.get_config().download_bps.unwrap().get(), 1_000);
        limiter.set_download_bps(None);
        assert_eq!(limiter.get_download_bps(), None);
    }
}
