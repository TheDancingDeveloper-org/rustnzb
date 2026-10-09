use std::num::NonZeroU32;
use std::sync::atomic::{AtomicU32, Ordering};

use arc_swap::ArcSwapOption;
use governor::DefaultDirectRateLimiter as RateLimiter;
use governor::Quota;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::Notify;

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
    /// Signalled on every reconfiguration so parked acquires abandon the
    /// limiter they loaded and restart against the current one.
    changed: Notify,
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
            changed: Notify::new(),
        }
    }

    async fn acquire(&self, size: NonZeroU32) -> anyhow::Result<()> {
        let mut remaining = size.get();
        while remaining > 0 {
            // Register for change notifications *before* loading the limiter,
            // so a reconfiguration between the load and the wait is not lost.
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();

            let lim = self.limiter.load_full();
            let Some(bucket) = lim else {
                // Unlimited (including a switch to unlimited mid-acquire).
                return Ok(());
            };
            // `Quota::per_second(bps)` gives a burst of `bps` cells, and
            // governor rejects any single request larger than the burst with
            // `InsufficientCapacity`. A decoded article (~750 KB) exceeds the
            // burst for every limit below that, so acquire in burst-sized
            // chunks rather than in one call.
            let chunk = remaining.min(bucket.burst.get());
            // `chunk` is non-zero: `remaining > 0` and `burst >= 1`.
            let n = NonZeroU32::new(chunk).expect("chunk > 0");
            tokio::select! {
                res = bucket.limiter.until_n_ready(n) => {
                    res?;
                    remaining -= chunk;
                }
                // The limit changed while parked: drop the wait on the old
                // limiter and re-evaluate `remaining` against the new one.
                () = &mut changed => {}
            }
        }
        Ok(())
    }

    fn set(&self, limit: Option<NonZeroU32>) {
        let new = Self::new_inner(limit);
        self.limiter.swap(new);
        self.current_bps
            .store(limit.map(|v| v.get()).unwrap_or(0), Ordering::Relaxed);
        self.changed.notify_waiters();
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

    /// Start an acquire that, at `bps`, would take hours, then reconfigure
    /// the limit and return how long the parked acquire took to complete.
    async fn acquire_then_reconfigure(
        initial: u32,
        changed: Option<NonZeroU32>,
    ) -> std::time::Duration {
        let limiter = Arc::new(BandwidthLimiter::new(BandwidthConfig {
            download_bps: NonZeroU32::new(initial),
        }));
        let waiter = {
            let limiter = Arc::clone(&limiter);
            tokio::spawn(async move {
                // One decoded article: ~750 KB, i.e. ~10 h at 20 B/s.
                limiter
                    .acquire_download(NonZeroU32::new(750_000).unwrap())
                    .await
            })
        };
        // Let the acquire park inside the old limiter.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert!(!waiter.is_finished(), "acquire should be throttled");

        let changed_at = std::time::Instant::now();
        limiter.set_download_bps(changed);
        tokio::time::timeout(std::time::Duration::from_secs(5), waiter)
            .await
            .expect("parked acquire never observed the limit change")
            .unwrap()
            .unwrap();
        changed_at.elapsed()
    }

    /// Regression (BUG-57): a parked acquire kept awaiting the limiter it
    /// loaded on entry, so lowering the limit to 20 B/s wedged every worker
    /// for hours and setting it back to unlimited did not release them.
    #[tokio::test]
    async fn switching_to_unlimited_releases_parked_acquire() {
        let took = acquire_then_reconfigure(20, None).await;
        assert!(
            took < std::time::Duration::from_secs(2),
            "unlimited took {took:?} to apply"
        );
    }

    #[tokio::test]
    async fn raising_limit_releases_parked_acquire() {
        let took = acquire_then_reconfigure(20, NonZeroU32::new(10_000_000)).await;
        assert!(
            took < std::time::Duration::from_secs(2),
            "raised limit took {took:?} to apply"
        );
    }

    #[tokio::test]
    async fn unlimited_acquire_returns_immediately() {
        let limiter = BandwidthLimiter::new(BandwidthConfig::default());
        tokio::time::timeout(
            std::time::Duration::from_millis(100),
            limiter.acquire_download(NonZeroU32::new(u32::MAX).unwrap()),
        )
        .await
        .expect("unlimited acquire must not wait")
        .unwrap();
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
