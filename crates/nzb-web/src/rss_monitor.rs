use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use arc_swap::ArcSwap;
use chrono::Utc;
use tokio::sync::Notify;
use tokio::time::{Duration, Instant};
use tracing::{info, warn};

use crate::fetch_guard::{
    FetchPolicy, MAX_FETCH_BODY_BYTES, build_fetch_client, read_response_bytes_limited,
    validate_fetch_url_with,
};
use crate::nzb_core::config::{AppConfig, RssFeedConfig};
use crate::nzb_core::models::{Priority, RssItem};

use crate::queue_manager::QueueManager;

/// Background RSS feed monitor that polls configured feeds for NZB links,
/// persists all discovered items to the database, and automatically enqueues
/// items that match download rules.
pub struct RssMonitor {
    config: Arc<ArcSwap<AppConfig>>,
    queue_manager: Arc<QueueManager>,
    data_dir: PathBuf,
    /// Woken whenever the config changes, so feed additions, edits and
    /// deletions are picked up without waiting for the current sleep.
    wake: Arc<Notify>,
}

impl RssMonitor {
    pub fn new(
        config: Arc<ArcSwap<AppConfig>>,
        queue_manager: Arc<QueueManager>,
        data_dir: PathBuf,
    ) -> Self {
        Self {
            config,
            queue_manager,
            data_dir,
            wake: Arc::new(Notify::new()),
        }
    }

    /// Use `wake` as the config-change signal (see
    /// [`crate::state::AppState::rss_monitor_wake`]).
    pub fn with_wake(mut self, wake: Arc<Notify>) -> Self {
        self.wake = wake;
        self
    }

    /// Migrate legacy rss_seen.json entries into the database on first run.
    fn migrate_seen_json(&self) {
        let seen_file = self.data_dir.join("rss_seen.json");
        if !seen_file.exists() {
            return;
        }

        let seen: HashSet<String> = std::fs::read_to_string(&seen_file)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();

        if seen.is_empty() {
            let _ = std::fs::remove_file(&seen_file);
            return;
        }

        info!(
            count = seen.len(),
            "Migrating legacy rss_seen.json to database"
        );

        for id in &seen {
            let item = RssItem {
                id: id.clone(),
                feed_name: "migrated".into(),
                title: id.clone(),
                url: None,
                published_at: None,
                first_seen_at: Utc::now(),
                downloaded: true,
                downloaded_at: Some(Utc::now()),
                category: None,
                size_bytes: 0,
            };
            let _ = self.queue_manager.rss_item_upsert(&item);
        }

        // Remove the legacy file after migration
        let _ = std::fs::remove_file(&seen_file);
        info!("Legacy rss_seen.json migrated and removed");
    }

    /// Run the monitor loop forever, polling each feed at its own configured
    /// interval. Feed config is re-read from the shared ArcSwap on every pass,
    /// and the loop sleeps only until the next feed is due or the config
    /// changes (see [`RssMonitor::with_wake`]), so feeds added, edited,
    /// re-enabled or removed via the API take effect at once.
    pub async fn run(self) {
        info!("RSS monitor started");

        // Migrate legacy seen file on first run
        self.migrate_seen_json();

        let mut schedule = FeedSchedule::default();
        let mut next_maintenance = Instant::now();

        loop {
            // A full Arc rather than an arc-swap guard: it is held across the
            // feed checks below.
            let cfg = self.config.load_full();
            schedule.sync(&cfg.rss_feeds);

            let now = Instant::now();
            let due: Vec<&RssFeedConfig> = cfg
                .rss_feeds
                .iter()
                .filter(|feed| feed.enabled && schedule.is_due(feed, now))
                .collect();

            for feed in &due {
                if let Err(e) = self.check_feed(feed).await {
                    warn!(feed = %feed.name, error = %e, "RSS feed check failed");
                }
                schedule.mark_checked(feed, Instant::now());
            }

            if !due.is_empty() || Instant::now() >= next_maintenance {
                self.prune_items(&cfg);
                next_maintenance = Instant::now() + IDLE_INTERVAL;
            }

            let wake_at = schedule
                .next_due(&cfg.rss_feeds)
                .map_or(next_maintenance, |due| due.min(next_maintenance));
            drop(cfg);

            tokio::select! {
                _ = tokio::time::sleep_until(wake_at) => {}
                _ = self.wake.notified() => {}
            }
        }
    }

