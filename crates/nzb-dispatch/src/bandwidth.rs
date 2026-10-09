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

/// A rate limiter together with its burst size, swapped as one unit so a
/// concurrent reconfiguration can never pair a limiter with the wrong burst.
struct Bucket {
    limiter: RateLimiter,
    burst: NonZeroU32,
}

struct Limit {
    limiter: ArcSwapOption<Bucket>,
    current_bps: AtomicU32,
}

impl Limit {
    fn new_inner(bps: Option<NonZeroU32>) -> Option<Arc<Bucket>> {
        let bps = bps?;
        Some(Arc::new(Bucket {
            limiter: RateLimiter::direct(Quota::per_second(bps)),
            burst: bps,
        }))
    }

    fn new(bps: Option<NonZeroU32>) -> Self {
        Self {
            limiter: ArcSwapOption::new(Self::new_inner(bps)),
            current_bps: AtomicU32::new(bps.map(|v| v.get()).unwrap_or(0)),
        }
    }

    async fn acquire(&self, size: NonZeroU32) -> anyhow::Result<()> {
        let lim = self.limiter.load().clone();
        if let Some(bucket) = lim.as_ref() {
            // `Quota::per_second(bps)` gives a burst of `bps` cells, and
            // governor rejects any single request larger than the burst with
            // `InsufficientCapacity`. A decoded article (~750 KB) exceeds the
            // burst for every limit below that, so acquire in burst-sized
            // chunks rather than in one call.
            let burst = bucket.burst.get();
            let mut remaining = size.get();
            while remaining > 0 {
                let chunk = remaining.min(burst);
                // `chunk` is non-zero: `remaining > 0` and `burst >= 1`.
                bucket
                    .limiter
                    .until_n_ready(NonZeroU32::new(chunk).expect("chunk > 0"))
                    .await?;
                remaining -= chunk;
            }
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

    /// Regression: with `Quota::per_second(bps)` the burst equals `bps`, so a
    /// single request larger than the per-second limit (one decoded article
    /// is ~750 KB) used to fail with `InsufficientCapacity` and the callers
    /// discarded the error — the limit was silently ignored.
    #[tokio::test]
    async fn request_larger_than_burst_is_throttled_not_rejected() {
        let limiter = BandwidthLimiter::new(BandwidthConfig {
            download_bps: NonZeroU32::new(200_000),
        });
        let start = std::time::Instant::now();
        limiter
            .acquire_download(NonZeroU32::new(500_000).unwrap())
            .await
            .expect("oversized request must be throttled, not rejected");
        // 200 KB of burst is free; the remaining 300 KB must wait ~1.5 s.
        assert!(
            start.elapsed() >= std::time::Duration::from_millis(1_000),
            "500 KB at 200 KB/s returned after {:?}",
            start.elapsed()
        );
    }

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
