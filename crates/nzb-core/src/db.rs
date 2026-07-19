use std::path::Path;

use chrono::Utc;
use rusqlite::{Connection, params};
use tracing::info;

use crate::error::NzbError;
use crate::models::*;

#[allow(dead_code)]
const SCHEMA_VERSION: u32 = 1;

/// Database handle for queue and history persistence.
pub struct Database {
    pub(crate) conn: Connection,
}

impl Database {
    /// Open (or create) the database at the given path.
    pub fn open(path: &Path) -> Result<Self, NzbError> {
        let conn = Connection::open(path)?;

        // Enable WAL mode for concurrent reads during downloads
        conn.execute_batch("PRAGMA journal_mode=WAL;")?;
        conn.execute_batch("PRAGMA foreign_keys=ON;")?;

        let db = Self { conn };
        db.migrate()?;
        Ok(db)
    }

    /// Open an in-memory database (for testing).
    pub fn open_memory() -> Result<Self, NzbError> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch("PRAGMA foreign_keys=ON;")?;
        let db = Self { conn };
        db.migrate()?;
        Ok(db)
    }

    fn migrate(&self) -> Result<(), NzbError> {
        self.conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS schema_version (
                version INTEGER NOT NULL
            );",
        )?;

        let version: u32 = self
            .conn
            .query_row(
                "SELECT COALESCE(MAX(version), 0) FROM schema_version",
                [],
                |row| row.get(0),
            )
            .unwrap_or(0);

        if version < 1 {
            info!("Applying database migration v1");
            self.conn.execute_batch(
                "
                -- Active download queue
                CREATE TABLE IF NOT EXISTS queue (
                    id TEXT PRIMARY KEY,
                    name TEXT NOT NULL,
                    category TEXT NOT NULL DEFAULT 'Default',
                    status TEXT NOT NULL DEFAULT 'queued',
                    priority INTEGER NOT NULL DEFAULT 1,
                    total_bytes INTEGER NOT NULL DEFAULT 0,
                    downloaded_bytes INTEGER NOT NULL DEFAULT 0,
                    file_count INTEGER NOT NULL DEFAULT 0,
                    files_completed INTEGER NOT NULL DEFAULT 0,
                    article_count INTEGER NOT NULL DEFAULT 0,
                    articles_downloaded INTEGER NOT NULL DEFAULT 0,
                    articles_failed INTEGER NOT NULL DEFAULT 0,
                    added_at TEXT NOT NULL,
                    completed_at TEXT,
                    work_dir TEXT NOT NULL,
                    output_dir TEXT NOT NULL,
                    password TEXT,
                    error_message TEXT,
                    -- Serialized NzbFile/Article data (bincode)
                    job_data BLOB
                );

                CREATE INDEX IF NOT EXISTS idx_queue_status ON queue(status);
                CREATE INDEX IF NOT EXISTS idx_queue_priority ON queue(priority DESC, added_at ASC);

                -- Completed/failed job history
                CREATE TABLE IF NOT EXISTS history (
                    id TEXT PRIMARY KEY,
                    name TEXT NOT NULL,
                    category TEXT NOT NULL DEFAULT 'Default',
                    status TEXT NOT NULL,
                    total_bytes INTEGER NOT NULL DEFAULT 0,
                    downloaded_bytes INTEGER NOT NULL DEFAULT 0,
                    added_at TEXT NOT NULL,
                    completed_at TEXT NOT NULL,
                    output_dir TEXT NOT NULL,
                    stages TEXT, -- JSON array of StageResult
                    error_message TEXT
                );

                CREATE INDEX IF NOT EXISTS idx_history_completed ON history(completed_at DESC);
                CREATE INDEX IF NOT EXISTS idx_history_status ON history(status);

                -- Server configuration (persisted separately from TOML for runtime changes)
                CREATE TABLE IF NOT EXISTS servers (
                    id TEXT PRIMARY KEY,
                    config TEXT NOT NULL -- JSON ServerConfig
                );

                INSERT INTO schema_version (version) VALUES (1);
                ",
            )?;
        }

        if version < 2 {
            info!("Applying database migration v2");
            self.conn.execute_batch(
                "
                -- Add NZB data storage and server stats to history
                ALTER TABLE history ADD COLUMN nzb_data BLOB;
                ALTER TABLE history ADD COLUMN server_stats TEXT DEFAULT '[]';

                -- Add server stats to queue
                ALTER TABLE queue ADD COLUMN server_stats TEXT DEFAULT '[]';

                -- Add NZB data to queue for preservation
                ALTER TABLE queue ADD COLUMN nzb_raw BLOB;

                UPDATE schema_version SET version = 2;
                ",
            )?;
        }

        if version < 3 {
            info!("Applying database migration v3");
            self.conn.execute_batch(
                "
                -- Per-job log storage for history
                ALTER TABLE history ADD COLUMN job_logs TEXT DEFAULT '[]';

                UPDATE schema_version SET version = 3;
                ",
            )?;
        }

        if version < 4 {
            info!("Applying database migration v4");
            self.conn.execute_batch(
                "
                -- RSS feed items (persistent feed cache)
                CREATE TABLE IF NOT EXISTS rss_items (
                    id TEXT PRIMARY KEY,
                    feed_name TEXT NOT NULL,
                    title TEXT NOT NULL,
                    url TEXT,
                    published_at TEXT,
                    first_seen_at TEXT NOT NULL,
                    downloaded INTEGER NOT NULL DEFAULT 0,
                    downloaded_at TEXT,
                    category TEXT,
                    size_bytes INTEGER DEFAULT 0
                );

                CREATE INDEX IF NOT EXISTS idx_rss_items_feed ON rss_items(feed_name);
                CREATE INDEX IF NOT EXISTS idx_rss_items_seen ON rss_items(first_seen_at DESC);

                -- RSS download rules
                CREATE TABLE IF NOT EXISTS rss_rules (
                    id TEXT PRIMARY KEY,
                    name TEXT NOT NULL,
                    feed_name TEXT NOT NULL,
                    category TEXT,
                    priority INTEGER NOT NULL DEFAULT 1,
                    match_regex TEXT NOT NULL,
                    enabled INTEGER NOT NULL DEFAULT 1
                );

                UPDATE schema_version SET version = 4;
                ",
            )?;
        }

        if version < 5 {
            info!("Applying database migration v5: settings table");
            self.conn.execute_batch(
                "
                CREATE TABLE IF NOT EXISTS settings (
                    key TEXT PRIMARY KEY,
                    value TEXT NOT NULL
                );

                UPDATE schema_version SET version = 5;
                ",
            )?;
        }

        // Ensure settings table exists for databases that jumped to v5
        // with groups tables but without settings (pre-extraction rustnzbd)
        self.conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS settings (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );",
        )?;

        #[cfg(feature = "groups-db")]
        if version < 6 {
            info!("Applying database migration v6: newsgroup browsing");
            self.conn.execute_batch(
                "
                CREATE TABLE IF NOT EXISTS groups (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    name TEXT NOT NULL UNIQUE,
                    description TEXT,
                    subscribed INTEGER NOT NULL DEFAULT 0,
                    article_count INTEGER NOT NULL DEFAULT 0,
                    first_article INTEGER NOT NULL DEFAULT 0,
                    last_article INTEGER NOT NULL DEFAULT 0,
                    last_scanned INTEGER NOT NULL DEFAULT 0,
                    last_updated TEXT,
                    created_at TEXT NOT NULL DEFAULT (datetime('now'))
                );
                CREATE INDEX IF NOT EXISTS idx_groups_subscribed ON groups(subscribed);
                CREATE INDEX IF NOT EXISTS idx_groups_name ON groups(name);

                CREATE TABLE IF NOT EXISTS headers (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    group_id INTEGER NOT NULL REFERENCES groups(id) ON DELETE CASCADE,
                    article_num INTEGER NOT NULL,
                    subject TEXT NOT NULL,
                    author TEXT NOT NULL,
                    date TEXT NOT NULL,
                    message_id TEXT NOT NULL,
                    references_ TEXT NOT NULL DEFAULT '',
                    bytes INTEGER NOT NULL DEFAULT 0,
                    lines INTEGER NOT NULL DEFAULT 0,
                    read INTEGER NOT NULL DEFAULT 0,
                    downloaded_at TEXT NOT NULL DEFAULT (datetime('now'))
                );
                CREATE INDEX IF NOT EXISTS idx_headers_group ON headers(group_id);
                CREATE INDEX IF NOT EXISTS idx_headers_msgid ON headers(message_id);
                CREATE INDEX IF NOT EXISTS idx_headers_artnum ON headers(group_id, article_num);

                CREATE VIRTUAL TABLE IF NOT EXISTS headers_fts USING fts5(
                    subject, author, content='headers', content_rowid='id',
                    tokenize='porter unicode61'
                );
                CREATE TRIGGER IF NOT EXISTS headers_fts_ins AFTER INSERT ON headers BEGIN
                    INSERT INTO headers_fts(rowid, subject, author) VALUES (new.id, new.subject, new.author);
                END;
                CREATE TRIGGER IF NOT EXISTS headers_fts_del AFTER DELETE ON headers BEGIN
                    INSERT INTO headers_fts(headers_fts, rowid, subject, author) VALUES ('delete', old.id, old.subject, old.author);
                END;

                UPDATE schema_version SET version = 6;
                ",
            )?;
        }

        if version < 7 {
            info!("Applying database migration v7: persistent download statistics");
            self.conn.execute_batch(
                "
                CREATE TABLE IF NOT EXISTS download_statistics (
                    job_id TEXT PRIMARY KEY,
                    completed_at TEXT NOT NULL,
                    status TEXT NOT NULL,
                    total_bytes INTEGER NOT NULL DEFAULT 0,
                    downloaded_bytes INTEGER NOT NULL DEFAULT 0,
                    duration_secs REAL NOT NULL DEFAULT 0,
                    average_speed_bps INTEGER NOT NULL DEFAULT 0,
                    server_stats TEXT NOT NULL DEFAULT '[]'
                );

                CREATE INDEX IF NOT EXISTS idx_download_statistics_completed
                    ON download_statistics(completed_at DESC);

                INSERT OR IGNORE INTO download_statistics (
                    job_id, completed_at, status, total_bytes, downloaded_bytes,
                    duration_secs, average_speed_bps, server_stats
                )
                SELECT id, completed_at, status, total_bytes, downloaded_bytes,
                    MAX(0, (julianday(completed_at) - julianday(added_at)) * 86400.0),
                    CASE
                        WHEN julianday(completed_at) > julianday(added_at)
                        THEN CAST(downloaded_bytes /
                            ((julianday(completed_at) - julianday(added_at)) * 86400.0) AS INTEGER)
                        ELSE 0
                    END,
                    COALESCE(server_stats, '[]')
                FROM history;

                DELETE FROM schema_version;
                INSERT INTO schema_version (version) VALUES (7);
                ",
            )?;
        }

        if version < 8 {
            info!("Applying database migration v8: active download duration");
            self.conn.execute_batch(
                "
                ALTER TABLE history ADD COLUMN download_time_secs REAL;
                DELETE FROM schema_version;
                INSERT INTO schema_version (version) VALUES (8);
                ",
            )?;
        }

        Ok(())
    }

    /// Read a setting by key.
    pub fn get_setting(&self, key: &str) -> Option<String> {
        self.conn
            .query_row("SELECT value FROM settings WHERE key = ?1", [key], |row| {
                row.get(0)
            })
            .ok()
    }

    /// Write a setting (upsert).
    pub fn set_setting(&self, key: &str, value: &str) {
        let _ = self.conn.execute(
            "INSERT INTO settings (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = ?2",
            [key, value],
        );
    }

    // -----------------------------------------------------------------------
    // Queue operations
    // -----------------------------------------------------------------------

    /// Insert a new job into the queue.
    pub fn queue_insert(&self, job: &NzbJob) -> Result<(), NzbError> {
        self.conn.execute(
            "INSERT INTO queue (id, name, category, status, priority, total_bytes,
             downloaded_bytes, file_count, files_completed, article_count,
             articles_downloaded, articles_failed, added_at, work_dir, output_dir, password)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
            params![
                job.id,
                job.name,
                job.category,
                job.status.to_string(),
                job.priority as i32,
                job.total_bytes as i64,
                job.downloaded_bytes as i64,
                job.file_count as i64,
                job.files_completed as i64,
                job.article_count as i64,
                job.articles_downloaded as i64,
                job.articles_failed as i64,
                job.added_at.to_rfc3339(),
                job.work_dir.to_string_lossy().to_string(),
                job.output_dir.to_string_lossy().to_string(),
                job.password,
            ],
        )?;
        Ok(())
    }

    /// Update job progress in the queue.
    pub fn queue_update_progress(
        &self,
        id: &str,
        status: JobStatus,
        downloaded_bytes: u64,
        articles_downloaded: usize,
        articles_failed: usize,
        files_completed: usize,
    ) -> Result<(), NzbError> {
        self.conn.execute(
            "UPDATE queue SET status=?2, downloaded_bytes=?3, articles_downloaded=?4,
             articles_failed=?5, files_completed=?6 WHERE id=?1",
            params![
                id,
                status.to_string(),
                downloaded_bytes as i64,
                articles_downloaded as i64,
                articles_failed as i64,
                files_completed as i64,
            ],
        )?;
        Ok(())
    }

    /// Update job priority in the queue.
    pub fn queue_update_priority(&self, id: &str, priority: i32) -> Result<(), NzbError> {
        self.conn.execute(
            "UPDATE queue SET priority = ?2 WHERE id = ?1",
            params![id, priority],
        )?;
        Ok(())
    }

    /// Remove a job from the queue.
    pub fn queue_remove(&self, id: &str) -> Result<(), NzbError> {
        self.conn
            .execute("DELETE FROM queue WHERE id=?1", params![id])?;
        Ok(())
    }

    /// List all jobs in the queue, ordered by priority then add time.
    pub fn queue_list(&self) -> Result<Vec<NzbJob>, NzbError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, name, category, status, priority, total_bytes, downloaded_bytes,
             file_count, files_completed, article_count, articles_downloaded, articles_failed,
             added_at, completed_at, work_dir, output_dir, password, error_message
             FROM queue ORDER BY priority DESC, added_at ASC",
        )?;

        let jobs = stmt
            .query_map([], |row| {
                Ok(NzbJob {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    category: row.get(2)?,
                    status: parse_status(&row.get::<_, String>(3)?),
                    priority: parse_priority(row.get::<_, i32>(4)?),
                    total_bytes: row.get::<_, i64>(5)? as u64,
                    downloaded_bytes: row.get::<_, i64>(6)? as u64,
                    file_count: row.get::<_, i64>(7)? as usize,
                    files_completed: row.get::<_, i64>(8)? as usize,
                    article_count: row.get::<_, i64>(9)? as usize,
                    articles_downloaded: row.get::<_, i64>(10)? as usize,
                    articles_failed: row.get::<_, i64>(11)? as usize,
                    added_at: parse_datetime(&row.get::<_, String>(12)?),
                    completed_at: row
                        .get::<_, Option<String>>(13)?
                        .map(|s| parse_datetime(&s)),
                    work_dir: row.get::<_, String>(14)?.into(),
                    output_dir: row.get::<_, String>(15)?.into(),
                    password: row.get(16)?,
                    error_message: row.get(17)?,
                    speed_bps: 0,
                    server_stats: Vec::new(),
                    files: Vec::new(), // Loaded separately
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;

        Ok(jobs)
    }

    // -----------------------------------------------------------------------
    // History operations
    // -----------------------------------------------------------------------

    /// Move a completed/failed job to history.
    pub fn history_insert(&self, entry: &HistoryEntry) -> Result<(), NzbError> {
        let stages_json = serde_json::to_string(&entry.stages).unwrap_or_default();
        let server_stats_json = serde_json::to_string(&entry.server_stats).unwrap_or_default();
        self.conn.execute(
            "INSERT INTO history (id, name, category, status, total_bytes, downloaded_bytes,
             added_at, completed_at, download_time_secs, output_dir, stages, error_message, nzb_data, server_stats)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
            params![
                entry.id,
                entry.name,
                entry.category,
                entry.status.to_string(),
                entry.total_bytes as i64,
                entry.downloaded_bytes as i64,
                entry.added_at.to_rfc3339(),
                entry.completed_at.to_rfc3339(),
                entry.download_time_secs,
                entry.output_dir.to_string_lossy().to_string(),
                stages_json,
                entry.error_message,
                entry.nzb_data,
                server_stats_json,
            ],
        )?;

        let duration_secs = entry.download_time_secs.unwrap_or_else(|| {
            (entry.completed_at - entry.added_at)
                .num_milliseconds()
                .max(0) as f64
                / 1000.0
        });
        let average_speed_bps = if duration_secs > 0.0 {
            (entry.downloaded_bytes as f64 / duration_secs) as u64
        } else {
            0
        };
        self.conn.execute(
            "INSERT OR REPLACE INTO download_statistics (
                job_id, completed_at, status, total_bytes, downloaded_bytes,
                duration_secs, average_speed_bps, server_stats
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                entry.id,
                entry.completed_at.to_rfc3339(),
                entry.status.to_string(),
                entry.total_bytes as i64,
                entry.downloaded_bytes as i64,
                duration_secs,
                average_speed_bps as i64,
                server_stats_json,
            ],
        )?;
        Ok(())
    }

    /// Return the compact, permanent download statistics ledger.
    pub fn download_statistics_list(&self) -> Result<Vec<DownloadStatistic>, NzbError> {
        let mut stmt = self.conn.prepare(
            "SELECT job_id, completed_at, status, total_bytes, downloaded_bytes,
                    duration_secs, average_speed_bps, server_stats
             FROM download_statistics ORDER BY completed_at DESC",
        )?;

        let rows = stmt
            .query_map([], |row| {
                let stats_json: String = row.get(7)?;
                Ok(DownloadStatistic {
                    job_id: row.get(0)?,
                    completed_at: parse_datetime(&row.get::<_, String>(1)?),
                    status: parse_status(&row.get::<_, String>(2)?),
                    total_bytes: row.get::<_, i64>(3)? as u64,
                    downloaded_bytes: row.get::<_, i64>(4)? as u64,
                    duration_secs: row.get(5)?,
                    average_speed_bps: row.get::<_, i64>(6)? as u64,
                    server_stats: serde_json::from_str(&stats_json).unwrap_or_default(),
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// List history entries, most recent first.
    pub fn history_list(&self, limit: usize) -> Result<Vec<HistoryEntry>, NzbError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, name, category, status, total_bytes, downloaded_bytes,
             added_at, completed_at, download_time_secs, output_dir, stages, error_message, server_stats,
             CASE WHEN nzb_data IS NOT NULL THEN 1 ELSE 0 END as has_nzb
             FROM history ORDER BY completed_at DESC LIMIT ?1",
        )?;

        let entries = stmt
            .query_map(params![limit as i64], |row| {
                let stages_json: String = row.get::<_, Option<String>>(10)?.unwrap_or_default();
                let stages: Vec<StageResult> =
                    serde_json::from_str(&stages_json).unwrap_or_default();
                let stats_json: String = row.get::<_, Option<String>>(12)?.unwrap_or_default();
                let server_stats: Vec<ServerArticleStats> =
                    serde_json::from_str(&stats_json).unwrap_or_default();
                let has_nzb: i64 = row.get(13)?;

                Ok(HistoryEntry {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    category: row.get(2)?,
                    status: parse_status(&row.get::<_, String>(3)?),
                    total_bytes: row.get::<_, i64>(4)? as u64,
                    downloaded_bytes: row.get::<_, i64>(5)? as u64,
                    added_at: parse_datetime(&row.get::<_, String>(6)?),
                    completed_at: parse_datetime(&row.get::<_, String>(7)?),
                    download_time_secs: row.get(8)?,
                    output_dir: row.get::<_, String>(9)?.into(),
                    stages,
                    error_message: row.get(11)?,
                    server_stats,
                    // Don't load actual blob in list - just note if it exists
                    nzb_data: if has_nzb != 0 { Some(Vec::new()) } else { None },
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;

        Ok(entries)
    }

    /// Get the raw NZB data for a history entry (for retry).
    pub fn history_get_nzb_data(&self, id: &str) -> Result<Option<Vec<u8>>, NzbError> {
        let result = self.conn.query_row(
            "SELECT nzb_data FROM history WHERE id = ?1",
            params![id],
            |row| row.get::<_, Option<Vec<u8>>>(0),
        );
        match result {
            Ok(data) => Ok(data),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(NzbError::Database(e)),
        }
    }

    /// Enforce history retention limit by deleting oldest entries.
    pub fn history_enforce_retention(&self, max_entries: usize) -> Result<(), NzbError> {
        self.conn.execute(
            "DELETE FROM history WHERE id NOT IN (
                SELECT id FROM history ORDER BY completed_at DESC LIMIT ?1
            )",
            params![max_entries as i64],
        )?;
        Ok(())
    }

    /// Get a single history entry by ID.
    pub fn history_get(&self, id: &str) -> Result<Option<HistoryEntry>, NzbError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, name, category, status, total_bytes, downloaded_bytes,
             added_at, completed_at, download_time_secs, output_dir, stages, error_message, server_stats
             FROM history WHERE id = ?1",
        )?;

        let result = stmt.query_row(params![id], |row| {
            let stages_json: String = row.get::<_, Option<String>>(10)?.unwrap_or_default();
            let stages: Vec<StageResult> = serde_json::from_str(&stages_json).unwrap_or_default();
            let stats_json: String = row.get::<_, Option<String>>(12)?.unwrap_or_default();
            let server_stats: Vec<ServerArticleStats> =
                serde_json::from_str(&stats_json).unwrap_or_default();

            Ok(HistoryEntry {
                id: row.get(0)?,
                name: row.get(1)?,
                category: row.get(2)?,
                status: parse_status(&row.get::<_, String>(3)?),
                total_bytes: row.get::<_, i64>(4)? as u64,
                downloaded_bytes: row.get::<_, i64>(5)? as u64,
                added_at: parse_datetime(&row.get::<_, String>(6)?),
                completed_at: parse_datetime(&row.get::<_, String>(7)?),
                download_time_secs: row.get(8)?,
                output_dir: row.get::<_, String>(9)?.into(),
                stages,
                error_message: row.get(11)?,
                server_stats,
                nzb_data: None,
            })
        });

        match result {
            Ok(entry) => Ok(Some(entry)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(NzbError::Database(e)),
        }
    }

    /// Store serialized job file/article state for resume support.
    pub fn queue_store_job_data(&self, id: &str, data: &[u8]) -> Result<(), NzbError> {
        self.conn.execute(
            "UPDATE queue SET job_data = ?2 WHERE id = ?1",
            params![id, data],
        )?;
        Ok(())
    }

    /// Load serialized job file/article state for resume.
    pub fn queue_load_job_data(&self, id: &str) -> Result<Option<Vec<u8>>, NzbError> {
        let mut stmt = self
            .conn
            .prepare("SELECT job_data FROM queue WHERE id = ?1")?;
        let result = stmt.query_row(params![id], |row| row.get::<_, Option<Vec<u8>>>(0))?;
        Ok(result)
    }

    /// Store raw NZB data for a queue job.
    pub fn queue_store_nzb_data(&self, id: &str, nzb_data: &[u8]) -> Result<(), NzbError> {
        self.conn.execute(
            "UPDATE queue SET nzb_raw = ?2 WHERE id = ?1",
            params![id, nzb_data],
        )?;
        Ok(())
    }

    /// Get raw NZB data from a queue job.
    pub fn queue_get_nzb_data(&self, id: &str) -> Result<Option<Vec<u8>>, NzbError> {
        let result = self.conn.query_row(
            "SELECT nzb_raw FROM queue WHERE id = ?1",
            params![id],
            |row| row.get::<_, Option<Vec<u8>>>(0),
        );
        match result {
            Ok(data) => Ok(data),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(NzbError::Database(e)),
        }
    }

    /// Count history entries.
    pub fn history_count(&self) -> Result<usize, NzbError> {
        let count: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM history", [], |row| row.get(0))?;
        Ok(count as usize)
    }

    /// Remove a history entry.
    pub fn history_remove(&self, id: &str) -> Result<(), NzbError> {
        self.conn
            .execute("DELETE FROM history WHERE id=?1", params![id])?;
        Ok(())
    }

    /// Clear all history.
    pub fn history_clear(&self) -> Result<(), NzbError> {
        self.conn.execute("DELETE FROM history", [])?;
        Ok(())
    }

    /// Store per-job logs for a history entry.
    pub fn history_store_logs(&self, id: &str, logs_json: &str) -> Result<(), NzbError> {
        self.conn.execute(
            "UPDATE history SET job_logs = ?2 WHERE id = ?1",
            params![id, logs_json],
        )?;
        Ok(())
    }

    /// Get per-job logs for a history entry.
    pub fn history_get_logs(&self, id: &str) -> Result<Option<String>, NzbError> {
        let result = self.conn.query_row(
            "SELECT job_logs FROM history WHERE id = ?1",
            params![id],
            |row| row.get::<_, Option<String>>(0),
        );
        match result {
            Ok(data) => Ok(data),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(NzbError::Database(e)),
        }
    }

    // -----------------------------------------------------------------------
    // RSS item operations
    // -----------------------------------------------------------------------

    /// Upsert an RSS feed item (insert or ignore if already exists).
    pub fn rss_item_upsert(&self, item: &RssItem) -> Result<(), NzbError> {
        self.conn.execute(
            "INSERT OR IGNORE INTO rss_items (id, feed_name, title, url, published_at,
             first_seen_at, downloaded, downloaded_at, category, size_bytes)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                item.id,
                item.feed_name,
                item.title,
                item.url,
                item.published_at.map(|d| d.to_rfc3339()),
                item.first_seen_at.to_rfc3339(),
                item.downloaded as i32,
                item.downloaded_at.map(|d| d.to_rfc3339()),
                item.category,
                item.size_bytes as i64,
            ],
        )?;
        Ok(())
    }

    /// Batch upsert RSS feed items in a single transaction.
    /// Returns the number of newly inserted items.
    pub fn rss_items_batch_upsert(&self, items: &[RssItem]) -> Result<usize, NzbError> {
        let tx = self.conn.unchecked_transaction()?;
        let mut inserted = 0usize;
        {
            let mut stmt = tx.prepare_cached(
                "INSERT OR IGNORE INTO rss_items (id, feed_name, title, url, published_at,
                 first_seen_at, downloaded, downloaded_at, category, size_bytes)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            )?;
            for item in items {
                let rows = stmt.execute(params![
                    item.id,
                    item.feed_name,
                    item.title,
                    item.url,
                    item.published_at.map(|d| d.to_rfc3339()),
                    item.first_seen_at.to_rfc3339(),
                    item.downloaded as i32,
                    item.downloaded_at.map(|d| d.to_rfc3339()),
                    item.category,
                    item.size_bytes as i64,
                ])?;
                if rows > 0 {
                    inserted += 1;
                }
            }
        }
        tx.commit()?;
        Ok(inserted)
    }

    /// Check if an RSS item ID already exists in the database.
    pub fn rss_item_exists(&self, id: &str) -> Result<bool, NzbError> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM rss_items WHERE id = ?1",
            params![id],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    /// List RSS items, optionally filtered by feed name, ordered by first_seen_at DESC.
    pub fn rss_items_list(
        &self,
        feed_name: Option<&str>,
        limit: usize,
    ) -> Result<Vec<RssItem>, NzbError> {
        let (sql, limit_val) = if let Some(name) = feed_name {
            let mut stmt = self.conn.prepare(
                "SELECT id, feed_name, title, url, published_at, first_seen_at,
                 downloaded, downloaded_at, category, size_bytes
                 FROM rss_items WHERE feed_name = ?1
                 ORDER BY first_seen_at DESC LIMIT ?2",
            )?;
            let items = stmt
                .query_map(params![name, limit as i64], |row| self.map_rss_item(row))?
                .collect::<Result<Vec<_>, _>>()?;
            return Ok(items);
        } else {
            (
                "SELECT id, feed_name, title, url, published_at, first_seen_at,
                 downloaded, downloaded_at, category, size_bytes
                 FROM rss_items ORDER BY first_seen_at DESC LIMIT ?1",
                limit,
            )
        };
        let mut stmt = self.conn.prepare(sql)?;
        let items = stmt
            .query_map(params![limit_val as i64], |row| self.map_rss_item(row))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(items)
    }

    /// Get a single RSS item by ID.
    pub fn rss_item_get(&self, id: &str) -> Result<Option<RssItem>, NzbError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, feed_name, title, url, published_at, first_seen_at,
             downloaded, downloaded_at, category, size_bytes
             FROM rss_items WHERE id = ?1",
        )?;
        let result = stmt.query_row(params![id], |row| self.map_rss_item(row));
        match result {
            Ok(item) => Ok(Some(item)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(NzbError::Database(e)),
        }
    }

    /// Mark an RSS item as downloaded.
    pub fn rss_item_mark_downloaded(
        &self,
        id: &str,
        category: Option<&str>,
    ) -> Result<(), NzbError> {
        self.conn.execute(
            "UPDATE rss_items SET downloaded = 1, downloaded_at = ?2, category = ?3 WHERE id = ?1",
            params![id, Utc::now().to_rfc3339(), category],
        )?;
        Ok(())
    }

    /// Count total RSS items.
    pub fn rss_item_count(&self) -> Result<usize, NzbError> {
        let count: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM rss_items", [], |row| row.get(0))?;
        Ok(count as usize)
    }

    /// Prune RSS items to keep only the N most recent (by first_seen_at).
    pub fn rss_items_prune(&self, keep: usize) -> Result<usize, NzbError> {
        let deleted = self.conn.execute(
            "DELETE FROM rss_items WHERE id NOT IN (
                SELECT id FROM rss_items ORDER BY first_seen_at DESC LIMIT ?1
            )",
            params![keep as i64],
        )?;
        Ok(deleted)
    }

    fn map_rss_item(&self, row: &rusqlite::Row<'_>) -> rusqlite::Result<RssItem> {
        Ok(RssItem {
            id: row.get(0)?,
            feed_name: row.get(1)?,
            title: row.get(2)?,
            url: row.get(3)?,
            published_at: row.get::<_, Option<String>>(4)?.map(|s| parse_datetime(&s)),
            first_seen_at: parse_datetime(&row.get::<_, String>(5)?),
            downloaded: row.get::<_, i32>(6)? != 0,
            downloaded_at: row.get::<_, Option<String>>(7)?.map(|s| parse_datetime(&s)),
            category: row.get(8)?,
            size_bytes: row.get::<_, i64>(9)? as u64,
        })
    }

    // -----------------------------------------------------------------------
    // RSS rule operations
    // -----------------------------------------------------------------------

    /// Insert a new RSS download rule.
    /// feed_names is stored as comma-separated string in the DB.
    pub fn rss_rule_insert(&self, rule: &RssRule) -> Result<(), NzbError> {
        let feed_names_str = rule.feed_names.join(",");
        self.conn.execute(
            "INSERT INTO rss_rules (id, name, feed_name, category, priority, match_regex, enabled)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                rule.id,
                rule.name,
                feed_names_str,
                rule.category,
                rule.priority,
                rule.match_regex,
                rule.enabled as i32,
            ],
        )?;
        Ok(())
    }

    /// List all RSS download rules.
    pub fn rss_rule_list(&self) -> Result<Vec<RssRule>, NzbError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, name, feed_name, category, priority, match_regex, enabled
             FROM rss_rules ORDER BY name ASC",
        )?;
        let rules = stmt
            .query_map([], |row| {
                let feed_names_str: String = row.get(2)?;
                let feed_names: Vec<String> = feed_names_str
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
                Ok(RssRule {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    feed_names,
                    category: row.get(3)?,
                    priority: row.get(4)?,
                    match_regex: row.get(5)?,
                    enabled: row.get::<_, i32>(6)? != 0,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rules)
    }

    /// Update an RSS download rule.
    pub fn rss_rule_update(&self, rule: &RssRule) -> Result<(), NzbError> {
        let feed_names_str = rule.feed_names.join(",");
        self.conn.execute(
            "UPDATE rss_rules SET name=?2, feed_name=?3, category=?4, priority=?5,
             match_regex=?6, enabled=?7 WHERE id=?1",
            params![
                rule.id,
                rule.name,
                feed_names_str,
                rule.category,
                rule.priority,
                rule.match_regex,
                rule.enabled as i32,
            ],
        )?;
        Ok(())
    }

    /// Delete an RSS download rule.
    pub fn rss_rule_delete(&self, id: &str) -> Result<(), NzbError> {
        self.conn
            .execute("DELETE FROM rss_rules WHERE id=?1", params![id])?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Parse helpers
// ---------------------------------------------------------------------------

fn parse_status(s: &str) -> JobStatus {
    match s.to_lowercase().as_str() {
        "queued" => JobStatus::Queued,
        "downloading" => JobStatus::Downloading,
        "paused" => JobStatus::Paused,
        "verifying" => JobStatus::Verifying,
        "repairing" => JobStatus::Repairing,
        "extracting" => JobStatus::Extracting,
        "postprocessing" => JobStatus::PostProcessing,
        "completed" => JobStatus::Completed,
        "failed" => JobStatus::Failed,
        _ => JobStatus::Queued,
    }
}

fn parse_priority(v: i32) -> Priority {
    match v {
        0 => Priority::Low,
        1 => Priority::Normal,
        2 => Priority::High,
        3 => Priority::Force,
        _ => Priority::Normal,
    }
}

fn parse_datetime(s: &str) -> chrono::DateTime<Utc> {
    chrono::DateTime::parse_from_rfc3339(s)
        .map(|dt| dt.with_timezone(&Utc))
        .unwrap_or_else(|_| Utc::now())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_job(id: &str, name: &str) -> NzbJob {
        NzbJob {
            id: id.into(),
            name: name.into(),
            category: "Default".into(),
            status: JobStatus::Queued,
            priority: Priority::Normal,
            total_bytes: 1_000_000,
            downloaded_bytes: 0,
            file_count: 3,
            files_completed: 0,
            article_count: 30,
            articles_downloaded: 0,
            articles_failed: 0,
            added_at: Utc::now(),
            completed_at: None,
            work_dir: "/tmp/test".into(),
            output_dir: "/downloads/test".into(),
            password: None,
            error_message: None,
            speed_bps: 0,
            server_stats: Vec::new(),
            files: Vec::new(),
        }
    }

    fn make_history(id: &str, name: &str) -> HistoryEntry {
        HistoryEntry {
            id: id.into(),
            name: name.into(),
            category: "movies".into(),
            status: JobStatus::Completed,
            total_bytes: 5_000_000,
            downloaded_bytes: 5_000_000,
            added_at: Utc::now(),
            completed_at: Utc::now(),
            output_dir: "/downloads/complete".into(),
            download_time_secs: None,
            stages: vec![StageResult {
                name: "Verify".into(),
                status: StageStatus::Success,
                message: None,
                duration_secs: 2.5,
            }],
            error_message: None,
            server_stats: Vec::new(),
            nzb_data: None,
        }
    }

    fn make_rss_item(id: &str, feed: &str, title: &str) -> RssItem {
        RssItem {
            id: id.into(),
            feed_name: feed.into(),
            title: title.into(),
            url: Some("https://example.com/nzb".into()),
            published_at: Some(Utc::now()),
            first_seen_at: Utc::now(),
            downloaded: false,
            downloaded_at: None,
            category: None,
            size_bytes: 1_000_000,
        }
    }

    // -----------------------------------------------------------------------
    // Schema & migration
    // -----------------------------------------------------------------------

    #[test]
    fn test_db_create_and_migrate() {
        let db = Database::open_memory().unwrap();
        let jobs = db.queue_list().unwrap();
        assert!(jobs.is_empty());
    }

    // -----------------------------------------------------------------------
    // Queue operations
    // -----------------------------------------------------------------------

    #[test]
    fn test_queue_insert_and_list() {
        let db = Database::open_memory().unwrap();
        let job = make_job("test-123", "Test Download");

        db.queue_insert(&job).unwrap();
        let jobs = db.queue_list().unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].name, "Test Download");
        assert_eq!(jobs[0].total_bytes, 1_000_000);
    }

    #[test]
    fn test_queue_update_progress() {
        let db = Database::open_memory().unwrap();
        db.queue_insert(&make_job("q1", "Job 1")).unwrap();

        db.queue_update_progress("q1", JobStatus::Downloading, 500_000, 15, 2, 1)
            .unwrap();

        let jobs = db.queue_list().unwrap();
        assert_eq!(jobs[0].status, JobStatus::Downloading);
        assert_eq!(jobs[0].downloaded_bytes, 500_000);
        assert_eq!(jobs[0].articles_downloaded, 15);
        assert_eq!(jobs[0].articles_failed, 2);
        assert_eq!(jobs[0].files_completed, 1);
    }

    #[test]
    fn test_queue_update_priority() {
        let db = Database::open_memory().unwrap();
        db.queue_insert(&make_job("q2", "Job 2")).unwrap();

        db.queue_update_priority("q2", 3).unwrap();

        let jobs = db.queue_list().unwrap();
        assert_eq!(jobs[0].priority, Priority::Force);
    }

    #[test]
    fn test_queue_remove() {
        let db = Database::open_memory().unwrap();
        db.queue_insert(&make_job("q3", "Job 3")).unwrap();
        assert_eq!(db.queue_list().unwrap().len(), 1);

        db.queue_remove("q3").unwrap();
        assert_eq!(db.queue_list().unwrap().len(), 0);
    }

    #[test]
    fn test_queue_ordering() {
        let db = Database::open_memory().unwrap();

        let mut low = make_job("low", "Low Priority");
        low.priority = Priority::Low;
        let mut high = make_job("high", "High Priority");
        high.priority = Priority::High;
        let normal = make_job("normal", "Normal Priority");

        db.queue_insert(&low).unwrap();
        db.queue_insert(&high).unwrap();
        db.queue_insert(&normal).unwrap();

        let jobs = db.queue_list().unwrap();
        assert_eq!(jobs[0].id, "high");
        assert_eq!(jobs[1].id, "normal");
        assert_eq!(jobs[2].id, "low");
    }

    #[test]
    fn test_queue_store_and_load_job_data() {
        let db = Database::open_memory().unwrap();
        db.queue_insert(&make_job("jd1", "Job Data")).unwrap();

        let blob = vec![1u8, 2, 3, 4, 5, 6, 7, 8];
        db.queue_store_job_data("jd1", &blob).unwrap();

        let loaded = db.queue_load_job_data("jd1").unwrap();
        assert_eq!(loaded, Some(blob));
    }

    #[test]
    fn test_queue_load_job_data_empty() {
        let db = Database::open_memory().unwrap();
        db.queue_insert(&make_job("jd2", "No Data")).unwrap();

        let loaded = db.queue_load_job_data("jd2").unwrap();
        assert!(loaded.is_none());
    }

    #[test]
    fn test_queue_store_and_get_nzb_data() {
        let db = Database::open_memory().unwrap();
        db.queue_insert(&make_job("nzb1", "NZB Store")).unwrap();

        let nzb = b"<nzb>...</nzb>".to_vec();
        db.queue_store_nzb_data("nzb1", &nzb).unwrap();

        let loaded = db.queue_get_nzb_data("nzb1").unwrap();
        assert_eq!(loaded, Some(nzb));
    }

    #[test]
    fn test_queue_get_nzb_data_nonexistent() {
        let db = Database::open_memory().unwrap();
        let result = db.queue_get_nzb_data("nonexistent").unwrap();
        assert!(result.is_none());
    }

    // -----------------------------------------------------------------------
    // History operations
    // -----------------------------------------------------------------------

    #[test]
    fn test_history_insert_and_list() {
        let db = Database::open_memory().unwrap();
        let entry = make_history("hist-1", "Completed Job");

        db.history_insert(&entry).unwrap();
        let history = db.history_list(10).unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].name, "Completed Job");
        assert_eq!(history[0].stages.len(), 1);
    }

    #[test]
    fn test_history_get_by_id() {
        let db = Database::open_memory().unwrap();
        db.history_insert(&make_history("h1", "History 1")).unwrap();

        let entry = db.history_get("h1").unwrap();
        assert!(entry.is_some());
        assert_eq!(entry.unwrap().name, "History 1");
    }

    #[test]
    fn test_history_get_nonexistent() {
        let db = Database::open_memory().unwrap();
        let entry = db.history_get("nonexistent").unwrap();
        assert!(entry.is_none());
    }

    #[test]
    fn test_history_get_nzb_data() {
        let db = Database::open_memory().unwrap();
        let mut entry = make_history("h-nzb", "With NZB");
        entry.nzb_data = Some(b"<nzb>data</nzb>".to_vec());
        db.history_insert(&entry).unwrap();

        let data = db.history_get_nzb_data("h-nzb").unwrap();
        assert!(data.is_some());
        assert_eq!(data.unwrap(), b"<nzb>data</nzb>");
    }

    #[test]
    fn test_history_get_nzb_data_nonexistent() {
        let db = Database::open_memory().unwrap();
        let data = db.history_get_nzb_data("missing").unwrap();
        assert!(data.is_none());
    }

    #[test]
    fn test_history_count() {
        let db = Database::open_memory().unwrap();
        assert_eq!(db.history_count().unwrap(), 0);

        db.history_insert(&make_history("hc1", "Job 1")).unwrap();
        db.history_insert(&make_history("hc2", "Job 2")).unwrap();
        db.history_insert(&make_history("hc3", "Job 3")).unwrap();
        assert_eq!(db.history_count().unwrap(), 3);
    }

    #[test]
    fn test_history_remove() {
        let db = Database::open_memory().unwrap();
        db.history_insert(&make_history("hr1", "To Remove"))
            .unwrap();
        assert_eq!(db.history_count().unwrap(), 1);

        db.history_remove("hr1").unwrap();
        assert_eq!(db.history_count().unwrap(), 0);
    }

    #[test]
    fn test_history_clear() {
        let db = Database::open_memory().unwrap();
        db.history_insert(&make_history("hcl1", "Job 1")).unwrap();
        db.history_insert(&make_history("hcl2", "Job 2")).unwrap();
        assert_eq!(db.history_count().unwrap(), 2);

        db.history_clear().unwrap();
        assert_eq!(db.history_count().unwrap(), 0);
    }

    #[test]
    fn test_history_enforce_retention() {
        let db = Database::open_memory().unwrap();
        for i in 0..5 {
            db.history_insert(&make_history(&format!("ret-{i}"), &format!("Job {i}")))
                .unwrap();
        }
        assert_eq!(db.history_count().unwrap(), 5);

        db.history_enforce_retention(3).unwrap();
        assert_eq!(db.history_count().unwrap(), 3);
    }

    #[test]
    fn test_history_store_and_get_logs() {
        let db = Database::open_memory().unwrap();
        db.history_insert(&make_history("hl1", "With Logs"))
            .unwrap();

        let logs = r#"[{"ts":"2024-01-01","msg":"Started"}]"#;
        db.history_store_logs("hl1", logs).unwrap();

        let loaded = db.history_get_logs("hl1").unwrap();
        assert_eq!(loaded.as_deref(), Some(logs));
    }

    #[test]
    fn test_history_get_logs_nonexistent() {
        let db = Database::open_memory().unwrap();
        let result = db.history_get_logs("missing").unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_history_with_server_stats() {
        let db = Database::open_memory().unwrap();
        let mut entry = make_history("hss", "Stats Job");
        entry.server_stats = vec![ServerArticleStats {
            server_id: "srv-1".into(),
            server_name: "Provider".into(),
            articles_downloaded: 100,
            articles_failed: 5,
            bytes_downloaded: 75_000_000,
        }];
        db.history_insert(&entry).unwrap();

        let loaded = db.history_list(10).unwrap();
        assert_eq!(loaded[0].server_stats.len(), 1);
        assert_eq!(loaded[0].server_stats[0].server_id, "srv-1");
        assert_eq!(loaded[0].server_stats[0].articles_downloaded, 100);
    }

    #[test]
    fn test_download_statistics_survive_history_deletion() {
        let db = Database::open_memory().unwrap();
        let mut entry = make_history("stats-1", "Statistics Job");
        entry.completed_at = Utc::now();
        entry.added_at = entry.completed_at - chrono::Duration::seconds(10);
        entry.downloaded_bytes = 10_000;
        entry.server_stats = vec![ServerArticleStats {
            server_id: "server-1".into(),
            server_name: "Primary".into(),
            articles_downloaded: 9,
            articles_failed: 1,
            bytes_downloaded: 10_000,
        }];

        db.history_insert(&entry).unwrap();
        db.history_clear().unwrap();

        let statistics = db.download_statistics_list().unwrap();
        assert_eq!(statistics.len(), 1);
        assert_eq!(statistics[0].job_id, "stats-1");
        assert_eq!(statistics[0].average_speed_bps, 1_000);
        assert_eq!(statistics[0].server_stats[0].articles_failed, 1);
    }

    // -----------------------------------------------------------------------
    // RSS item operations
    // -----------------------------------------------------------------------

    #[test]
    fn test_rss_item_upsert_and_get() {
        let db = Database::open_memory().unwrap();
        let item = make_rss_item("rss-1", "feed-a", "Test Title");
        db.rss_item_upsert(&item).unwrap();

        let loaded = db.rss_item_get("rss-1").unwrap();
        assert!(loaded.is_some());
        let loaded = loaded.unwrap();
        assert_eq!(loaded.title, "Test Title");
        assert_eq!(loaded.feed_name, "feed-a");
        assert!(!loaded.downloaded);
    }

    #[test]
    fn test_rss_item_upsert_ignores_duplicate() {
        let db = Database::open_memory().unwrap();
        let item = make_rss_item("dup-1", "feed-a", "Original");
        db.rss_item_upsert(&item).unwrap();

        // Upsert again with different title — should be ignored (INSERT OR IGNORE)
        let item2 = make_rss_item("dup-1", "feed-a", "Updated");
        db.rss_item_upsert(&item2).unwrap();

        let loaded = db.rss_item_get("dup-1").unwrap().unwrap();
        assert_eq!(loaded.title, "Original"); // Not updated
    }

    #[test]
    fn test_rss_item_exists() {
        let db = Database::open_memory().unwrap();
        assert!(!db.rss_item_exists("rss-x").unwrap());

        db.rss_item_upsert(&make_rss_item("rss-x", "feed", "Title"))
            .unwrap();
        assert!(db.rss_item_exists("rss-x").unwrap());
    }

    #[test]
    fn test_rss_items_batch_upsert() {
        let db = Database::open_memory().unwrap();
        let items = vec![
            make_rss_item("b1", "feed-a", "Title 1"),
            make_rss_item("b2", "feed-a", "Title 2"),
            make_rss_item("b3", "feed-b", "Title 3"),
        ];

        let inserted = db.rss_items_batch_upsert(&items).unwrap();
        assert_eq!(inserted, 3);
        assert_eq!(db.rss_item_count().unwrap(), 3);

        // Batch again — duplicates ignored
        let inserted2 = db.rss_items_batch_upsert(&items).unwrap();
        assert_eq!(inserted2, 0);
        assert_eq!(db.rss_item_count().unwrap(), 3);
    }

    #[test]
    fn test_rss_items_list_all() {
        let db = Database::open_memory().unwrap();
        db.rss_item_upsert(&make_rss_item("la1", "feed-a", "A1"))
            .unwrap();
        db.rss_item_upsert(&make_rss_item("la2", "feed-b", "B1"))
            .unwrap();

        let items = db.rss_items_list(None, 100).unwrap();
        assert_eq!(items.len(), 2);
    }

    #[test]
    fn test_rss_items_list_filtered() {
        let db = Database::open_memory().unwrap();
        db.rss_item_upsert(&make_rss_item("lf1", "feed-a", "A Item"))
            .unwrap();
        db.rss_item_upsert(&make_rss_item("lf2", "feed-b", "B Item"))
            .unwrap();
        db.rss_item_upsert(&make_rss_item("lf3", "feed-a", "A Item 2"))
            .unwrap();

        let items = db.rss_items_list(Some("feed-a"), 100).unwrap();
        assert_eq!(items.len(), 2);
        for item in &items {
            assert_eq!(item.feed_name, "feed-a");
        }
    }

    #[test]
    fn test_rss_item_mark_downloaded() {
        let db = Database::open_memory().unwrap();
        db.rss_item_upsert(&make_rss_item("md1", "feed", "Title"))
            .unwrap();

        db.rss_item_mark_downloaded("md1", Some("movies")).unwrap();

        let loaded = db.rss_item_get("md1").unwrap().unwrap();
        assert!(loaded.downloaded);
        assert!(loaded.downloaded_at.is_some());
        assert_eq!(loaded.category.as_deref(), Some("movies"));
    }

    #[test]
    fn test_rss_item_count() {
        let db = Database::open_memory().unwrap();
        assert_eq!(db.rss_item_count().unwrap(), 0);

        db.rss_item_upsert(&make_rss_item("c1", "f", "T1")).unwrap();
        db.rss_item_upsert(&make_rss_item("c2", "f", "T2")).unwrap();
        assert_eq!(db.rss_item_count().unwrap(), 2);
    }

    #[test]
    fn test_rss_items_prune() {
        let db = Database::open_memory().unwrap();
        for i in 0..10 {
            db.rss_item_upsert(&make_rss_item(&format!("pr-{i}"), "f", &format!("T{i}")))
                .unwrap();
        }
        assert_eq!(db.rss_item_count().unwrap(), 10);

        let deleted = db.rss_items_prune(5).unwrap();
        assert_eq!(deleted, 5);
        assert_eq!(db.rss_item_count().unwrap(), 5);
    }

    // -----------------------------------------------------------------------
    // RSS rule operations
    // -----------------------------------------------------------------------

    #[test]
    fn test_rss_rule_insert_and_list() {
        let db = Database::open_memory().unwrap();
        let rule = RssRule {
            id: "rule-1".into(),
            name: "Movies".into(),
            feed_names: vec!["feed-a".into(), "feed-b".into()],
            category: Some("movies".into()),
            priority: 2,
            match_regex: ".*1080p.*".into(),
            enabled: true,
        };

        db.rss_rule_insert(&rule).unwrap();
        let rules = db.rss_rule_list().unwrap();
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].name, "Movies");
        assert_eq!(rules[0].feed_names, vec!["feed-a", "feed-b"]);
        assert_eq!(rules[0].match_regex, ".*1080p.*");
        assert!(rules[0].enabled);
    }

    #[test]
    fn test_rss_rule_update() {
        let db = Database::open_memory().unwrap();
        let rule = RssRule {
            id: "rule-u".into(),
            name: "Original".into(),
            feed_names: vec!["feed-a".into()],
            category: None,
            priority: 1,
            match_regex: ".*".into(),
            enabled: true,
        };
        db.rss_rule_insert(&rule).unwrap();

        let updated = RssRule {
            id: "rule-u".into(),
            name: "Updated".into(),
            feed_names: vec!["feed-a".into(), "feed-c".into()],
            category: Some("tv".into()),
            priority: 3,
            match_regex: ".*2160p.*".into(),
            enabled: false,
        };
        db.rss_rule_update(&updated).unwrap();

        let rules = db.rss_rule_list().unwrap();
        assert_eq!(rules[0].name, "Updated");
        assert_eq!(rules[0].feed_names, vec!["feed-a", "feed-c"]);
        assert_eq!(rules[0].category.as_deref(), Some("tv"));
        assert!(!rules[0].enabled);
    }

    #[test]
    fn test_rss_rule_delete() {
        let db = Database::open_memory().unwrap();
        let rule = RssRule {
            id: "rule-d".into(),
            name: "Delete Me".into(),
            feed_names: vec!["f".into()],
            category: None,
            priority: 1,
            match_regex: ".*".into(),
            enabled: true,
        };
        db.rss_rule_insert(&rule).unwrap();
        assert_eq!(db.rss_rule_list().unwrap().len(), 1);

        db.rss_rule_delete("rule-d").unwrap();
        assert_eq!(db.rss_rule_list().unwrap().len(), 0);
    }

    #[test]
    fn test_settings_get_set() {
        let db = Database::open_memory().unwrap();
        db.set_setting("theme", "dark");
        assert_eq!(db.get_setting("theme"), Some("dark".to_string()));
    }

    #[test]
    fn test_settings_upsert() {
        let db = Database::open_memory().unwrap();
        db.set_setting("speed", "100");
        db.set_setting("speed", "200");
        assert_eq!(db.get_setting("speed"), Some("200".to_string()));
    }

    #[test]
    fn test_settings_missing_key() {
        let db = Database::open_memory().unwrap();
        assert_eq!(db.get_setting("nonexistent"), None);
    }
}