    /// Trim the stored RSS item history to the configured limits.
    fn prune_items(&self, cfg: &AppConfig) {
        let limit = cfg.general.rss_history_limit.unwrap_or(500);
        if let Ok(count) = self.queue_manager.rss_item_count()
            && count > limit
            && let Ok(pruned) = self.queue_manager.rss_items_prune(limit)
            && pruned > 0
        {
            info!(pruned, "Pruned old RSS items");
        }

        if let Some(days) = cfg.general.rss_downloaded_item_expiry_days {
            let cutoff = (Utc::now() - chrono::Duration::days(days as i64)).to_rfc3339();
            if let Ok(expired) = self.queue_manager.rss_items_expire_downloaded(&cutoff)
                && expired > 0
            {
                info!(expired, "Expired downloaded RSS items");
            }
        }
    }

    fn fetch_policy(&self) -> FetchPolicy {
        FetchPolicy::from_config(&self.config.load().general)
    }

    async fn check_feed(&self, feed: &RssFeedConfig) -> anyhow::Result<()> {
        info!(feed = %feed.name, url = %feed.url, "Checking RSS feed");

        let feed_plan = validate_fetch_url_with(&feed.url, &self.fetch_policy())
            .await
            .map_err(|error| anyhow::anyhow!(error.to_string()))?;
        let feed_client =
            build_fetch_client(&feed_plan).map_err(|error| anyhow::anyhow!(error.to_string()))?;
        let response = feed_client.get(feed_plan.url).send().await?;
        if !response.status().is_success() {
            anyhow::bail!("HTTP {}", response.status());
        }
        let body = read_response_bytes_limited(response, MAX_FETCH_BODY_BYTES)
            .await
            .map_err(|error| anyhow::anyhow!(error.to_string()))?;
        let parsed = feed_rs::parser::parse(&body[..])?;

        // Compile filter regex if provided
        let filter = match feed.filter_regex.as_deref() {
            None => None,
            Some(pattern) => Some(
                Self::compile_filter(pattern)
                    .ok_or_else(|| anyhow::anyhow!("invalid RSS filter expression"))?,
            ),
        };

        // Load download rules for this feed
        let rules = self
            .queue_manager
            .rss_rule_list()
            .unwrap_or_default()
            .into_iter()
            .filter(|r| r.enabled && r.feed_names.iter().any(|n| n == &feed.name))
            .collect::<Vec<_>>();

        // Collect all items for batch insert (single DB lock)
        struct PendingItem {
            item: RssItem,
            title: String,
            nzb_url: Option<String>,
        }
        let now = Utc::now();
        let mut pending: Vec<PendingItem> = Vec::new();

        for entry in &parsed.entries {
            let title = entry
                .title
                .as_ref()
                .map(|t| t.content.clone())
                .unwrap_or_default();
            let nzb_url = Self::extract_nzb_url(entry);
            let size_bytes = entry
                .media
                .iter()
                .flat_map(|m| &m.content)
                .filter_map(|c| c.size)
                .next()
                .unwrap_or(0);
            let published_at = entry.published.or(entry.updated);

            if let Some(max_age_days) = feed.max_age_days
                && let Some(published_at) = published_at
                && now.signed_duration_since(published_at).num_seconds()
                    > (max_age_days as i64).saturating_mul(86_400)
            {
                continue;
            }

            pending.push(PendingItem {
                item: RssItem {
                    id: entry.id.clone(),
                    feed_name: feed.name.clone(),
                    title: title.clone(),
                    url: nzb_url.clone(),
                    published_at,
                    first_seen_at: now,
                    downloaded: false,
                    downloaded_at: None,
                    category: feed.category.clone(),
                    size_bytes,
                },
                title,
                nzb_url,
            });
        }

        // Batch insert all items in one transaction (single DB lock)
        let items_for_insert: Vec<RssItem> = pending.iter().map(|p| p.item.clone()).collect();
        let downloaded_ids: HashSet<String> = pending
            .iter()
            .filter(|pending| {
                self.queue_manager
                    .rss_item_get(&pending.item.id)
                    .ok()
                    .flatten()
                    .is_some_and(|item| item.downloaded)
            })
            .map(|pending| pending.item.id.clone())
            .collect();
        let new_items = self
            .queue_manager
            .rss_items_batch_upsert(&items_for_insert)
            .unwrap_or(0);

        // Now process auto-downloads for newly inserted items only
        // (batch_upsert uses INSERT OR IGNORE so only new items get inserted)
        let mut handled_ids = HashSet::new();
        for p in &pending {
            let Some(ref url) = p.nzb_url else { continue };

            if downloaded_ids.contains(&p.item.id) || !handled_ids.insert(p.item.id.clone()) {
                continue;
            }

            // Feed-level filter must pass (if set)
            let passes_filter = match filter {
                Some(ref re) => re.is_match(&p.title),
                None => true,
            };
            if !passes_filter {
                continue;
            }

            // Check download rules
            let matched_rule = rules.iter().find(|r| {
                Self::compile_filter(&r.match_regex)
                    .map(|re| re.is_match(&p.title))
                    .unwrap_or(false)
            });

            // Auto-download logic:
            // 1. If a download rule matches → download with rule's category/priority
            // 2. If feed has auto_download enabled (and no filter_regex) → download all
            // 3. Otherwise → don't auto-download
            let (should_download, category, priority) = if let Some(rule) = matched_rule {
                (
                    true,
                    rule.category.clone().or_else(|| feed.category.clone()),
                    rule.priority,
                )
            } else if feed.auto_download && feed.filter_regex.is_none() {
                (true, feed.category.clone(), 1)
            } else {
                (false, None, 1)
            };

            if !should_download {
                continue;
            }

            info!(feed = %feed.name, title = %p.title, url = %url, "Auto-downloading RSS item");

            match self
                .fetch_and_enqueue(url, &p.title, feed, category.as_deref(), priority)
                .await
            {
                Ok(()) => {
                    let _ = self
                        .queue_manager
                        .rss_item_mark_downloaded(&p.item.id, category.as_deref());
                    info!(title = %p.title, "RSS item enqueued successfully");
                }
                Err(e) => {
                    warn!(title = %p.title, error = %e, "Failed to enqueue RSS item");
                }
            }
        }

        if new_items > 0 {
            info!(feed = %feed.name, new_items, "RSS feed check complete");
        }

        Ok(())
    }

