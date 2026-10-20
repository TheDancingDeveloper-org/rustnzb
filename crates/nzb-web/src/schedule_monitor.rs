//! Background monitor that applies the time-of-day download schedule.
//!
//! The evaluator in [`nzb_core::schedule`] is pure; this task is the only
//! part that knows about the clock. It re-evaluates at every minute boundary
//! (so windows start and end promptly) and immediately whenever the config
//! changes, so enabling a rule or editing its window takes effect at once.

use std::sync::Arc;

use arc_swap::ArcSwap;
use chrono::{Local, Timelike};
use tokio::sync::Notify;
use tokio::time::Duration;
use tracing::debug;

use crate::nzb_core::config::AppConfig;
use crate::nzb_core::schedule::{ScheduleDecision, evaluate_config};
use crate::queue_manager::QueueManager;

pub struct ScheduleMonitor {
    config: Arc<ArcSwap<AppConfig>>,
    queue_manager: Arc<QueueManager>,
    /// Woken on every committed config change.
    wake: Arc<Notify>,
    /// The last decision applied. Re-applying an identical decision is a
    /// no-op, so the log stays quiet while a window is open.
    last: ScheduleDecision,
}

impl ScheduleMonitor {
    pub fn new(config: Arc<ArcSwap<AppConfig>>, queue_manager: Arc<QueueManager>) -> Self {
        Self {
            config,
            queue_manager,
            wake: Arc::new(Notify::new()),
            last: ScheduleDecision::default(),
        }
    }

    pub fn with_wake(mut self, wake: Arc<Notify>) -> Self {
        self.wake = wake;
        self
    }

    /// Evaluate once against the current config and wall clock, applying the
    /// decision. Returns the decision, for tests and startup.
    pub fn apply_now(&mut self) -> ScheduleDecision {
        let config = self.config.load();
        let decision = evaluate_config(&config.schedule, Local::now().naive_local());
        if decision != self.last {
            debug!(
                active_rules = ?decision.active_rule_ids,
                pause = decision.pause,
                speed_limit_bps = ?decision.speed_limit_bps,
                disabled_servers = ?decision.disabled_servers,
                "Schedule decision changed"
            );
        }
        self.queue_manager
            .apply_schedule(&decision, config.schedule.enabled, false);
        self.last = decision.clone();
        decision
    }

    pub async fn run(mut self) {
        self.apply_now();
        loop {
            // Sleep to the next minute boundary so window edges are not
            // delayed by a long fixed interval, and wake early on a config
            // change. A minute is the finest resolution the schedule has.
            let now = Local::now();
            let into_minute = Duration::from_secs(now.second() as u64)
                + Duration::from_nanos(now.nanosecond() as u64);
            let until_next = Duration::from_secs(60)
                .saturating_sub(into_minute)
                .max(Duration::from_millis(50));
            tokio::select! {
                _ = self.wake.notified() => {}
                _ = tokio::time::sleep(until_next) => {}
            }
            self.apply_now();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::log_buffer::LogBuffer;
    use crate::nzb_core::config::ServerConfig;
    use crate::nzb_core::db::Database;
    use crate::nzb_core::schedule::{ScheduleAction, ScheduleRule};
    use crate::queue_manager::PauseReason;

    fn manager() -> (Arc<QueueManager>, tempfile::TempDir) {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let manager = QueueManager::new(
            Vec::<ServerConfig>::new(),
            Database::open_memory().expect("database"),
            tempdir.path().join("incomplete"),
            tempdir.path().join("complete"),
            LogBuffer::default(),
            1,
            Vec::new(),
            0,
            0,
            false,
            5,
            false,
            false,
            100.0,
            30,
        );
        (manager, tempdir)
    }

    fn all_day_pause() -> ScheduleRule {
        ScheduleRule {
            id: "always".into(),
            name: "Always".into(),
            days: vec![
                "mon".into(),
                "tue".into(),
                "wed".into(),
                "thu".into(),
                "fri".into(),
                "sat".into(),
                "sun".into(),
            ],
            start: "00:00".into(),
            end: "24:00".into(),
            action: ScheduleAction::Pause,
            ..ScheduleRule::default()
        }
    }

    #[tokio::test]
    async fn apply_now_pauses_and_resumes_as_the_config_changes() {
        let (manager, _tempdir) = manager();
        let mut config = AppConfig::default();
        config.schedule.enabled = true;
        config.schedule.rules = vec![all_day_pause()];
        let config = Arc::new(ArcSwap::from_pointee(config));
        let mut monitor = ScheduleMonitor::new(Arc::clone(&config), Arc::clone(&manager));

        let decision = monitor.apply_now();
        assert_eq!(decision.active_rule_ids, vec!["always"]);
        assert!(manager.is_paused());
        assert!(manager.schedule_pause_active());
        assert_eq!(manager.global_pause_reason(), Some(PauseReason::Schedule));

        // Turning the schedule off lifts the pause it applied.
        let mut next = AppConfig::default();
        next.schedule.enabled = false;
        next.schedule.rules = vec![all_day_pause()];
        config.store(Arc::new(next));
        assert!(monitor.apply_now().is_idle());
        assert!(!manager.is_paused());
        assert!(!manager.schedule_pause_active());
    }
}
