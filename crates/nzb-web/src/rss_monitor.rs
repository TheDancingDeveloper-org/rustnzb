use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use arc_swap::ArcSwap;
use chrono::Utc;
use tracing::{info, warn};

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
        }
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

    /// Run the monitor loop forever, polling feeds at their configured intervals.
    /// Reads feed config from the shared ArcSwap on each iteration so that
    /// feeds added/removed/toggled via the API take effect without a restart.
    pub async fn run(self) {
        info!("RSS monitor started");

        // Migrate legacy seen file on first run
        self.migrate_seen_json();

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .expect("Failed to create HTTP client");

        loop {
            let cfg = self.config.load();
            let feeds = &cfg.rss_feeds;

            for feed in feeds {
                if !feed.enabled {
                    continue;
                }

                if let Err(e) = self.check_feed(&client, feed).await {
                    warn!(feed = %feed.name, error = %e, "RSS feed check failed");
                }
            }

            // Prune old items based on config
            let limit = cfg.general.rss_history_limit.unwrap_or(500);
            if let Ok(count) = self.queue_manager.rss_item_count()
                && count > limit
                && let Ok(pruned) = self.queue_manager.rss_items_prune(limit)
                && pruned > 0
            {
                info!(pruned, "Pruned old RSS items");
            }

            // Use the minimum poll interval across all enabled feeds, defaulting to 15 min
            let interval = feeds
                .iter()
                .filter(|f| f.enabled)
                .map(|f| f.poll_interval_secs)
                .min()
                .unwrap_or(900);

            drop(cfg);
            tokio::time::sleep(tokio::time::Duration::from_secs(interval)).await;
        }
    }

    async fn check_feed(
        &self,
        client: &reqwest::Client,
        feed: &RssFeedConfig,
    ) -> anyhow::Result<()> {
        info!(feed = %feed.name, url = %feed.url, "Checking RSS feed");

        let response = client.get(&feed.url).send().await?;
        let body = response.bytes().await?;
        let parsed = feed_rs::parser::parse(&body[..])?;

        // Compile filter regex if provided
        let filter = feed
            .filter_regex
            .as_ref()
            .and_then(|r| regex::Regex::new(r).ok());

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
        let new_items = self
            .queue_manager
            .rss_items_batch_upsert(&items_for_insert)
            .unwrap_or(0);

        // Now process auto-downloads for newly inserted items only
        // (batch_upsert uses INSERT OR IGNORE so only new items get inserted)
        for p in &pending {
            let Some(ref url) = p.nzb_url else { continue };

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
                regex::Regex::new(&r.match_regex)
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

            // Skip if already downloaded (existing item in DB)
            if self
                .queue_manager
                .rss_item_exists(&p.item.id)
                .unwrap_or(false)
            {
                // Item existed before this batch — already processed previously
                // Check if it was newly inserted by seeing if it's in our new count
                // Actually, we can just check the downloaded flag
                if let Ok(Some(existing)) = self.queue_manager.rss_item_get(&p.item.id)
                    && existing.downloaded
                {
                    continue;
                }
            }

            info!(feed = %feed.name, title = %p.title, url = %url, "Auto-downloading RSS item");

            match self
                .fetch_and_enqueue(client, url, &p.title, feed, category.as_deref(), priority)
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
        client: &reqwest::Client,
        url: &str,
        name: &str,
        feed: &RssFeedConfig,
        category: Option<&str>,
        priority: i32,
    ) -> anyhow::Result<()> {
        let response = client.get(url).send().await?;
        if !response.status().is_success() {
            anyhow::bail!("HTTP {}", response.status());
        }
        let data = response.bytes().await?;

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

        self.queue_manager.add_job(job, Some(data.to_vec()))?;
        Ok(())
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
}