    fn compile_filter(pattern: &str) -> Option<regex::Regex> {
        const MAX_PATTERN_BYTES: usize = 512;
        if pattern.len() > MAX_PATTERN_BYTES {
            return None;
        }
        regex::RegexBuilder::new(pattern)
            .size_limit(1024 * 1024)
            .build()
            .ok()
    }

    /// Extract NZB URL from a feed entry's links or media content.
    fn extract_nzb_url(entry: &feed_rs::model::Entry) -> Option<String> {
        entry
            .links
            .iter()
            .find(|l| {
                l.href.ends_with(".nzb")
                    || l.media_type
                        .as_deref()
                        .is_some_and(|mt| mt == "application/x-nzb")
            })
            .map(|l| l.href.clone())
            .or_else(|| {
                // Check media content for NZB URLs
                entry
                    .media
                    .iter()
                    .flat_map(|m| &m.content)
                    .find(|c| c.url.as_ref().is_some_and(|u| u.as_str().ends_with(".nzb")))
                    .and_then(|c| c.url.as_ref().map(|u| u.to_string()))
            })
            .or_else(|| {
                // Fall back to first link
                entry.links.first().map(|l| l.href.clone())
            })
    }

    async fn fetch_and_enqueue(
        &self,
        url: &str,
        name: &str,
        feed: &RssFeedConfig,
        category: Option<&str>,
        priority: i32,
    ) -> anyhow::Result<()> {
        let plan = validate_fetch_url_with(url, &self.fetch_policy())
            .await
            .map_err(|error| anyhow::anyhow!(error.to_string()))?;
        let client =
            build_fetch_client(&plan).map_err(|error| anyhow::anyhow!(error.to_string()))?;
        let response = client.get(plan.url).send().await?;
        if !response.status().is_success() {
            anyhow::bail!("HTTP {}", response.status());
        }
        let data = read_response_bytes_limited(response, MAX_FETCH_BODY_BYTES)
            .await
            .map_err(|error| anyhow::anyhow!(error.to_string()))?;

        let mut job = crate::nzb_core::nzb_parser::parse_nzb(name, &data)?;

        if let Some(cat) = category {
            job.category = cat.to_string();
        } else if let Some(ref cat) = feed.category {
            job.category = cat.clone();
        }

        job.priority = match priority {
            0 => Priority::Low,
            2 => Priority::High,
            3 => Priority::Force,
            _ => Priority::Normal,
        };

        job.work_dir = self.queue_manager.incomplete_dir().join(&job.id);
        job.output_dir = if !job.category.is_empty() && job.category != "Default" {
            self.queue_manager
                .complete_dir()
                .join(&job.category)
                .join(&job.name)
        } else {
            self.queue_manager.complete_dir().join(&job.name)
        };

        std::fs::create_dir_all(&job.work_dir)?;

        self.queue_manager.add_job(job, Some(data))?;
        Ok(())
    }
}

