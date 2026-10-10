use std::path::Path;

use chrono::Utc;
use rusqlite::{Connection, OptionalExtension, params};
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

        if version < 9 {
            info!("Applying database migration v9: per-article retry outcomes");
            self.conn.execute_batch(
                "
                ALTER TABLE history ADD COLUMN retry_data BLOB;
                DELETE FROM schema_version;
                INSERT INTO schema_version (version) VALUES (9);
                ",
            )?;
        }

        if version < 10 {
            info!("Applying database migration v10: per-article damage ledger");
            // Created empty. `history.retry_data` (v9) is a retry checkpoint,
            // not damage evidence, so there is no backfill: it carries no
            // failure reason, per-server evidence, TTL, or server fingerprint
            // to reconstruct a confirmed-missing record from. See WI-143.
            self.conn.execute_batch(
                "
                CREATE TABLE IF NOT EXISTS damage_ledger (
                    scope_id TEXT NOT NULL,
                    file_index INTEGER NOT NULL,
                    segment_number INTEGER NOT NULL,
                    message_id TEXT NOT NULL,
                    state TEXT NOT NULL,
                    evidence TEXT,
                    first_refused_at TEXT,
                    confirmed_at TEXT,
                    expires_at TEXT,
                    server_fingerprint TEXT NOT NULL DEFAULT '',
                    PRIMARY KEY (scope_id, file_index, segment_number)
                );
                CREATE INDEX IF NOT EXISTS idx_damage_ledger_file
                    ON damage_ledger (scope_id, file_index);
                DELETE FROM schema_version;
                INSERT INTO schema_version (version) VALUES (10);
                ",
            )?;
        }

        if version < 11 {
            info!("Applying database migration v11: idempotent queue admissions");
            self.conn.execute_batch(
                "
                CREATE TABLE queue_admissions (
                    idempotency_key TEXT PRIMARY KEY,
                    payload_digest TEXT NOT NULL,
                    job_id TEXT NOT NULL UNIQUE,
                    accepted_at TEXT NOT NULL
                );

                CREATE INDEX idx_queue_admissions_job
                    ON queue_admissions(job_id);

                DELETE FROM schema_version;
                INSERT INTO schema_version (version) VALUES (11);
                ",
            )?;
        }

        if version < 12 {
            info!("Applying database migration v12: typed terminal failure codes");
            self.conn.execute_batch(
                "
                ALTER TABLE history ADD COLUMN failure_code TEXT;
                UPDATE history
                SET failure_code = 'download_failed'
                WHERE LOWER(status) = 'failed' AND failure_code IS NULL;

                DELETE FROM schema_version;
                INSERT INTO schema_version (version) VALUES (12);
                ",
            )?;
        }

        if version < 13 {
            info!("Applying database migration v13: per-job post-processing override");
            // Partial schemas (as built by migration tests) may lack the
            // queue table; only add the column where the table exists.
            let has_queue: i64 = self.conn.query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'queue'",
                [],
                |row| row.get(0),
            )?;
            if has_queue > 0 {
                self.conn
                    .execute_batch("ALTER TABLE queue ADD COLUMN pp_override INTEGER;")?;
            }
            self.conn.execute_batch(
                "
                DELETE FROM schema_version;
                INSERT INTO schema_version (version) VALUES (13);
                ",
            )?;
        }

        if version < 14 {
            info!("Applying database migration v14: history post-processing level");
            // Existing rows keep NULL: their level was never recorded.
            let has_history: i64 = self.conn.query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'history'",
                [],
                |row| row.get(0),
            )?;
            if has_history > 0 {
                self.conn
                    .execute_batch("ALTER TABLE history ADD COLUMN post_processing INTEGER;")?;
            }
            self.conn.execute_batch(
                "
                DELETE FROM schema_version;
                INSERT INTO schema_version (version) VALUES (14);
                ",
            )?;
        }

        if version < 15 {
            info!("Applying database migration v15: per-job archive deletion");
            // Partial schemas (as built by migration tests) may lack either
            // table; only add the column where the table exists.
            for table in ["queue", "history"] {
                let exists: i64 = self.conn.query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
                    [table],
                    |row| row.get(0),
                )?;
                if exists > 0 {
                    self.conn.execute_batch(&format!(
                        "ALTER TABLE {table} ADD COLUMN delete_archives INTEGER;"
                    ))?;
                }
            }
            self.conn.execute_batch(
                "
                DELETE FROM schema_version;
                INSERT INTO schema_version (version) VALUES (15);
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

    /// Atomically bind an idempotency key to one payload and queued job.
    ///
    /// The admission row intentionally has no foreign key to queue or history:
    /// it remains authoritative after a job moves or bounded history is pruned.
    pub fn queue_admit(
        &mut self,
        job: &NzbJob,
        nzb_data: &[u8],
        idempotency_key: &str,
        payload_digest: &str,
    ) -> Result<QueueAdmissionOutcome, NzbError> {
        let transaction = self.conn.transaction()?;
        let existing = transaction
            .query_row(
                "SELECT idempotency_key, payload_digest, job_id, accepted_at
                 FROM queue_admissions WHERE idempotency_key = ?1",
                [idempotency_key],
                |row| {
                    Ok(QueueAdmission {
                        idempotency_key: row.get(0)?,
                        payload_digest: row.get(1)?,
                        job_id: row.get(2)?,
                        accepted_at: parse_datetime(&row.get::<_, String>(3)?),
                    })
                },
            )
            .optional()?;

        if let Some(existing) = existing {
            if existing.payload_digest != payload_digest {
                return Err(NzbError::AdmissionConflict);
            }
            transaction.commit()?;
            return Ok(QueueAdmissionOutcome::Existing(existing));
        }

        transaction.execute(
            "INSERT INTO queue (id, name, category, status, priority, total_bytes,
             downloaded_bytes, file_count, files_completed, article_count,
             articles_downloaded, articles_failed, added_at, work_dir, output_dir, password,
             nzb_raw, pp_override, delete_archives)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15,
             ?16, ?17, ?18, ?19)",
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
                nzb_data,
                job.pp_override,
                job.delete_archives,
            ],
        )?;

        let admission = QueueAdmission {
            idempotency_key: idempotency_key.to_string(),
            payload_digest: payload_digest.to_string(),
            job_id: job.id.clone(),
            accepted_at: job.added_at,
        };
        transaction.execute(
            "INSERT INTO queue_admissions (
                idempotency_key, payload_digest, job_id, accepted_at
             ) VALUES (?1, ?2, ?3, ?4)",
            params![
                admission.idempotency_key,
                admission.payload_digest,
                admission.job_id,
                admission.accepted_at.to_rfc3339(),
            ],
        )?;
        transaction.commit()?;
        Ok(QueueAdmissionOutcome::Inserted(admission))
    }

    /// Resolve one admission without relying on bounded queue/history lists.
    pub fn queue_admission_observe(
        &self,
        idempotency_key: &str,
    ) -> Result<Option<QueueAdmissionObservation>, NzbError> {
        let admission = self
            .conn
            .query_row(
                "SELECT idempotency_key, payload_digest, job_id, accepted_at
                 FROM queue_admissions WHERE idempotency_key = ?1",
                [idempotency_key],
                |row| {
                    Ok(QueueAdmission {
                        idempotency_key: row.get(0)?,
                        payload_digest: row.get(1)?,
                        job_id: row.get(2)?,
                        accepted_at: parse_datetime(&row.get::<_, String>(3)?),
                    })
                },
            )
            .optional()?;
        let Some(admission) = admission else {
            return Ok(None);
        };

        // The durable terminal history row is authoritative over the lingering
        // queue view, so it is consulted first: a job that has completed or
        // failed carries its typed failure code here.
        let history_state = self
            .conn
            .query_row(
                "SELECT status, total_bytes, downloaded_bytes, completed_at, output_dir,
                 error_message, failure_code FROM history WHERE id = ?1",
                [&admission.job_id],
                |row| {
                    Ok(QueueAdmissionState::History {
                        status: parse_status(&row.get::<_, String>(0)?),
                        total_bytes: row.get::<_, i64>(1)? as u64,
                        downloaded_bytes: row.get::<_, i64>(2)? as u64,
                        completed_at: parse_datetime(&row.get::<_, String>(3)?),
                        output_dir: row.get::<_, String>(4)?.into(),
                        error_message: row.get(5)?,
                        failure_code: parse_failure_code(row.get(6)?)?,
                    })
                },
            )
            .optional()?;
        if let Some(state) = history_state {
            return Ok(Some(QueueAdmissionObservation { admission, state }));
        }

        let queued_state = self
            .conn
            .query_row(
                "SELECT status, total_bytes, downloaded_bytes, output_dir, error_message
                 FROM queue WHERE id = ?1",
                [&admission.job_id],
                |row| {
                    Ok(QueueAdmissionState::Queue {
                        status: parse_status(&row.get::<_, String>(0)?),
                        total_bytes: row.get::<_, i64>(1)? as u64,
                        downloaded_bytes: row.get::<_, i64>(2)? as u64,
                        output_dir: row.get::<_, String>(3)?.into(),
                        error_message: row.get(4)?,
                    })
                },
            )
            .optional()?;
        if let Some(state) = queued_state {
            return Ok(Some(QueueAdmissionObservation { admission, state }));
        }

        Ok(Some(QueueAdmissionObservation {
            admission,
            state: QueueAdmissionState::Unobserved,
        }))
    }

    /// Insert a new job into the queue.
    pub fn queue_insert(&self, job: &NzbJob) -> Result<(), NzbError> {
        self.conn.execute(
            "INSERT INTO queue (id, name, category, status, priority, total_bytes,
             downloaded_bytes, file_count, files_completed, article_count,
             articles_downloaded, articles_failed, added_at, work_dir, output_dir, password,
             pp_override, delete_archives)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16,
             ?17, ?18)",
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
                job.pp_override,
                job.delete_archives,
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

    /// Update the non-terminal, user-visible queue message for a job.
    pub fn queue_update_error_message(
        &self,
        id: &str,
        error_message: Option<&str>,
    ) -> Result<(), NzbError> {
        self.conn.execute(
            "UPDATE queue SET error_message=?2 WHERE id=?1",
            params![id, error_message],
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
             added_at, completed_at, work_dir, output_dir, password, error_message, pp_override,
             delete_archives
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
                    pp_override: row.get(18)?,
                    delete_archives: row.get(19)?,
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
        self.history_insert_with_carried_bytes(entry, 0)
    }

    /// Move a completed/failed job to history, excluding `carried_bytes`
    /// from its statistics row. A history retry starts from the failed
    /// attempt's checkpoint, whose bytes that attempt's own statistics row
    /// already counts; the history row keeps the job's cumulative total.
    pub fn history_insert_with_carried_bytes(
        &self,
        entry: &HistoryEntry,
        carried_bytes: u64,
    ) -> Result<(), NzbError> {
        // Invariant: a Failed row carries exactly one typed failure code, and a
        // non-Failed row carries none. This keeps the terminal history row a
        // trustworthy source of the failure reason.
        if (entry.status == JobStatus::Failed) != entry.failure_code.is_some() {
            return Err(NzbError::Other("terminal_failure_code_invalid".to_string()));
        }
        let stages_json = serde_json::to_string(&entry.stages).unwrap_or_default();
        let server_stats_json = serde_json::to_string(&entry.server_stats).unwrap_or_default();
        self.conn.execute(
            "INSERT INTO history (id, name, category, status, total_bytes, downloaded_bytes,
             added_at, completed_at, download_time_secs, output_dir, stages, error_message,
             nzb_data, server_stats, retry_data, failure_code, post_processing, delete_archives)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16,
             ?17, ?18)",
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
                entry.retry_data,
                entry.failure_code.map(|code| code.to_string()),
                entry.post_processing,
                entry.delete_archives,
            ],
        )?;

        let duration_secs = entry.download_time_secs.unwrap_or_else(|| {
            (entry.completed_at - entry.added_at)
                .num_milliseconds()
                .max(0) as f64
                / 1000.0
        });
        let downloaded_bytes = entry.downloaded_bytes.saturating_sub(carried_bytes);
        let average_speed_bps = if duration_secs > 0.0 {
            (downloaded_bytes as f64 / duration_secs) as u64
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
                downloaded_bytes as i64,
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
             CASE WHEN nzb_data IS NOT NULL THEN 1 ELSE 0 END as has_nzb, failure_code,
             post_processing, delete_archives
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
                    failure_code: parse_failure_code(row.get(14)?)?,
                    post_processing: row.get(15)?,
                    delete_archives: row.get(16)?,
                    server_stats,
                    // Don't load actual blob in list - just note if it exists
                    nzb_data: if has_nzb != 0 { Some(Vec::new()) } else { None },
                    retry_data: None,
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

    /// Get persisted per-article outcomes for a history retry.
    pub fn history_get_retry_data(&self, id: &str) -> Result<Option<Vec<u8>>, NzbError> {
        let result = self.conn.query_row(
            "SELECT retry_data FROM history WHERE id = ?1",
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
    ///
    /// A limit of `0` is treated as "keep all" and is a no-op: the
    /// `LIMIT 0` subquery would otherwise match nothing and delete every
    /// row (GH #136). Callers normalize zero away already; this is the last
    /// line of defence.
    pub fn history_enforce_retention(&self, max_entries: usize) -> Result<(), NzbError> {
        if max_entries == 0 {
            return Ok(());
        }
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
             added_at, completed_at, download_time_secs, output_dir, stages, error_message, server_stats,
             failure_code, post_processing, delete_archives
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
                failure_code: parse_failure_code(row.get(13)?)?,
                post_processing: row.get(14)?,
                delete_archives: row.get(15)?,
                server_stats,
                nzb_data: None,
                retry_data: None,
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

    /// Remove downloaded RSS items older than `cutoff` and return the count.
    pub fn rss_items_expire_downloaded(&self, cutoff: &str) -> Result<usize, NzbError> {
        Ok(self.conn.execute(
            "DELETE FROM rss_items WHERE downloaded = 1 AND downloaded_at IS NOT NULL
             AND downloaded_at < ?1",
            params![cutoff],
        )?)
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

fn parse_failure_code(value: Option<String>) -> rusqlite::Result<Option<JobFailureCode>> {
    value
        .map(|value| {
            value.parse().map_err(|_| {
                rusqlite::Error::FromSqlConversionFailure(
                    0,
                    rusqlite::types::Type::Text,
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "invalid terminal failure code",
                    )
                    .into(),
                )
            })
        })
        .transpose()
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
            pp_override: None,
            delete_archives: None,
            server_stats: Vec::new(),
            files: Vec::new(),
        }
    }

    #[test]
    fn queue_round_trips_post_processing_override() {
        let db = Database::open_memory().unwrap();
        let mut overridden = make_job("pp-job", "PP Job");
        overridden.pp_override = Some(1);
        db.queue_insert(&overridden).unwrap();
        db.queue_insert(&make_job("plain-job", "Plain Job"))
            .unwrap();

        let jobs = db.queue_list().unwrap();
        let pp = |id: &str| jobs.iter().find(|job| job.id == id).unwrap().pp_override;
        assert_eq!(pp("pp-job"), Some(1));
        assert_eq!(pp("plain-job"), None);
    }

    #[test]
    fn queue_and_history_round_trip_delete_archives() {
        let db = Database::open_memory().unwrap();
        let mut keep = make_job("keep-job", "Keep Job");
        keep.delete_archives = Some(false);
        db.queue_insert(&keep).unwrap();
        db.queue_insert(&make_job("plain-job", "Plain Job"))
            .unwrap();
        let jobs = db.queue_list().unwrap();
        let flag = |id: &str| {
            jobs.iter()
                .find(|job| job.id == id)
                .unwrap()
                .delete_archives
        };
        assert_eq!(flag("keep-job"), Some(false));
        assert_eq!(flag("plain-job"), None);

        let mut stored = make_history("keep-history", "Keep History");
        stored.delete_archives = Some(false);
        db.history_insert(&stored).unwrap();
        assert_eq!(
            db.history_get("keep-history")
                .unwrap()
                .unwrap()
                .delete_archives,
            Some(false)
        );
        let listed = db.history_list(10).unwrap();
        assert_eq!(listed[0].delete_archives, Some(false));
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
            failure_code: None,
            post_processing: None,
            delete_archives: None,
            server_stats: Vec::new(),
            nzb_data: None,
            retry_data: None,
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
    fn queue_admission_replays_exact_payload_and_rejects_conflict() {
        let mut db = Database::open_memory().unwrap();
        let job = make_job("admitted-job", "Admitted");
        let inserted = db
            .queue_admit(&job, b"payload", "request-key", "sha256:first")
            .unwrap();
        assert!(matches!(inserted, QueueAdmissionOutcome::Inserted(_)));

        let replay = db
            .queue_admit(
                &make_job("unused-job", "Replay"),
                b"payload",
                "request-key",
                "sha256:first",
            )
            .unwrap();
        let QueueAdmissionOutcome::Existing(replay) = replay else {
            panic!("exact replay should return the existing admission");
        };
        assert_eq!(replay.job_id, "admitted-job");
        assert_eq!(db.queue_list().unwrap().len(), 1);

        let conflict = db.queue_admit(
            &make_job("conflicting-job", "Conflict"),
            b"different",
            "request-key",
            "sha256:different",
        );
        assert!(matches!(conflict, Err(NzbError::AdmissionConflict)));
        assert_eq!(db.queue_list().unwrap().len(), 1);
    }

    #[test]
    fn queue_admission_survives_reopen_and_outlives_queue_observation() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("queue.db");
        {
            let mut db = Database::open(&path).unwrap();
            db.queue_admit(
                &make_job("durable-job", "Durable"),
                b"payload",
                "durable-key",
                "sha256:durable",
            )
            .unwrap();
        }

        let mut db = Database::open(&path).unwrap();
        let observation = db.queue_admission_observe("durable-key").unwrap().unwrap();
        assert_eq!(observation.admission.job_id, "durable-job");
        assert!(matches!(
            observation.state,
            QueueAdmissionState::Queue { .. }
        ));

        db.queue_remove("durable-job").unwrap();
        let observation = db.queue_admission_observe("durable-key").unwrap().unwrap();
        assert_eq!(observation.state, QueueAdmissionState::Unobserved);

        let replay = db
            .queue_admit(
                &make_job("new-job", "New"),
                b"payload",
                "durable-key",
                "sha256:durable",
            )
            .unwrap();
        let QueueAdmissionOutcome::Existing(replay) = replay else {
            panic!("durable replay should preserve the original job identity");
        };
        assert_eq!(replay.job_id, "durable-job");
        assert!(db.queue_list().unwrap().is_empty());
    }

    #[test]
    fn history_round_trips_post_processing_level() {
        let db = Database::open_memory().unwrap();
        let mut stored = make_history("pp-level", "PP Level");
        stored.post_processing = Some(1);
        db.history_insert(&stored).unwrap();
        db.history_insert(&make_history("pp-legacy", "Legacy"))
            .unwrap();

        assert_eq!(
            db.history_get("pp-level").unwrap().unwrap().post_processing,
            Some(1)
        );
        assert_eq!(
            db.history_get("pp-legacy")
                .unwrap()
                .unwrap()
                .post_processing,
            None
        );
        let listed = db.history_list(10).unwrap();
        let level = listed.iter().find(|entry| entry.id == "pp-level").unwrap();
        assert_eq!(level.post_processing, Some(1));
    }

    #[test]
    fn typed_failure_migration_backfills_only_failed_history() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("queue.db");
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch(
                "
                CREATE TABLE schema_version (version INTEGER NOT NULL);
                INSERT INTO schema_version (version) VALUES (11);
                CREATE TABLE history (
                    id TEXT PRIMARY KEY,
                    status TEXT NOT NULL
                );
                INSERT INTO history (id, status) VALUES
                    ('failed-job', 'Failed'),
                    ('completed-job', 'Completed');
                ",
            )
            .unwrap();
        drop(connection);

        let db = Database::open(&path).unwrap();
        let failed: Option<String> = db
            .conn
            .query_row(
                "SELECT failure_code FROM history WHERE id = 'failed-job'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let completed: Option<String> = db
            .conn
            .query_row(
                "SELECT failure_code FROM history WHERE id = 'completed-job'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(failed.as_deref(), Some("download_failed"));
        assert_eq!(completed, None);
    }

    #[test]
    fn queue_admission_prefers_typed_terminal_history_over_lingering_queue_view() {
        let mut db = Database::open_memory().unwrap();
        db.queue_admit(
            &make_job("terminal-job", "Terminal"),
            b"payload",
            "terminal-key",
            "sha256:terminal",
        )
        .unwrap();
        let mut history = make_history("terminal-job", "Terminal");
        history.status = JobStatus::Failed;
        history.failure_code = Some(JobFailureCode::ArchiveInvalid);
        history.error_message = Some("private extractor diagnostic".to_string());
        db.history_insert(&history).unwrap();

        let observation = db.queue_admission_observe("terminal-key").unwrap().unwrap();
        assert!(matches!(
            observation.state,
            QueueAdmissionState::History {
                status: JobStatus::Failed,
                failure_code: Some(JobFailureCode::ArchiveInvalid),
                ..
            }
        ));
    }

    #[test]
    fn terminal_history_requires_failure_code_exactly_on_failure() {
        let db = Database::open_memory().unwrap();
        let mut failed = make_history("failed-job", "Failed");
        failed.status = JobStatus::Failed;
        assert!(matches!(
            db.history_insert(&failed),
            Err(NzbError::Other(code)) if code == "terminal_failure_code_invalid"
        ));

        failed.failure_code = Some(JobFailureCode::DownloadFailed);
        db.history_insert(&failed).unwrap();

        let mut completed = make_history("completed-job", "Completed");
        completed.failure_code = Some(JobFailureCode::DownloadFailed);
        assert!(matches!(
            db.history_insert(&completed),
            Err(NzbError::Other(code)) if code == "terminal_failure_code_invalid"
        ));
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

    /// GH #136: a retention limit of zero must not wipe history.
    #[test]
    fn test_history_enforce_retention_zero_keeps_all() {
        let db = Database::open_memory().unwrap();
        for i in 0..3 {
            db.history_insert(&make_history(&format!("ret0-{i}"), &format!("Job {i}")))
                .unwrap();
        }

        db.history_enforce_retention(0).unwrap();
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
    fn test_download_statistics_exclude_carried_bytes() {
        let db = Database::open_memory().unwrap();
        let mut entry = make_history("retry-1", "Retried Job");
        entry.downloaded_bytes = 10_000;
        entry.download_time_secs = Some(2.0);

        db.history_insert_with_carried_bytes(&entry, 6_000).unwrap();

        // History keeps the job's cumulative progress; the statistics row
        // counts only what this attempt fetched.
        assert_eq!(db.history_list(1).unwrap()[0].downloaded_bytes, 10_000);
        let statistics = db.download_statistics_list().unwrap();
        assert_eq!(statistics[0].downloaded_bytes, 4_000);
        assert_eq!(statistics[0].average_speed_bps, 2_000);
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