/// How long the monitor sleeps when no feed is due sooner; also the cadence
/// of RSS item history maintenance.
const IDLE_INTERVAL: Duration = Duration::from_secs(900);

/// Smallest accepted `poll_interval_secs`. The API rejects anything lower,
/// and the monitor clamps lower values from existing or hand-edited configs
/// so a `0` interval cannot spin the loop against the indexer.
pub const MIN_POLL_INTERVAL_SECS: u64 = 60;

/// A feed's poll interval with [`MIN_POLL_INTERVAL_SECS`] applied.
fn effective_poll_interval(feed: &RssFeedConfig) -> Duration {
    Duration::from_secs(feed.poll_interval_secs.max(MIN_POLL_INTERVAL_SECS))
}

/// When each enabled feed was last checked, keyed by feed name.
///
/// A feed's next due time is derived from its *current* `poll_interval_secs`
/// on every pass, so an interval edit takes effect immediately. Feeds that are
/// unknown (newly added, renamed, re-enabled or re-pointed at a new URL) are
/// due at once; deleted or disabled feeds are forgotten.
#[derive(Debug, Default)]
struct FeedSchedule {
    last_checked: HashMap<String, (String, Instant)>,
    /// Feeds already warned about for an interval below the floor.
    clamped: HashSet<String>,
}

impl FeedSchedule {
    /// Drop entries for feeds that are gone, disabled, or now point elsewhere,
    /// and warn (once per feed) about intervals below the floor. Returns the
    /// names newly warned about.
    fn sync(&mut self, feeds: &[RssFeedConfig]) -> Vec<String> {
        self.last_checked.retain(|name, (url, _)| {
            feeds
                .iter()
                .any(|f| f.enabled && &f.name == name && &f.url == url)
        });

        let too_fast: HashSet<&str> = feeds
            .iter()
            .filter(|f| f.enabled && f.poll_interval_secs < MIN_POLL_INTERVAL_SECS)
            .map(|f| f.name.as_str())
            .collect();
        self.clamped.retain(|name| too_fast.contains(name.as_str()));
        let mut warned = Vec::new();
        for feed in feeds {
            if too_fast.contains(feed.name.as_str()) && self.clamped.insert(feed.name.clone()) {
                warn!(
                    feed = %feed.name,
                    poll_interval_secs = feed.poll_interval_secs,
                    min_secs = MIN_POLL_INTERVAL_SECS,
                    "RSS feed poll interval is below the minimum; using the minimum"
                );
                warned.push(feed.name.clone());
            }
        }
        warned
    }

    fn due_at(&self, feed: &RssFeedConfig) -> Option<Instant> {
        self.last_checked.get(&feed.name).map(|(_, at)| {
            // An absurd interval must not overflow `Instant`; a year out
            // is effectively "never" for a poller.
            at.checked_add(effective_poll_interval(feed))
                .unwrap_or(*at + Duration::from_secs(365 * 86_400))
        })
    }

    fn is_due(&self, feed: &RssFeedConfig, now: Instant) -> bool {
        self.due_at(feed).is_none_or(|due| due <= now)
    }

    fn mark_checked(&mut self, feed: &RssFeedConfig, at: Instant) {
        self.last_checked
            .insert(feed.name.clone(), (feed.url.clone(), at));
    }

    /// Earliest due time across enabled feeds; `None` when there are none.
    /// A never-checked feed counts as due now.
    fn next_due(&self, feeds: &[RssFeedConfig]) -> Option<Instant> {
        let now = Instant::now();
        feeds
            .iter()
            .filter(|f| f.enabled)
            .map(|f| self.due_at(f).unwrap_or(now))
            .min()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::log_buffer::LogBuffer;
    use crate::nzb_core::db::Database;

    fn monitor(data_dir: PathBuf) -> (RssMonitor, Arc<QueueManager>) {
        let config = Arc::new(ArcSwap::from_pointee(AppConfig::default()));
        let queue_manager = QueueManager::new(
            Vec::new(),
            Database::open_memory().expect("in-memory database"),
            data_dir.join("incomplete"),
            data_dir.join("complete"),
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
        let monitor = RssMonitor::new(config, queue_manager.clone(), data_dir);
        (monitor, queue_manager)
    }

    #[test]
    fn extracts_nzb_links_before_falling_back_to_the_first_link() {
        let feed = feed_rs::parser::parse(
            &br#"<?xml version="1.0"?><feed xmlns="http://www.w3.org/2005/Atom">
                <id>feed</id><title>Feed</title><updated>2026-07-27T00:00:00Z</updated>
                <entry><id>nzb</id><title>NZB</title><updated>2026-07-27T00:00:00Z</updated>
                    <link href="https://example.test/page"/><link href="https://example.test/release.nzb"/>
                </entry>
                <entry><id>fallback</id><title>Fallback</title><updated>2026-07-27T00:00:00Z</updated>
                    <link href="https://example.test/page"/>
                </entry>
            </feed>"#[..],
        )
        .expect("valid atom feed");

        assert_eq!(
            RssMonitor::extract_nzb_url(&feed.entries[0]).as_deref(),
            Some("https://example.test/release.nzb")
        );
        assert_eq!(
            RssMonitor::extract_nzb_url(&feed.entries[1]).as_deref(),
            Some("https://example.test/page")
        );
    }

    #[tokio::test]
    async fn migrates_legacy_seen_file_once_and_marks_items_downloaded() {
        let temp = tempfile::tempdir().expect("tempdir");
        let seen_file = temp.path().join("rss_seen.json");
        std::fs::write(&seen_file, r#"["first-item", "second-item"]"#).expect("seen file");
        let (monitor, queue_manager) = monitor(temp.path().to_path_buf());

        monitor.migrate_seen_json();

        assert!(!seen_file.exists());
        for id in ["first-item", "second-item"] {
            let item = queue_manager
                .rss_item_get(id)
                .expect("database query")
                .expect("migrated item");
            assert_eq!(item.feed_name, "migrated");
            assert!(item.downloaded);
            assert!(item.downloaded_at.is_some());
        }
    }

    #[test]
    fn feed_filters_fail_closed_at_syntax_and_size_limits() {
        assert!(RssMonitor::compile_filter(r"release-[0-9]+").is_some());
        assert!(RssMonitor::compile_filter("(").is_none());
        assert!(RssMonitor::compile_filter(&"x".repeat(513)).is_none());
    }

    #[tokio::test]
    async fn feed_checks_reject_local_targets_before_network_access() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (monitor, _) = monitor(temp.path().to_path_buf());
        let feed = RssFeedConfig {
            name: "local-feed".into(),
            url: "http://127.0.0.1:9/feed.xml".into(),
            poll_interval_secs: 900,
            category: None,
            filter_regex: None,
            enabled: true,
            auto_download: true,
            max_age_days: None,
        };

        let error = monitor
            .check_feed(&feed)
            .await
            .expect_err("local addresses must be rejected");
        assert!(error.to_string().contains("private/reserved"));
    }

    const EMPTY_RSS: &str =
        r#"<?xml version="1.0"?><rss version="2.0"><channel><title>t</title></channel></rss>"#;

    /// Serve `EMPTY_RSS` on loopback and report the (tokio, so possibly
    /// paused) time of every request that arrives.
    async fn feed_server() -> (
        std::net::SocketAddr,
        tokio::sync::mpsc::UnboundedReceiver<tokio::time::Instant>,
    ) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind feed server");
        let addr = listener.local_addr().expect("feed server addr");
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let mut request = Vec::new();
                let mut buf = [0u8; 1024];
                while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                    match socket.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => request.extend_from_slice(&buf[..n]),
                    }
                }
                let _ = tx.send(tokio::time::Instant::now());
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/rss+xml\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    EMPTY_RSS.len(),
                    EMPTY_RSS
                );
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.shutdown().await;
            }
        });
        (addr, rx)
    }

    fn feed(name: &str, addr: std::net::SocketAddr, poll_interval_secs: u64) -> RssFeedConfig {
        RssFeedConfig {
            name: name.into(),
            url: format!("http://{addr}/{name}.xml"),
            poll_interval_secs,
            category: None,
            filter_regex: None,
            enabled: true,
            auto_download: false,
            max_age_days: None,
        }
    }

    /// BUG-66: with no feeds at boot the monitor sleeps for the 900 s idle
    /// interval, and a feed added through the API used to wait out that whole
    /// sleep before its first check. Saving the config must wake the monitor.
    /// Runs on a paused clock, so the 900 s sleep costs no wall-clock time and
    /// the assertion is on virtual time.
    #[tokio::test(start_paused = true)]
    async fn feed_config_changes_take_effect_without_waiting_for_the_current_sleep() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (addr, mut requests) = feed_server().await;

        let mut config = AppConfig::default();
        config.general.fetch_allowed_hosts = vec!["127.0.0.1".into()];
        let (monitor, queue_manager) = monitor(temp.path().to_path_buf());
        let shared = Arc::clone(&monitor.config);
        shared.store(Arc::new(config));
        let state = crate::state::AppState::new(
            Arc::clone(&shared),
            temp.path().join("config.toml"),
            queue_manager,
            LogBuffer::default(),
            Arc::new(crate::auth::TokenStore::new()),
            Arc::new(crate::auth::CredentialStore::new(temp.path().to_path_buf())),
        );
        let monitor = monitor.with_wake(Arc::clone(&state.rss_monitor_wake));
        let task = tokio::spawn(monitor.run());

        // Let the monitor run its empty first pass and go to sleep.
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;

        let added_at = tokio::time::Instant::now();
        state
            .update_config_with(|config| {
                config.rss_feeds.push(feed("late", addr, 600));
                Ok::<_, anyhow::Error>(())
            })
            .expect("add feed");

        let fetched_at = requests.recv().await.expect("feed was fetched");
        let waited = fetched_at - added_at;
        assert!(
            waited < std::time::Duration::from_secs(5),
            "new feed waited {waited:?} for its first check"
        );

        // Shortening the interval takes effect without waiting out the old 600 s.
        state
            .update_config_with(|config| {
                config.rss_feeds[0].poll_interval_secs = 60;
                Ok::<_, anyhow::Error>(())
            })
            .expect("edit feed");
        let refetched_at = requests.recv().await.expect("feed was re-fetched");
        let gap = refetched_at - fetched_at;
        assert!(
            gap < std::time::Duration::from_secs(120),
            "edited interval took {gap:?} to apply"
        );

        // A deleted feed is never fetched again.
        state
            .update_config_with(|config| {
                config.rss_feeds.clear();
                Ok::<_, anyhow::Error>(())
            })
            .expect("delete feed");
        tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
        while let Ok(at) = requests.try_recv() {
            assert!(
                at <= refetched_at + std::time::Duration::from_secs(1),
                "deleted feed was fetched again after {:?}",
                at - refetched_at
            );
        }
        task.abort();
    }

    fn schedule_feed(name: &str, url: &str, poll_interval_secs: u64) -> RssFeedConfig {
        RssFeedConfig {
            name: name.into(),
            url: url.into(),
            poll_interval_secs,
            category: None,
            filter_regex: None,
            enabled: true,
            auto_download: false,
            max_age_days: None,
        }
    }

    #[test]
    fn schedule_checks_new_feeds_at_once_and_then_per_feed_interval() {
        let t0 = Instant::now();
        let fast = schedule_feed("fast", "https://example.test/fast", 60);
        let slow = schedule_feed("slow", "https://example.test/slow", 3600);
        let feeds = vec![fast.clone(), slow.clone()];
        let mut schedule = FeedSchedule::default();

        schedule.sync(&feeds);
        assert!(schedule.is_due(&fast, t0) && schedule.is_due(&slow, t0));

        schedule.mark_checked(&fast, t0);
        schedule.mark_checked(&slow, t0);
        let at_61 = t0 + Duration::from_secs(61);
        assert!(schedule.is_due(&fast, at_61));
        assert!(
            !schedule.is_due(&slow, at_61),
            "slow feed is not over-polled"
        );
        assert_eq!(
            schedule.next_due(&feeds),
            Some(t0 + Duration::from_secs(60))
        );
    }

    #[test]
    fn schedule_applies_interval_edits_immediately() {
        let t0 = Instant::now();
        let mut feed = schedule_feed("f", "https://example.test/f", 900);
        let mut schedule = FeedSchedule::default();
        schedule.mark_checked(&feed, t0);
        assert_eq!(schedule.due_at(&feed), Some(t0 + Duration::from_secs(900)));

        feed.poll_interval_secs = 120;
        schedule.sync(std::slice::from_ref(&feed));
        assert_eq!(schedule.due_at(&feed), Some(t0 + Duration::from_secs(120)));

        feed.poll_interval_secs = u64::MAX;
        assert!(
            schedule.due_at(&feed).is_some(),
            "huge interval must not overflow"
        );
    }

    #[test]
    fn schedule_forgets_deleted_disabled_and_repointed_feeds() {
        let t0 = Instant::now();
        let kept = schedule_feed("kept", "https://example.test/kept", 900);
        let deleted = schedule_feed("deleted", "https://example.test/deleted", 900);
        let mut toggled = schedule_feed("toggled", "https://example.test/toggled", 900);
        let mut moved = schedule_feed("moved", "https://example.test/old", 900);
        let mut schedule = FeedSchedule::default();
        for feed in [&kept, &deleted, &toggled, &moved] {
            schedule.mark_checked(feed, t0);
        }

        toggled.enabled = false;
        moved.url = "https://example.test/new".into();
        let feeds = vec![kept.clone(), toggled.clone(), moved.clone()];
        schedule.sync(&feeds);

        assert!(!schedule.is_due(&kept, t0));
        assert!(!schedule.last_checked.contains_key("deleted"));
        assert!(!schedule.last_checked.contains_key("toggled"));
        assert!(
            schedule.is_due(&moved, t0),
            "a re-pointed feed is checked at once"
        );
        assert_eq!(
            schedule.next_due(&[toggled.clone()]),
            None,
            "disabled feeds never wake the loop"
        );

        toggled.enabled = true;
        schedule.sync(std::slice::from_ref(&toggled));
        assert!(
            schedule.is_due(&toggled, t0),
            "a re-enabled feed is checked at once"
        );
    }

    #[test]
    fn schedule_clamps_intervals_below_the_floor_and_warns_once_per_feed() {
        let t0 = Instant::now();
        let mut zero = schedule_feed("zero", "https://example.test/zero", 0);
        let tiny = schedule_feed("tiny", "https://example.test/tiny", 5);
        let ok = schedule_feed("ok", "https://example.test/ok", MIN_POLL_INTERVAL_SECS);
        let mut schedule = FeedSchedule::default();

        let feeds = vec![zero.clone(), tiny.clone(), ok.clone()];
        let mut warned = schedule.sync(&feeds);
        warned.sort();
        assert_eq!(warned, vec!["tiny".to_string(), "zero".to_string()]);
        assert!(schedule.sync(&feeds).is_empty(), "warns only once per feed");

        let floor = Duration::from_secs(MIN_POLL_INTERVAL_SECS);
        for feed in [&zero, &tiny, &ok] {
            schedule.mark_checked(feed, t0);
            assert_eq!(schedule.due_at(feed), Some(t0 + floor), "{}", feed.name);
            assert!(!schedule.is_due(feed, t0), "{} must not spin", feed.name);
        }

        // Fixing the interval and then breaking it again warns afresh.
        zero.poll_interval_secs = 900;
        assert!(schedule.sync(std::slice::from_ref(&zero)).is_empty());
        zero.poll_interval_secs = 0;
        assert_eq!(
            schedule.sync(std::slice::from_ref(&zero)),
            vec!["zero".to_string()]
        );
    }
}
