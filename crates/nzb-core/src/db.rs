use std::path::Path;

use chrono::Utc;
use rusqlite::{Connection, OptionalExtension, params};
use sha2::{Digest, Sha256};
use tracing::info;

use crate::error::NzbError;
use crate::models::*;

#[allow(dead_code)]
const SCHEMA_VERSION: u32 = 1;

/// The content digest that recognises a repeated NZB: `sha256:` followed by
/// the lowercase hex SHA-256 of the payload.
pub fn nzb_digest(payload: &[u8]) -> String {
    let hex: String = Sha256::digest(payload)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("sha256:{hex}")
}

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
        // Apply every pending migration in one transaction.
        //
        // Each migration used to commit separately and stamp `schema_version`
        // on its own. That cost one durable commit per migration — 31 fsyncs
        // to create a fresh database, measured — and on a contended disk (the
        // CI container shares a dind volume with concurrent builds) the serial
        // commits dominated start-up: at 600 ms per fsync the backend took over
        // 15 s to open its database, never reached the HTTP bind, and global
        // setup timed out without a single log line (the migration notices are
        // INFO, the e2e config logs at WARN).
        //
        // Batching leaves about ten fsyncs (SQLite's own commit and WAL
        // bookkeeping), and the same 600 ms shim now reaches health in 4.3 s
        // rather than failing, or 10.7 s at 1.5 s per fsync.
        //
        // Each migration still logs as it runs. The version row is written
        // once, after the last one, and the batch rolls back together: an
        // interrupted upgrade leaves the database at its previous version
        // rather than half-migrated.
        let tx = self.conn.unchecked_transaction()?;
        // Creating the bookkeeping table is part of the same batch: as its own
        // statement it would be a second durable commit before the first
        // migration even ran.
        tx.execute_batch(
            "CREATE TABLE IF NOT EXISTS schema_version (
                version INTEGER NOT NULL
            );",
        )?;
        let version: u32 = tx
            .query_row(
                "SELECT COALESCE(MAX(version), 0) FROM schema_version",
                [],
                |row| row.get(0),
            )
            .unwrap_or(0);
        let mut applied = version;

        if version < 1 {
            info!("Applying database migration v1");
            tx.execute_batch(
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
                ",
            )?;
            applied = 1;
        }

        if version < 2 {
            info!("Applying database migration v2");
            tx.execute_batch(
                "
                -- Add NZB data storage and server stats to history
                ALTER TABLE history ADD COLUMN nzb_data BLOB;
                ALTER TABLE history ADD COLUMN server_stats TEXT DEFAULT '[]';

                -- Add server stats to queue
                ALTER TABLE queue ADD COLUMN server_stats TEXT DEFAULT '[]';

                -- Add NZB data to queue for preservation
                ALTER TABLE queue ADD COLUMN nzb_raw BLOB;
                ",
            )?;
            applied = 2;
        }

        if version < 3 {
            info!("Applying database migration v3");
            tx.execute_batch(
                "
                -- Per-job log storage for history
                ALTER TABLE history ADD COLUMN job_logs TEXT DEFAULT '[]';
                ",
            )?;
            applied = 3;
        }

        if version < 4 {
            info!("Applying database migration v4");
            tx.execute_batch(
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
                ",
            )?;
            applied = 4;
        }

        if version < 5 {
            info!("Applying database migration v5: settings table");
            tx.execute_batch(
                "
                CREATE TABLE IF NOT EXISTS settings (
                    key TEXT PRIMARY KEY,
                    value TEXT NOT NULL
                );
                ",
            )?;
            applied = 5;
        }

        // Ensure settings table exists for databases that jumped to v5
        // with groups tables but without settings (pre-extraction rustnzbd)
        tx.execute_batch(
            "CREATE TABLE IF NOT EXISTS settings (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );",
        )?;

        #[cfg(feature = "groups-db")]
        if version < 6 {
            info!("Applying database migration v6: newsgroup browsing");
            tx.execute_batch(
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
                ",
            )?;
            applied = 6;
        }

        if version < 7 {
            info!("Applying database migration v7: persistent download statistics");
            tx.execute_batch(
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
                ",
            )?;
            applied = 7;
        }

        if version < 8 {
            info!("Applying database migration v8: active download duration");
            tx.execute_batch(
                "
                ALTER TABLE history ADD COLUMN download_time_secs REAL;
                ",
            )?;
            applied = 8;
        }

        if version < 9 {
            info!("Applying database migration v9: per-article retry outcomes");
            tx.execute_batch(
                "
                ALTER TABLE history ADD COLUMN retry_data BLOB;
                ",
            )?;
            applied = 9;
        }

        if version < 10 {
            info!("Applying database migration v10: per-article damage ledger");
            // Created empty. `history.retry_data` (v9) is a retry checkpoint,
            // not damage evidence, so there is no backfill: it carries no
            // failure reason, per-server evidence, TTL, or server fingerprint
            // to reconstruct a confirmed-missing record from. See WI-143.
            tx.execute_batch(
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
                ",
            )?;
            applied = 10;
        }

        if version < 11 {
            info!("Applying database migration v11: idempotent queue admissions");
            tx.execute_batch(
                "
                CREATE TABLE queue_admissions (
                    idempotency_key TEXT PRIMARY KEY,
                    payload_digest TEXT NOT NULL,
                    job_id TEXT NOT NULL UNIQUE,
                    accepted_at TEXT NOT NULL
                );

                CREATE INDEX idx_queue_admissions_job
                    ON queue_admissions(job_id);
                ",
            )?;
            applied = 11;
        }

        if version < 12 {
            info!("Applying database migration v12: typed terminal failure codes");
            tx.execute_batch(
                "
                ALTER TABLE history ADD COLUMN failure_code TEXT;
                UPDATE history
                SET failure_code = 'download_failed'
                WHERE LOWER(status) = 'failed' AND failure_code IS NULL;
                ",
            )?;
            applied = 12;
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
                tx.execute_batch("ALTER TABLE queue ADD COLUMN pp_override INTEGER;")?;
            }
            applied = 13;
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
                tx.execute_batch("ALTER TABLE history ADD COLUMN post_processing INTEGER;")?;
            }
            applied = 14;
        }

        if version < 15 {
            info!("Applying database migration v15: per-job archive deletion");
            // Partial schemas (as built by migration tests) may lack either
            // table; only add the column where the table exists.
            let mut table_exists_map = std::collections::HashMap::new();
            for table in ["queue", "history"] {
                let exists: i64 = self.conn.query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
                    [table],
                    |row| row.get(0),
                )?;
                table_exists_map.insert(table, exists > 0);
            }
            for table in ["queue", "history"] {
                if table_exists_map.get(&table).copied().unwrap_or(false) {
                    tx.execute_batch(&format!(
                        "ALTER TABLE {table} ADD COLUMN delete_archives INTEGER;"
                    ))?;
                }
            }
            applied = 15;
        }

        if version < 16 {
            info!("Applying database migration v16: header dedupe + per-group scan server");
            // Existing databases may have duplicate header rows when the same
            // article was fetched from more than one server. Deduplicate first
            // (keeping the oldest row so read state survives), then enforce a
            // unique index so it cannot happen again.
            let has_headers: i64 = self.conn.query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'headers'",
                [],
                |row| row.get(0),
            )?;
            let has_groups: i64 = self.conn.query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'groups'",
                [],
                |row| row.get(0),
            )?;
            let has_scan_server: i64 = if has_groups > 0 {
                self.conn.query_row(
                    "SELECT COUNT(*) FROM pragma_table_info('groups') WHERE name = 'scan_server_id'",
                    [],
                    |row| row.get(0),
                )?
            } else {
                0
            };
            if has_headers > 0 {
                tx.execute_batch(
                    "
                    UPDATE headers
                    SET read = 1
                    WHERE read = 0
                      AND id IN (
                          SELECT MIN(id)
                          FROM headers
                          GROUP BY group_id, message_id
                          HAVING MAX(read) = 1
                      );
                    DELETE FROM headers
                    WHERE id NOT IN (
                        SELECT MIN(id) FROM headers GROUP BY group_id, message_id
                    );
                    CREATE UNIQUE INDEX IF NOT EXISTS idx_headers_group_msgid
                        ON headers(group_id, message_id);
                    ",
                )?;
            }
            if has_groups > 0 && has_scan_server == 0 {
                tx.execute_batch("ALTER TABLE groups ADD COLUMN scan_server_id TEXT;")?;
            }
            applied = 16;
        }

        if version < 17 {
            info!("Applying database migration v17: NZB content digests for duplicate detection");
            // Partial schemas (as built by migration tests) may lack either
            // table, so only the tables that exist gain the column. Existing
            // rows are backfilled from their stored NZB so they can be matched.
            for (table, blob_column) in [("queue", "nzb_raw"), ("history", "nzb_data")] {
                let table_exists: i64 = tx.query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
                    [table],
                    |row| row.get(0),
                )?;
                if table_exists == 0 {
                    continue;
                }
                tx.execute_batch(&format!(
                    "ALTER TABLE {table} ADD COLUMN nzb_digest TEXT;
                     CREATE INDEX IF NOT EXISTS idx_{table}_nzb_digest ON {table}(nzb_digest);"
                ))?;
                let has_blob: i64 = tx.query_row(
                    "SELECT COUNT(*) FROM pragma_table_info(?1) WHERE name = ?2",
                    params![table, blob_column],
                    |row| row.get(0),
                )?;
                if has_blob == 0 {
                    continue;
                }
                let rows: Vec<(String, Vec<u8>)> = {
                    let mut stmt = tx.prepare(&format!(
                        "SELECT id, {blob_column} FROM {table} WHERE length({blob_column}) > 0"
                    ))?;
                    stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
                        .collect::<rusqlite::Result<_>>()?
                };
                for (id, data) in rows {
                    tx.execute(
                        &format!("UPDATE {table} SET nzb_digest = ?2 WHERE id = ?1"),
                        params![id, nzb_digest(&data)],
                    )?;
                }
            }
            applied = 17;
        }

        if version < 18 {
            info!("Applying database migration v18: history list columns ahead of the NZB blob");
            // Columns added to history by ALTER TABLE are stored after
            // nzb_data, so reading them walks the blob's overflow chain (about
            // 1.3 s for a 50-row page on a production database). List queries
            // read them from this narrow copy instead; triggers keep it in step.
            let has_history: i64 = self.conn.query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'history'",
                [],
                |row| row.get(0),
            )?;
            // Partial schemas (as built by migration tests) may lack the
            // source columns: they still get the table, so list queries work.
            let source_columns: i64 = self.conn.query_row(
                "SELECT COUNT(*) FROM pragma_table_info('history') WHERE name IN
                 ('download_time_secs', 'server_stats', 'failure_code',
                  'post_processing', 'delete_archives')",
                [],
                |row| row.get(0),
            )?;
            tx.execute_batch(
                "CREATE TABLE IF NOT EXISTS history_list_meta (
                    id TEXT PRIMARY KEY,
                    download_time_secs REAL,
                    server_stats TEXT,
                    failure_code TEXT,
                    post_processing INTEGER,
                    delete_archives INTEGER
                );",
            )?;
            if has_history > 0 && source_columns == 5 {
                tx.execute_batch(
                    "INSERT INTO history_list_meta
                        SELECT id, download_time_secs, server_stats, failure_code,
                               post_processing, delete_archives FROM history;
                    CREATE TRIGGER history_list_meta_insert AFTER INSERT ON history BEGIN
                        INSERT OR REPLACE INTO history_list_meta VALUES (
                            NEW.id, NEW.download_time_secs, NEW.server_stats,
                            NEW.failure_code, NEW.post_processing, NEW.delete_archives);
                    END;
                    CREATE TRIGGER history_list_meta_update AFTER UPDATE OF
                        id, download_time_secs, server_stats, failure_code,
                        post_processing, delete_archives ON history BEGIN
                        DELETE FROM history_list_meta WHERE id = OLD.id;
                        INSERT OR REPLACE INTO history_list_meta VALUES (
                            NEW.id, NEW.download_time_secs, NEW.server_stats,
                            NEW.failure_code, NEW.post_processing, NEW.delete_archives);
                    END;
                    CREATE TRIGGER history_list_meta_delete AFTER DELETE ON history BEGIN
                        DELETE FROM history_list_meta WHERE id = OLD.id;
                    END;
                    CREATE INDEX IF NOT EXISTS idx_history_category ON history(category);",
                )?;
            }
            applied = 18;
        }

        if version < 19 {
            info!("Applying database migration v19: PAR2 volumes fetched and available");
            // Stored in history and copied to history_list_meta, so list
            // queries read them without walking the NZB blob (v18).
            let has_history: i64 = self.conn.query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'history'",
                [],
                |row| row.get(0),
            )?;
            let has_triggers: i64 = self.conn.query_row(
                "SELECT COUNT(*) FROM sqlite_master
                 WHERE type = 'trigger' AND name LIKE 'history_list_meta_%'",
                [],
                |row| row.get(0),
            )?;
            tx.execute_batch(
                "ALTER TABLE history_list_meta ADD COLUMN par2_volumes_fetched INTEGER;
                 ALTER TABLE history_list_meta ADD COLUMN par2_volumes_available INTEGER;",
            )?;
            if has_history > 0 {
                tx.execute_batch(
                    "ALTER TABLE history ADD COLUMN par2_volumes_fetched INTEGER;
                     ALTER TABLE history ADD COLUMN par2_volumes_available INTEGER;",
                )?;
            }
            if has_triggers > 0 {
                tx.execute_batch(
                    "DROP TRIGGER history_list_meta_insert;
                    DROP TRIGGER history_list_meta_update;
                    CREATE TRIGGER history_list_meta_insert AFTER INSERT ON history BEGIN
                        INSERT OR REPLACE INTO history_list_meta VALUES (
                            NEW.id, NEW.download_time_secs, NEW.server_stats,
                            NEW.failure_code, NEW.post_processing, NEW.delete_archives,
                            NEW.par2_volumes_fetched, NEW.par2_volumes_available);
                    END;
                    CREATE TRIGGER history_list_meta_update AFTER UPDATE OF
                        id, download_time_secs, server_stats, failure_code,
                        post_processing, delete_archives, par2_volumes_fetched,
                        par2_volumes_available ON history BEGIN
                        DELETE FROM history_list_meta WHERE id = OLD.id;
                        INSERT OR REPLACE INTO history_list_meta VALUES (
                            NEW.id, NEW.download_time_secs, NEW.server_stats,
                            NEW.failure_code, NEW.post_processing, NEW.delete_archives,
                            NEW.par2_volumes_fetched, NEW.par2_volumes_available);
                    END;",
                )?;
            }
            applied = 19;
        }

        if version < 20 {
            info!("Applying database migration v20: restorable history deletions");
            // A history delete the UI offers to undo marks the row instead of
            // dropping it: `trash = 1` hides it from every history read, and
            // `trash_deadline` says when it stops being restorable. The row
            // keeps its NZB and retry blobs, so an undo is lossless; a purge
            // drops the row and its ledger entries exactly like a hard delete.
            let has_history: bool = self
                .conn
                .query_row(
                    "SELECT COUNT(*) > 0 FROM sqlite_master WHERE type = 'table' AND name = 'history'",
                    [],
                    |row| row.get(0),
                )
                .unwrap_or(false);
            if has_history {
                add_column_if_missing(&tx, "history", "trash", "INTEGER NOT NULL DEFAULT 0")?;
                add_column_if_missing(&tx, "history", "trash_deadline", "TEXT")?;
                // `trash` is added after `nzb_data`, so reading it from
                // `history` walks the blob's overflow chain (v18's reason for
                // the narrow copy). List queries filter the copy instead.
                let has_meta: bool = tx
                    .query_row(
                        "SELECT COUNT(*) > 0 FROM sqlite_master
                         WHERE type = 'table' AND name = 'history_list_meta'",
                        [],
                        |row| row.get(0),
                    )
                    .unwrap_or(false);
                if has_meta {
                    add_column_if_missing(&tx, "history_list_meta", "trash", "INTEGER")?;
                    tx.execute_batch(
                        "DROP TRIGGER IF EXISTS history_list_meta_insert;
                        DROP TRIGGER IF EXISTS history_list_meta_update;
                        DROP TRIGGER IF EXISTS history_list_meta_delete;
                        CREATE TRIGGER history_list_meta_insert AFTER INSERT ON history BEGIN
                            INSERT OR REPLACE INTO history_list_meta (
                                id, download_time_secs, server_stats, failure_code,
                                post_processing, delete_archives, par2_volumes_fetched,
                                par2_volumes_available, trash)
                            VALUES (
                                NEW.id, NEW.download_time_secs, NEW.server_stats,
                                NEW.failure_code, NEW.post_processing, NEW.delete_archives,
                                NEW.par2_volumes_fetched, NEW.par2_volumes_available,
                                NEW.trash);
                        END;
                        CREATE TRIGGER history_list_meta_update AFTER UPDATE ON history BEGIN
                            DELETE FROM history_list_meta WHERE id = OLD.id;
                            INSERT OR REPLACE INTO history_list_meta (
                                id, download_time_secs, server_stats, failure_code,
                                post_processing, delete_archives, par2_volumes_fetched,
                                par2_volumes_available, trash)
                            VALUES (
                                NEW.id, NEW.download_time_secs, NEW.server_stats,
                                NEW.failure_code, NEW.post_processing, NEW.delete_archives,
                                NEW.par2_volumes_fetched, NEW.par2_volumes_available,
                                NEW.trash);
                        END;
                        CREATE TRIGGER history_list_meta_delete AFTER DELETE ON history BEGIN
                            DELETE FROM history_list_meta WHERE id = OLD.id;
                        END;
                        UPDATE history_list_meta SET trash = COALESCE(
                            (SELECT trash FROM history h WHERE h.id = history_list_meta.id), 0);",
                    )?;
                }
            }
            applied = 20;
        }

        if version < 21 {
            info!("Applying database migration v21: covering indexes for history aggregates");
            // The history panel's statistics (completed/failed counts, bytes,
            // duration and failure reasons) used to load every summary row
            // just to fold it in Rust, which meant a cold read of the whole
            // table. `history_window_stats` and `history_failure_messages`
            // aggregate in SQLite instead, and these indexes let them, the
            // category list and the paged count run without opening the table
            // or walking any row's stored NZB blob.
            //
            // `idx_h_stats_cover` also serves the page and its count; the
            // partial `idx_h_fail_cover` carries only the failed rows' error
            // text. Its predicate must spell `COLLATE NOCASE`, because the
            // query compares status case-insensitively and SQLite only uses a
            // partial index whose term matches the query's collation.
            //
            // `idx_meta_trash` matters most: the anti-join that hides trashed
            // rows probes `history_list_meta(trash, id)` on every stats,
            // category and count query, and without it each probe walks the
            // meta table.
            //
            // Each index is created only when its table and columns exist:
            // migration tests build partial schemas to exercise older steps,
            // and those must still reach the newest schema version.
            create_index_if_columns(
                &tx,
                "history",
                &["completed_at", "status", "total_bytes", "added_at", "id"],
                "CREATE INDEX IF NOT EXISTS idx_h_stats_cover
                    ON history(completed_at DESC, status, total_bytes, added_at, id);",
            )?;
            create_index_if_columns(
                &tx,
                "history",
                &["category", "id"],
                "CREATE INDEX IF NOT EXISTS idx_h_cat_cover ON history(category, id);",
            )?;
            create_index_if_columns(
                &tx,
                "history_list_meta",
                &["trash", "id"],
                "CREATE INDEX IF NOT EXISTS idx_meta_trash ON history_list_meta(trash, id);",
            )?;
            create_index_if_columns(
                &tx,
                "history",
                &["completed_at", "error_message", "id", "status"],
                "CREATE INDEX IF NOT EXISTS idx_h_fail_cover
                    ON history(completed_at DESC, error_message, id)
                    WHERE status = 'Failed' COLLATE NOCASE;",
            )?;
            applied = 21;
        }

        if version < 22 {
            info!("Applying database migration v22: indexed duplicate lookup by name");
            // The duplicate check on every add matches names ignoring case.
            // Without an index on LOWER(name) it scanned the whole history
            // table, whose rows carry their NZB blob inline: on a 2.3 GB
            // database that took 18 s under the global DB lock and froze the
            // GUI. These expression indexes (with the v17 digest indexes) let
            // each branch of the lookup seek instead.
            for table in ["queue", "history"] {
                create_index_if_columns(
                    &tx,
                    table,
                    &["name"],
                    &format!(
                        "CREATE INDEX IF NOT EXISTS idx_{table}_name_lower ON {table}(LOWER(name));"
                    ),
                )?;
            }
            applied = 22;
        }

        if version < 23 {
            info!("Applying database migration v23: history rows kept after an *arr delete");
            // An *arr "Remove Completed" deletes the job's files through the
            // SABnzbd API but leaves rustnzb's own History row in place. The
            // marker lives in its own narrow table, so list reads join it
            // without touching columns stored after the NZB blob, and the
            // SABnzbd history the *arrs poll can skip the marked rows.
            tx.execute_batch(
                "CREATE TABLE IF NOT EXISTS history_client_removals (
                    id TEXT PRIMARY KEY,
                    removed_by TEXT NOT NULL,
                    removed_at TEXT NOT NULL,
                    files_deleted INTEGER NOT NULL DEFAULT 0
                );",
            )?;
            let has_history: bool = tx
                .query_row(
                    "SELECT COUNT(*) > 0 FROM sqlite_master WHERE type = 'table' AND name = 'history'",
                    [],
                    |row| row.get(0),
                )
                .unwrap_or(false);
            if has_history {
                tx.execute_batch(
                    "CREATE TRIGGER IF NOT EXISTS history_client_removals_delete
                        AFTER DELETE ON history BEGIN
                        DELETE FROM history_client_removals WHERE id = OLD.id;
                    END;",
                )?;
            }
            applied = 23;
        }

        if applied > version {
            stamp_schema_version(&tx, applied)?;
            tx.commit()?;
            info!(version = applied, "Database schema up to date");
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
             nzb_raw, pp_override, delete_archives, nzb_digest)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15,
             ?16, ?17, ?18, ?19, ?20)",
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
                nzb_digest(nzb_data),
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

    /// Update a job's progress counters without touching its status, for the
    /// periodic progress save: status is written separately, from the job
    /// list's current value, so a stale snapshot never overwrites a pause or
    /// resume.
    pub fn queue_update_counters(
        &self,
        id: &str,
        downloaded_bytes: u64,
        articles_downloaded: usize,
        articles_failed: usize,
        files_completed: usize,
    ) -> Result<(), NzbError> {
        self.conn.execute(
            "UPDATE queue SET downloaded_bytes=?2, articles_downloaded=?3,
             articles_failed=?4, files_completed=?5 WHERE id=?1",
            params![
                id,
                downloaded_bytes as i64,
                articles_downloaded as i64,
                articles_failed as i64,
                files_completed as i64,
            ],
        )?;
        Ok(())
    }

    /// Update only a queued job's status.
    pub fn queue_update_status(&self, id: &str, status: JobStatus) -> Result<(), NzbError> {
        self.conn.execute(
            "UPDATE queue SET status=?2 WHERE id=?1",
            params![id, status.to_string()],
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
                    slice_map: None,
                    held_par2_bytes: 0,
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

    /// Find queued and history jobs that an incoming NZB matches by name
    /// (ignoring ASCII case) or, when `digest` is given, by content digest.
    /// Only a history job that completed successfully matches; failed and
    /// deleted (trashed) history jobs never do. An arr re-grabbing a release
    /// whose earlier attempt failed or was removed is therefore admitted as a
    /// new job, and the old Failed history row is kept next to it (it is not
    /// replaced). Queued jobs always match.
    pub fn find_duplicate_candidates(
        &self,
        name: &str,
        digest: Option<&str>,
    ) -> Result<Vec<DuplicateMatch>, NzbError> {
        let mut stmt = self.conn.prepare(DUPLICATE_CANDIDATES_SQL)?;
        let matches = stmt
            .query_map(params![name, digest], |row| {
                let location = match row.get::<_, String>(4)?.as_str() {
                    "queue" => DuplicateLocation::Queue,
                    _ => DuplicateLocation::History,
                };
                Ok(DuplicateMatch {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    by_name: row.get::<_, i64>(2)? != 0,
                    by_digest: row.get::<_, i64>(3)? != 0,
                    location,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(matches)
    }

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
        // The history row and its statistics-ledger row commit together, so a
        // failed ledger write cannot leave a history entry the statistics
        // never counted.
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "INSERT INTO history (id, name, category, status, total_bytes, downloaded_bytes,
             added_at, completed_at, download_time_secs, output_dir, stages, error_message,
             nzb_data, server_stats, retry_data, failure_code, post_processing, delete_archives,
             nzb_digest, par2_volumes_fetched, par2_volumes_available)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16,
             ?17, ?18, ?19, ?20, ?21)",
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
                entry.nzb_data.as_deref().map(nzb_digest),
                entry.par2_volumes_fetched,
                entry.par2_volumes_available,
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
        tx.execute(
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
        tx.commit()?;
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
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {HISTORY_LIST_COLUMNS} FROM {HISTORY_LIST_FROM}
             WHERE COALESCE(m.trash, 0) = 0 ORDER BY h.completed_at DESC LIMIT ?1"
        ))?;
        let entries = stmt
            .query_map(params![limit as i64], history_list_row)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(entries)
    }

    /// One page of history matching `filter`, most recent first, plus the
    /// total number of matching rows. Filtering, ordering and paging all run
    /// in SQL, and the stored NZB and retry blobs are never read.
    pub fn history_page(
        &self,
        filter: &HistoryFilter,
        offset: usize,
        limit: usize,
    ) -> Result<(Vec<HistoryEntry>, usize), NzbError> {
        let (where_sql, args) = filter.to_sql();
        // Count with an anti-join, not the page's `LEFT JOIN`: the page still
        // needs the meta columns, but the count only needs to know that no
        // trashed row matched, and the anti-join lets it run entirely on the
        // covering indexes.
        let total: i64 = self.conn.query_row(
            &format!(
                "SELECT COUNT(*) FROM history h{}",
                with_condition(
                    &where_sql,
                    "h.id NOT IN (SELECT id FROM history_list_meta WHERE trash = 1)"
                )
            ),
            rusqlite::params_from_iter(args.iter()),
            |row| row.get(0),
        )?;
        let mut stmt = self
            .conn
            .prepare(&history_page_sql(&where_sql, args.len()))?;
        let mut page_args = args;
        page_args.push(Box::new(limit.min(i64::MAX as usize) as i64));
        page_args.push(Box::new(offset.min(i64::MAX as usize) as i64));
        let entries = stmt
            .query_map(
                rusqlite::params_from_iter(page_args.iter()),
                history_list_row,
            )?
            .collect::<Result<Vec<_>, _>>()?;
        Ok((entries, total as usize))
    }

    /// Distinct non-empty history categories, sorted.
    ///
    /// Trashed rows are excluded with an anti-join rather than the list
    /// query's `LEFT JOIN` + `COALESCE(m.trash, 0) = 0`: the subquery only
    /// ever yields rows whose `trash` is 1, so its ids are never NULL and
    /// `NOT IN` and the `COALESCE` test select the same rows. Either way the
    /// plan can read `idx_h_cat_cover` and probe `idx_meta_trash` without
    /// opening the `history` table.
    pub fn history_categories(&self) -> Result<Vec<String>, NzbError> {
        let mut stmt = self.conn.prepare(&history_categories_sql())?;
        let categories = stmt
            .query_map([], |row| row.get(0))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(categories)
    }

    /// Lightweight view of every history row matching `filter`, most recent
    /// first. Only the columns stored ahead of the NZB blob are read, so this
    /// stays cheap however large the stored NZBs are.
    pub fn history_summaries(
        &self,
        filter: &HistoryFilter,
    ) -> Result<Vec<HistorySummary>, NzbError> {
        let (where_sql, args) = filter.to_sql();
        let mut stmt = self.conn.prepare(&format!(
            "SELECT h.id, h.name, h.category, h.status, h.total_bytes, h.downloaded_bytes,
             h.added_at, h.completed_at, h.output_dir, h.error_message
             FROM {HISTORY_LIST_FROM}{} ORDER BY h.completed_at DESC",
            with_condition(&where_sql, "COALESCE(m.trash, 0) = 0")
        ))?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(args.iter()), |row| {
                Ok(HistorySummary {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    category: row.get(2)?,
                    status: parse_status(&row.get::<_, String>(3)?),
                    total_bytes: row.get::<_, i64>(4)? as u64,
                    downloaded_bytes: row.get::<_, i64>(5)? as u64,
                    added_at: parse_datetime(&row.get::<_, String>(6)?),
                    completed_at: parse_datetime(&row.get::<_, String>(7)?),
                    output_dir: row.get::<_, String>(8)?.into(),
                    error_message: row.get(9)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Completed/failed counts, completed bytes and completed duration over
    /// every history row matching `filter`.
    ///
    /// The history panel's statistics used to be folded in Rust from
    /// [`Self::history_summaries`], which reads a summary row (including its
    /// error text) for every entry in the window. The same fold runs here as
    /// one aggregate over `idx_h_stats_cover`, so a cold panel load touches a
    /// narrow index instead of the table.
    pub fn history_window_stats(
        &self,
        filter: &HistoryFilter,
    ) -> Result<HistoryWindowStats, NzbError> {
        let (where_sql, args) = filter.to_sql();
        let sql = history_window_stats_sql(&where_sql);
        let stats = self
            .conn
            .query_row(&sql, rusqlite::params_from_iter(args.iter()), |row| {
                Ok(HistoryWindowStats {
                    completed: row.get::<_, i64>(0)? as u64,
                    completed_bytes: row.get::<_, i64>(1)? as u64,
                    completed_duration_ms: row.get(2)?,
                    failed: row.get::<_, i64>(3)? as u64,
                })
            })?;
        Ok(stats)
    }

    /// Error text of every failed row matching `filter`, most recent first.
    ///
    /// Only the failure reasons need the text, so this reads the partial
    /// `idx_h_fail_cover` index instead of the summary columns. The index is
    /// named explicitly because the planner would otherwise prefer the
    /// completed-at index when a days window narrows the scan, and then walk
    /// the table for each row's error message.
    pub fn history_failure_messages(
        &self,
        filter: &HistoryFilter,
    ) -> Result<Vec<Option<String>>, NzbError> {
        let (where_sql, args) = filter.to_sql();
        let sql = history_failure_messages_sql(&where_sql);
        let mut stmt = self.conn.prepare(&sql)?;
        let messages = stmt
            .query_map(rusqlite::params_from_iter(args.iter()), |row| row.get(0))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(messages)
    }

    /// List-shaped history entries for `ids`, in the order given. Unknown ids
    /// are skipped. Like [`Database::history_list`], blobs are not loaded.
    pub fn history_get_listed(&self, ids: &[String]) -> Result<Vec<HistoryEntry>, NzbError> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {HISTORY_LIST_COLUMNS} FROM {HISTORY_LIST_FROM}
             WHERE h.id = ?1 AND COALESCE(m.trash, 0) = 0"
        ))?;
        let mut entries = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(entry) = stmt.query_row(params![id], history_list_row).optional()? {
                entries.push(entry);
            }
        }
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

    /// Enforce history retention limit by deleting oldest entries, and the
    /// ledger rows of jobs now gone.
    ///
    /// A limit of `0` is treated as "keep all" and is a no-op: the
    /// `LIMIT 0` subquery would otherwise match nothing and delete every
    /// row (GH #136). Callers normalize zero away already; this is the last
    /// line of defence.
    ///
    /// Trashed entries are not part of the limit: they are already gone from
    /// the user's view, and retention must not silently consume an undo
    /// window that the purge deadline owns.
    pub fn history_enforce_retention(&self, max_entries: usize) -> Result<(), NzbError> {
        if max_entries == 0 {
            return Ok(());
        }
        self.conn.execute(
            "DELETE FROM history WHERE trash = 0 AND id NOT IN (
                SELECT id FROM history WHERE trash = 0
                ORDER BY completed_at DESC LIMIT ?1
            )",
            params![max_entries as i64],
        )?;
        self.ledger_drop_orphans()?;
        Ok(())
    }

    /// Move history rows into the undo window instead of deleting them.
    ///
    /// Each row keeps its NZB and retry blobs, so [`Self::history_restore`] is
    /// lossless. Returns the summaries of the rows actually trashed, in the
    /// order given; an id that is unknown or already trashed is skipped, since
    /// the caller reports that as a per-id failure.
    pub fn history_trash(
        &self,
        ids: &[String],
        deadline: chrono::DateTime<Utc>,
    ) -> Result<Vec<HistorySummary>, NzbError> {
        let mut trashed = Vec::with_capacity(ids.len());
        let tx = self.conn.unchecked_transaction()?;
        for id in ids {
            let changed = tx.execute(
                "UPDATE history SET trash = 1, trash_deadline = ?2
                 WHERE id = ?1 AND trash = 0",
                params![id, deadline.to_rfc3339()],
            )?;
            if changed == 0 {
                continue;
            }
            if let Some(summary) = history_summary_by_id(&tx, id)? {
                trashed.push(summary);
            }
        }
        tx.commit()?;
        Ok(trashed)
    }

    /// Bring trashed rows back before their deadline. Returns the ids that
    /// were restored, in the order given; an unknown id, or one whose window
    /// has closed, is skipped.
    pub fn history_restore(
        &self,
        ids: &[String],
        now: chrono::DateTime<Utc>,
    ) -> Result<Vec<String>, NzbError> {
        let mut restored = Vec::with_capacity(ids.len());
        let tx = self.conn.unchecked_transaction()?;
        for id in ids {
            let changed = tx.execute(
                "UPDATE history SET trash = 0, trash_deadline = NULL
                 WHERE id = ?1 AND trash = 1
                   AND (trash_deadline IS NULL OR trash_deadline >= ?2)",
                params![id, now.to_rfc3339()],
            )?;
            if changed > 0 {
                restored.push(id.clone());
            }
        }
        tx.commit()?;
        Ok(restored)
    }

    /// Delete every trashed row whose deadline has passed, and the ledger rows
    /// of those jobs. Returns the summaries that were purged so the caller can
    /// reclaim their retained work directories.
    pub fn history_purge_expired(
        &self,
        now: chrono::DateTime<Utc>,
    ) -> Result<Vec<HistorySummary>, NzbError> {
        let tx = self.conn.unchecked_transaction()?;
        let mut purged = Vec::new();
        {
            let mut stmt = tx.prepare(
                "SELECT id, name, category, status, total_bytes, downloaded_bytes, added_at,
                 completed_at, output_dir, error_message
                 FROM history
                 WHERE trash = 1 AND (trash_deadline IS NULL OR trash_deadline < ?1)",
            )?;
            let rows = stmt.query_map(params![now.to_rfc3339()], |row| {
                Ok(HistorySummary {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    category: row.get(2)?,
                    status: parse_status(&row.get::<_, String>(3)?),
                    total_bytes: row.get::<_, i64>(4)? as u64,
                    downloaded_bytes: row.get::<_, i64>(5)? as u64,
                    added_at: parse_datetime(&row.get::<_, String>(6)?),
                    completed_at: parse_datetime(&row.get::<_, String>(7)?),
                    output_dir: row.get::<_, String>(8)?.into(),
                    error_message: row.get(9)?,
                })
            })?;
            for row in rows {
                purged.push(row?);
            }
        }
        tx.execute(
            "DELETE FROM history
             WHERE trash = 1 AND (trash_deadline IS NULL OR trash_deadline < ?1)",
            params![now.to_rfc3339()],
        )?;
        tx.commit()?;
        self.ledger_drop_orphans()?;
        Ok(purged)
    }

    /// Every trashed row, regardless of deadline. Used to keep a trashed
    /// row's retained work directories out of the orphan sweep while its
    /// undo window is open.
    pub fn history_trashed_summaries(&self) -> Result<Vec<HistorySummary>, NzbError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, name, category, status, total_bytes, downloaded_bytes, added_at,
             completed_at, output_dir, error_message
             FROM history WHERE trash = 1",
        )?;
        let rows = stmt
            .query_map([], |row| {
                Ok(HistorySummary {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    category: row.get(2)?,
                    status: parse_status(&row.get::<_, String>(3)?),
                    total_bytes: row.get::<_, i64>(4)? as u64,
                    downloaded_bytes: row.get::<_, i64>(5)? as u64,
                    added_at: parse_datetime(&row.get::<_, String>(6)?),
                    completed_at: parse_datetime(&row.get::<_, String>(7)?),
                    output_dir: row.get::<_, String>(8)?.into(),
                    error_message: row.get(9)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Ids of every trashed row, regardless of deadline. Used to keep their
    /// retained work directories out of the orphan sweep while an undo is
    /// still possible.
    pub fn history_trashed_ids(&self) -> Result<Vec<String>, NzbError> {
        let mut stmt = self
            .conn
            .prepare("SELECT id FROM history WHERE trash = 1")?;
        let ids = stmt
            .query_map([], |row| row.get(0))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(ids)
    }

    /// Get a single history entry by ID.
    pub fn history_get(&self, id: &str) -> Result<Option<HistoryEntry>, NzbError> {
        let mut stmt = self.conn.prepare(
            "SELECT h.id, h.name, h.category, h.status, h.total_bytes, h.downloaded_bytes,
             h.added_at, h.completed_at, m.download_time_secs, h.output_dir, h.stages,
             h.error_message, m.server_stats, m.failure_code, m.post_processing, m.delete_archives,
             m.par2_volumes_fetched, m.par2_volumes_available,
             r.removed_by, r.removed_at, r.files_deleted
             FROM history h LEFT JOIN history_list_meta m ON m.id = h.id
             LEFT JOIN history_client_removals r ON r.id = h.id
             WHERE h.id = ?1 AND COALESCE(m.trash, 0) = 0",
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
                par2_volumes_fetched: row.get(16)?,
                par2_volumes_available: row.get(17)?,
                client_removal: client_removal_at(row, 18)?,
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
            "UPDATE queue SET nzb_raw = ?2, nzb_digest = ?3 WHERE id = ?1",
            params![id, nzb_data, nzb_digest(nzb_data)],
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
        let count: i64 = self.conn.query_row(
            &format!("SELECT COUNT(*) FROM {HISTORY_LIST_FROM} WHERE COALESCE(m.trash, 0) = 0"),
            [],
            |row| row.get(0),
        )?;
        Ok(count as usize)
    }

    /// Mark history rows as removed by an external client (an *arr through
    /// the SABnzbd API) instead of deleting them. Returns how many rows were
    /// marked; an unknown id is skipped.
    pub fn history_mark_client_removed(
        &self,
        ids: &[String],
        removed_by: &str,
        removed_at: chrono::DateTime<Utc>,
        files_deleted: bool,
    ) -> Result<usize, NzbError> {
        let tx = self.conn.unchecked_transaction()?;
        let mut marked = 0;
        for id in ids {
            marked += tx.execute(
                "INSERT OR REPLACE INTO history_client_removals
                    (id, removed_by, removed_at, files_deleted)
                 SELECT id, ?2, ?3, ?4 FROM history WHERE id = ?1",
                params![id, removed_by, removed_at.to_rfc3339(), files_deleted],
            )?;
        }
        tx.commit()?;
        Ok(marked)
    }

    /// Ids of every history row an external client has removed.
    pub fn history_client_removed_ids(
        &self,
    ) -> Result<std::collections::HashSet<String>, NzbError> {
        let mut stmt = self
            .conn
            .prepare("SELECT id FROM history_client_removals")?;
        let ids = stmt
            .query_map([], |row| row.get(0))?
            .collect::<Result<_, _>>()?;
        Ok(ids)
    }

    /// Remove a history entry, and the ledger rows of a job now gone.
    pub fn history_remove(&self, id: &str) -> Result<(), NzbError> {
        self.conn
            .execute("DELETE FROM history WHERE id=?1", params![id])?;
        self.ledger_drop_orphans()?;
        Ok(())
    }

    /// Clear all history, and the ledger rows of jobs now gone.
    pub fn history_clear(&self) -> Result<(), NzbError> {
        self.conn.execute("DELETE FROM history", [])?;
        self.ledger_drop_orphans()?;
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

/// Columns of a history list row, selected `FROM` [`HISTORY_LIST_FROM`].
/// `has_nzb` uses `typeof()`, which SQLite answers from the record header
/// without loading the blob itself. Columns stored after `nzb_data` in
/// `history` come from `history_list_meta`, so no blob overflow chain is
/// walked for a list row.
const HISTORY_LIST_COLUMNS: &str = "h.id, h.name, h.category, h.status, h.total_bytes,
     h.downloaded_bytes, h.added_at, h.completed_at, m.download_time_secs, h.output_dir, h.stages,
     h.error_message, m.server_stats, typeof(h.nzb_data) <> 'null' AS has_nzb, m.failure_code,
     m.post_processing, m.delete_archives, m.par2_volumes_fetched, m.par2_volumes_available,
     r.removed_by, r.removed_at, r.files_deleted";

/// Duplicate lookup for `find_duplicate_candidates`. Name and digest
/// matches are separate SELECTs joined by UNION (which also removes a row
/// matched both ways) so each one seeks its own index —
/// `idx_*_name_lower` or `idx_*_nzb_digest` — rather than SQLite falling
/// back to a full scan for the OR. Only matching rows are ever read.
const DUPLICATE_CANDIDATES_SQL: &str = "
    SELECT id, name, LOWER(name) = LOWER(?1), COALESCE(nzb_digest = ?2, 0), 'queue'
    FROM queue WHERE LOWER(name) = LOWER(?1)
    UNION
    SELECT id, name, LOWER(name) = LOWER(?1), COALESCE(nzb_digest = ?2, 0), 'queue'
    FROM queue WHERE nzb_digest = ?2
    UNION
    SELECT id, name, LOWER(name) = LOWER(?1), COALESCE(nzb_digest = ?2, 0), 'history'
    FROM history WHERE LOWER(name) = LOWER(?1) AND trash = 0
        AND status = 'Completed' COLLATE NOCASE
    UNION
    SELECT id, name, LOWER(name) = LOWER(?1), COALESCE(nzb_digest = ?2, 0), 'history'
    FROM history WHERE nzb_digest = ?2 AND trash = 0
        AND status = 'Completed' COLLATE NOCASE";

/// Source of [`HISTORY_LIST_COLUMNS`].
const HISTORY_LIST_FROM: &str = "history h LEFT JOIN history_list_meta m ON m.id = h.id
     LEFT JOIN history_client_removals r ON r.id = h.id";

/// Append `condition` to a filter's ` WHERE ...` clause (or start one).
fn with_condition(where_sql: &str, condition: &str) -> String {
    match where_sql.strip_prefix(" WHERE ") {
        Some(rest) => format!(" WHERE {condition} AND {rest}"),
        None => format!(" WHERE {condition}"),
    }
}

fn history_page_sql(where_sql: &str, arg_count: usize) -> String {
    // `trash = 0` is part of the page, not the caller's filter, so a paged
    // request can never see a row that is inside its undo window.
    format!(
        "SELECT {HISTORY_LIST_COLUMNS} FROM {HISTORY_LIST_FROM}{}
         ORDER BY h.completed_at DESC LIMIT ?{} OFFSET ?{}",
        with_condition(where_sql, "COALESCE(m.trash, 0) = 0"),
        arg_count + 1,
        arg_count + 2
    )
}

/// One [`HistorySummary`] by id, read inside an open transaction.
fn history_summary_by_id(conn: &Connection, id: &str) -> Result<Option<HistorySummary>, NzbError> {
    let mut stmt = conn.prepare(
        "SELECT id, name, category, status, total_bytes, downloaded_bytes, added_at,
         completed_at, output_dir, error_message
         FROM history WHERE id = ?1",
    )?;
    Ok(stmt
        .query_row(params![id], |row| {
            Ok(HistorySummary {
                id: row.get(0)?,
                name: row.get(1)?,
                category: row.get(2)?,
                status: parse_status(&row.get::<_, String>(3)?),
                total_bytes: row.get::<_, i64>(4)? as u64,
                downloaded_bytes: row.get::<_, i64>(5)? as u64,
                added_at: parse_datetime(&row.get::<_, String>(6)?),
                completed_at: parse_datetime(&row.get::<_, String>(7)?),
                output_dir: row.get::<_, String>(8)?.into(),
                error_message: row.get(9)?,
            })
        })
        .optional()?)
}

fn history_list_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<HistoryEntry> {
    let stages_json: String = row.get::<_, Option<String>>(10)?.unwrap_or_default();
    let stages: Vec<StageResult> = serde_json::from_str(&stages_json).unwrap_or_default();
    let stats_json: String = row.get::<_, Option<String>>(12)?.unwrap_or_default();
    let server_stats: Vec<ServerArticleStats> =
        serde_json::from_str(&stats_json).unwrap_or_default();
    let has_nzb: bool = row.get(13)?;

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
        nzb_data: if has_nzb { Some(Vec::new()) } else { None },
        retry_data: None,
        par2_volumes_fetched: row.get(17)?,
        par2_volumes_available: row.get(18)?,
        client_removal: client_removal_at(row, 19)?,
    })
}

/// The [`HistoryClientRemoval`] read from `removed_by`, `removed_at` and
/// `files_deleted`, starting at column `first`.
fn client_removal_at(
    row: &rusqlite::Row<'_>,
    first: usize,
) -> rusqlite::Result<Option<HistoryClientRemoval>> {
    let Some(removed_by) = row.get::<_, Option<String>>(first)? else {
        return Ok(None);
    };
    Ok(Some(HistoryClientRemoval {
        removed_by,
        removed_at: parse_datetime(&row.get::<_, String>(first + 1)?),
        files_deleted: row.get(first + 2)?,
    }))
}

/// SQL-side history filter. Every set field narrows the match.
#[derive(Debug, Clone, Default)]
pub struct HistoryFilter {
    /// Only rows completed at or after this instant.
    pub completed_since: Option<chrono::DateTime<Utc>>,
    /// Exact status, compared case-insensitively.
    pub status: Option<JobStatus>,
    /// Exact category.
    pub category: Option<String>,
    /// Case-insensitive substring of the name.
    pub search: Option<String>,
}

impl HistoryFilter {
    fn to_sql(&self) -> (String, Vec<Box<dyn rusqlite::ToSql>>) {
        let mut clauses: Vec<String> = Vec::new();
        let mut args: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        if let Some(since) = self.completed_since {
            args.push(Box::new(since.to_rfc3339()));
            clauses.push(format!("completed_at >= ?{}", args.len()));
        }
        if let Some(status) = self.status {
            args.push(Box::new(status.to_string()));
            clauses.push(format!("status = ?{} COLLATE NOCASE", args.len()));
        }
        if let Some(category) = &self.category {
            args.push(Box::new(category.clone()));
            clauses.push(format!("category = ?{}", args.len()));
        }
        if let Some(search) = &self.search {
            args.push(Box::new(search.to_lowercase()));
            clauses.push(format!("instr(lower(name), ?{}) > 0", args.len()));
        }
        if clauses.is_empty() {
            (String::new(), args)
        } else {
            (format!(" WHERE {}", clauses.join(" AND ")), args)
        }
    }
}

/// The history columns stored ahead of the NZB blob: enough to filter,
/// count, total and delete without touching the blob's overflow pages.
#[derive(Debug, Clone)]
pub struct HistorySummary {
    pub id: String,
    pub name: String,
    pub category: String,
    pub status: JobStatus,
    pub total_bytes: u64,
    pub downloaded_bytes: u64,
    pub added_at: chrono::DateTime<Utc>,
    pub completed_at: chrono::DateTime<Utc>,
    pub output_dir: std::path::PathBuf,
    pub error_message: Option<String>,
}

/// The aggregate half of the history panel's statistics: completed count,
/// completed bytes, completed duration and failed count.
///
/// Kept beside [`HISTORY_LIST_FROM`]'s query helpers so the query-plan test
/// asserts on the SQL the endpoint actually runs.
fn history_window_stats_sql(where_sql: &str) -> String {
    format!(
        "SELECT
            COALESCE(SUM(CASE WHEN h.status = 'Completed' COLLATE NOCASE
                              THEN 1 ELSE 0 END), 0),
            COALESCE(SUM(CASE WHEN h.status = 'Completed' COLLATE NOCASE
                              THEN h.total_bytes ELSE 0 END), 0),
            COALESCE(SUM(CASE WHEN h.status = 'Completed' COLLATE NOCASE
                              THEN CAST(ROUND((julianday(h.completed_at)
                                               - julianday(h.added_at)) * 86400000.0)
                                        AS INTEGER)
                              ELSE 0 END), 0),
            COALESCE(SUM(CASE WHEN h.status = 'Failed' COLLATE NOCASE
                              THEN 1 ELSE 0 END), 0)
         FROM history h{}",
        with_condition(
            where_sql,
            "h.id NOT IN (SELECT id FROM history_list_meta WHERE trash = 1)"
        )
    )
}

/// Error text of the failed rows matching the same filter, newest first.
fn history_failure_messages_sql(where_sql: &str) -> String {
    format!(
        "SELECT h.error_message FROM history h INDEXED BY idx_h_fail_cover{}
         ORDER BY h.completed_at DESC",
        with_condition(
            &with_condition(where_sql, "h.status = 'Failed' COLLATE NOCASE"),
            "h.id NOT IN (SELECT id FROM history_list_meta WHERE trash = 1)"
        )
    )
}

/// The distinct non-empty categories of the untrashed rows, in name order.
fn history_categories_sql() -> String {
    "SELECT DISTINCT h.category FROM history h
     WHERE h.id NOT IN (SELECT id FROM history_list_meta WHERE trash = 1)
       AND h.category <> ''
     ORDER BY h.category"
        .to_string()
}

/// The history panel's aggregates over a time window, computed in SQL.
///
/// `completed_duration_ms` is the sum of each completed row's
/// `(completed_at - added_at)` in whole milliseconds. SQLite derives it from
/// `julianday`, whose double precision is finer than a millisecond, and rounds
/// to the nearest one, so the totals match the Rust fold's `num_milliseconds()`
/// except where a row's duration has a sub-millisecond remainder.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HistoryWindowStats {
    pub completed: u64,
    pub completed_bytes: u64,
    pub completed_duration_ms: i64,
    pub failed: u64,
}

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

/// Record the schema version a batch of migrations reached. The caller owns
/// the transaction and commits it once, after the last migration.
fn stamp_schema_version(tx: &rusqlite::Transaction<'_>, version: u32) -> Result<(), NzbError> {
    tx.execute_batch(&format!(
        "DELETE FROM schema_version; INSERT INTO schema_version (version) VALUES ({version});"
    ))?;
    Ok(())
}

#[allow(dead_code)]
fn add_column_if_missing(
    tx: &rusqlite::Transaction<'_>,
    table: &str,
    column: &str,
    definition: &str,
) -> Result<(), NzbError> {
    let exists: bool = tx
        .query_row(
            "SELECT COUNT(*) > 0 FROM pragma_table_info(?) WHERE name = ?",
            [table, column],
            |row| row.get(0),
        )
        .unwrap_or(false);

    if !exists {
        tx.execute_batch(&format!(
            "ALTER TABLE {table} ADD COLUMN {column} {definition};"
        ))?;
    }

    Ok(())
}

/// Run `definition` to create an index only when `table` exists and carries
/// every column it indexes.
///
/// Migration tests build partial schemas to reach an older version, so a
/// later step must not assume the full table shape. An index over a column a
/// partial schema lacks is skipped, exactly as `add_column_if_missing` would
/// leave the column absent.
fn create_index_if_columns(
    tx: &rusqlite::Transaction<'_>,
    table: &str,
    columns: &[&str],
    definition: &str,
) -> Result<(), NzbError> {
    for column in columns {
        let exists: bool = tx
            .query_row(
                "SELECT COUNT(*) > 0 FROM pragma_table_info(?) WHERE name = ?",
                [table, column],
                |row| row.get(0),
            )
            .unwrap_or(false);
        if !exists {
            return Ok(());
        }
    }
    tx.execute_batch(definition)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_job(id: &str, name: &str) -> NzbJob {
        NzbJob {
            slice_map: None,
            held_par2_bytes: 0,
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
            client_removal: None,
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
            par2_volumes_fetched: None,
            par2_volumes_available: None,
        }
    }

    #[test]
    fn duplicate_candidates_match_queue_and_history_by_name_or_digest() {
        let db = Database::open_memory().unwrap();
        let payload = b"<nzb>queued</nzb>";
        let digest = nzb_digest(payload);

        let queued = make_job("queued-job", "Show.S01E01");
        db.queue_insert(&queued).unwrap();
        db.queue_store_nzb_data(&queued.id, payload).unwrap();

        let mut finished = make_history("done-job", "Other.Release");
        finished.nzb_data = Some(b"<nzb>finished</nzb>".to_vec());
        db.history_insert(&finished).unwrap();

        // Name match is case-insensitive and reports the queue row.
        let by_name = db
            .find_duplicate_candidates("show.s01e01", Some(&nzb_digest(b"unrelated")))
            .unwrap();
        assert_eq!(by_name.len(), 1);
        assert_eq!(by_name[0].id, "queued-job");
        assert_eq!(by_name[0].location, DuplicateLocation::Queue);
        assert!(by_name[0].by_name && !by_name[0].by_digest);

        // Digest match finds a history row under a different name.
        let by_digest = db
            .find_duplicate_candidates("Renamed.Upload", Some(&nzb_digest(b"<nzb>finished</nzb>")))
            .unwrap();
        assert_eq!(by_digest.len(), 1);
        assert_eq!(by_digest[0].id, "done-job");
        assert_eq!(by_digest[0].location, DuplicateLocation::History);
        assert!(!by_digest[0].by_name && by_digest[0].by_digest);

        // The queued payload's digest also matches the queue row by content.
        let both = db
            .find_duplicate_candidates("Show.S01E01", Some(&digest))
            .unwrap();
        assert_eq!(both.len(), 1);
        assert!(both[0].by_name && both[0].by_digest);

        assert!(
            db.find_duplicate_candidates("Nothing.Here", Some(&nzb_digest(b"new")))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn duplicate_candidates_seek_indexes_not_scan() {
        let db = Database::open_memory().unwrap();
        let mut stmt = db
            .conn
            .prepare(&format!("EXPLAIN QUERY PLAN {DUPLICATE_CANDIDATES_SQL}"))
            .unwrap();
        let plan: Vec<String> = stmt
            .query_map(params!["x", "sha256:y"], |row| row.get(3))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        for index in [
            "idx_queue_name_lower",
            "idx_queue_nzb_digest",
            "idx_history_name_lower",
            "idx_history_nzb_digest",
        ] {
            assert!(
                plan.iter().any(|step| step.contains(index)),
                "{index} unused: {plan:?}"
            );
        }
        assert!(
            !plan
                .iter()
                .any(|step| step.starts_with("SCAN queue") || step.starts_with("SCAN history")),
            "duplicate lookup scans a table: {plan:?}"
        );
    }

    #[test]
    fn duplicate_lookup_stays_fast_with_large_history() {
        // A file database, like production: 2,000 history rows each carrying
        // a 256 KiB NZB blob inline (~500 MB). The lookup must not read them.
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(&dir.path().join("dup.db")).unwrap();
        let blob = vec![b'x'; 256 * 1024];
        db.conn.execute_batch("PRAGMA synchronous = OFF;").unwrap();
        for i in 0..2000 {
            let mut entry = make_history(&format!("h{i}"), &format!("Release.{i}"));
            let mut data = blob.clone();
            data.extend_from_slice(i.to_string().as_bytes());
            entry.nzb_data = Some(data);
            db.history_insert(&entry).unwrap();
        }

        let start = std::time::Instant::now();
        let found = db
            .find_duplicate_candidates("release.1999", Some(&nzb_digest(b"new")))
            .unwrap();
        let elapsed = start.elapsed();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].id, "h1999");
        assert!(
            elapsed < std::time::Duration::from_millis(200),
            "duplicate lookup took {elapsed:?}"
        );
    }

    #[test]
    fn nzb_digest_is_sha256_hex_with_prefix() {
        assert_eq!(
            nzb_digest(b""),
            "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
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
    fn history_round_trips_par2_volume_counts() {
        let db = Database::open_memory().unwrap();
        let mut stored = make_history("par2-vols", "Vols");
        stored.par2_volumes_fetched = Some(2);
        stored.par2_volumes_available = Some(5);
        db.history_insert(&stored).unwrap();
        db.history_insert(&make_history("par2-legacy", "Legacy"))
            .unwrap();

        let got = db.history_get("par2-vols").unwrap().unwrap();
        assert_eq!(
            (got.par2_volumes_fetched, got.par2_volumes_available),
            (Some(2), Some(5))
        );
        let listed = db.history_list(10).unwrap();
        let vols = listed.iter().find(|entry| entry.id == "par2-vols").unwrap();
        assert_eq!(
            (vols.par2_volumes_fetched, vols.par2_volumes_available),
            (Some(2), Some(5))
        );
        let legacy = listed
            .iter()
            .find(|entry| entry.id == "par2-legacy")
            .unwrap();
        assert_eq!(legacy.par2_volumes_available, None);
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

    /// The history row and its statistics-ledger row are one logical write:
    /// if the ledger insert fails, the history row must not be left behind.
    #[test]
    fn history_insert_is_atomic_with_statistics_ledger() {
        let db = Database::open_memory().unwrap();
        db.conn
            .execute_batch("DROP TABLE download_statistics;")
            .unwrap();

        assert!(db.history_insert(&make_history("h-1", "Half")).is_err());
        assert!(
            db.history_get("h-1").unwrap().is_none(),
            "history row must roll back with the failed statistics write"
        );
    }

    /// A trashed row disappears from every history read, keeps its blobs for
    /// the undo, and comes back unchanged when it is restored.
    #[test]
    fn history_trash_hides_the_row_and_restore_brings_it_back() {
        let db = Database::open_memory().unwrap();
        let mut entry = make_history("undo-1", "Undo.Me");
        entry.nzb_data = Some(b"<nzb/>".to_vec());
        entry.retry_data = Some(b"retry".to_vec());
        db.history_insert(&entry).unwrap();
        db.history_insert(&make_history("other", "Other")).unwrap();

        let now = Utc::now();
        let trashed = db
            .history_trash(
                &["undo-1".into(), "missing".into()],
                now + chrono::Duration::seconds(10),
            )
            .unwrap();
        assert_eq!(trashed.len(), 1, "only the existing row is reported");
        assert_eq!(trashed[0].id, "undo-1");
        assert_eq!(db.history_count().unwrap(), 1);

        assert!(db.history_get("undo-1").unwrap().is_none());
        assert!(
            db.history_get_listed(&["undo-1".into()])
                .unwrap()
                .is_empty()
        );
        assert!(
            db.history_list(10)
                .unwrap()
                .iter()
                .all(|e| e.id != "undo-1")
        );
        let (page, total) = db.history_page(&HistoryFilter::default(), 0, 50).unwrap();
        assert_eq!(total, 1);
        assert!(page.iter().all(|e| e.id != "undo-1"));
        assert!(
            !db.history_categories().unwrap().is_empty(),
            "another row still supplies the category"
        );
        assert!(
            db.history_summaries(&HistoryFilter::default())
                .unwrap()
                .iter()
                .all(|e| e.id != "undo-1")
        );
        assert!(
            db.find_duplicate_candidates("Undo.Me", None)
                .unwrap()
                .is_empty(),
            "a trashed row must not block a re-add"
        );

        // The blobs survive the trash, which is what makes undo lossless.
        assert_eq!(
            db.history_get_nzb_data("undo-1").unwrap().as_deref(),
            Some(&b"<nzb/>"[..])
        );
        assert_eq!(
            db.history_get_retry_data("undo-1").unwrap().as_deref(),
            Some(&b"retry"[..])
        );
        assert_eq!(db.history_trashed_ids().unwrap(), vec!["undo-1"]);

        let restored = db
            .history_restore(&["undo-1".into(), "missing".into()], now)
            .unwrap();
        assert_eq!(restored, vec!["undo-1"]);
        assert_eq!(db.history_count().unwrap(), 2);
        let got = db.history_get("undo-1").unwrap().unwrap();
        assert_eq!(got.name, "Undo.Me");
        assert_eq!(
            db.find_duplicate_candidates("Undo.Me", None).unwrap().len(),
            1
        );
        assert!(db.history_trashed_ids().unwrap().is_empty());
    }

    /// The deadline is the whole contract: a restore is accepted up to it and
    /// refused after it, and only a purge removes the expired row.
    #[test]
    fn history_restore_is_refused_after_the_deadline_until_a_purge() {
        let db = Database::open_memory().unwrap();
        db.history_insert(&make_history("expiring", "Expiring"))
            .unwrap();
        let trashed_at = Utc::now();
        let deadline = trashed_at + chrono::Duration::seconds(10);
        db.history_trash(&["expiring".into()], deadline).unwrap();

        let after = deadline + chrono::Duration::seconds(1);
        assert!(
            db.history_restore(&["expiring".into()], after)
                .unwrap()
                .is_empty()
        );
        assert_eq!(db.history_count().unwrap(), 0, "still hidden from reads");

        // Not yet expired, and not purged by a sweep that runs before the
        // deadline, so the row is still physically there.
        assert!(db.history_purge_expired(trashed_at).unwrap().is_empty());
        assert_eq!(db.history_trashed_ids().unwrap(), vec!["expiring"]);

        let purged = db.history_purge_expired(after).unwrap();
        assert_eq!(purged.len(), 1);
        assert_eq!(purged[0].id, "expiring");
        assert!(db.history_get_nzb_data("expiring").unwrap().is_none());
        assert!(db.history_trashed_ids().unwrap().is_empty());
    }

    /// Expired rows are purged even when the deadline is already in the past
    /// when the sweep runs, and the job's ledger rows go with them.
    #[test]
    fn history_purge_removes_the_row_and_its_ledger_entries() {
        let db = Database::open_memory().unwrap();
        db.history_insert(&make_history("trash-ledger", "Ledger"))
            .unwrap();
        note_present(&db, "trash-ledger", 0);
        db.history_trash(
            &["trash-ledger".into()],
            Utc::now() - chrono::Duration::seconds(1),
        )
        .unwrap();

        assert_eq!(db.history_purge_expired(Utc::now()).unwrap().len(), 1);
        assert_eq!(ledger_rows(&db, "trash-ledger"), 0);
    }

    /// Retention counts only visible rows: a trashed row must not consume a
    /// slot, and must not be deleted by the retention pass either.
    #[test]
    fn history_enforce_retention_ignores_trashed_rows() {
        let db = Database::open_memory().unwrap();
        for i in 0..3 {
            db.history_insert(&make_history(&format!("keep-{i}"), &format!("Keep {i}")))
                .unwrap();
        }
        db.history_insert(&make_history("trashed", "Trashed"))
            .unwrap();
        let future = Utc::now() + chrono::Duration::seconds(10);
        db.history_trash(&["trashed".into()], future).unwrap();

        db.history_enforce_retention(3).unwrap();
        assert_eq!(db.history_count().unwrap(), 3);
        assert_eq!(
            db.history_trashed_ids().unwrap(),
            vec!["trashed"],
            "retention must leave the undo window to the purge deadline"
        );

        // Restoring after a retention pass still works while the window is open.
        db.history_restore(&["trashed".into()], Utc::now()).unwrap();
        assert_eq!(db.history_count().unwrap(), 4);
    }

    /// Applying the schema is one transaction, not one per migration.
    ///
    /// Each migration used to commit separately, and every commit is an fsync.
    /// On a contended volume those serialise: 19 commits were enough to keep
    /// the backend from ever binding its port inside the e2e health timeout.
    /// The version row must therefore appear only once the whole batch has
    /// succeeded, and a re-open of an up-to-date database must write nothing.
    #[test]
    fn fresh_database_migrates_in_one_transaction() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("queue.db");
        let db = Database::open(&path).unwrap();

        let version: u32 = db
            .conn
            .query_row("SELECT MAX(version) FROM schema_version", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert!(version >= 21, "schema should reach v21, got {version}");

        // Every migration's objects are present, which is only true if the
        // single batch ran to the end before stamping the version.
        for table in ["queue", "history", "settings", "history_list_meta"] {
            let exists: i64 = db
                .conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
                    [table],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(exists, 1, "{table} should exist after migrating");
        }

        // Re-opening an up-to-date database has no migration to apply, so it
        // must not touch schema_version at all.
        drop(db);
        let reopened = Database::open(&path).unwrap();
        let versions: i64 = reopened
            .conn
            .query_row("SELECT COUNT(*) FROM schema_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(versions, 1, "the version row is written exactly once");
    }

    /// A lock held briefly by another connection (backup tool, second
    /// process) must be waited on, not turned into an immediate SQLITE_BUSY.
    #[test]
    fn open_sets_busy_timeout() {
        let directory = tempfile::tempdir().unwrap();
        let db = Database::open(&directory.path().join("queue.db")).unwrap();
        let timeout_ms: i64 = db
            .conn
            .query_row("PRAGMA busy_timeout", [], |row| row.get(0))
            .unwrap();
        assert!(timeout_ms >= 5_000, "busy_timeout was {timeout_ms} ms");
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

    fn seeded_history_db() -> (Database, Vec<HistoryEntry>) {
        let db = Database::open_memory().unwrap();
        let base = Utc::now();
        for i in 0..40u32 {
            let mut entry = make_history(&format!("h{i:02}"), &format!("Show.S01E{i:02}"));
            if i % 3 == 0 {
                entry.name = format!("Movie.Ünïcode.{i}");
            }
            entry.category = ["movies", "tv", ""][i as usize % 3].into();
            if i % 4 == 0 {
                entry.status = JobStatus::Failed;
                entry.failure_code = Some(JobFailureCode::ArchivePasswordRequired);
                entry.error_message = Some("Password required: archive".into());
            }
            entry.completed_at = base - chrono::Duration::hours(i64::from(i) * 7);
            entry.added_at = entry.completed_at - chrono::Duration::minutes(5);
            if i % 5 == 0 {
                entry.nzb_data = Some(vec![7u8; 64 * 1024]);
                entry.retry_data = Some(vec![9u8; 64 * 1024]);
            }
            db.history_insert(&entry).unwrap();
        }
        let all = db.history_list(usize::MAX).unwrap();
        (db, all)
    }

    #[test]
    fn history_page_matches_the_in_memory_filter() {
        let (db, all) = seeded_history_db();
        let now = Utc::now();
        let filters = [
            HistoryFilter::default(),
            HistoryFilter {
                completed_since: Some(now - chrono::Duration::days(3)),
                ..Default::default()
            },
            HistoryFilter {
                status: Some(JobStatus::Failed),
                ..Default::default()
            },
            HistoryFilter {
                category: Some("tv".into()),
                search: Some("s01e1".into()),
                ..Default::default()
            },
            HistoryFilter {
                completed_since: Some(now - chrono::Duration::days(7)),
                status: Some(JobStatus::Completed),
                category: Some("movies".into()),
                search: Some("MOVIE".into()),
            },
        ];
        for filter in &filters {
            let expected: Vec<&HistoryEntry> = all
                .iter()
                .filter(|e| filter.completed_since.is_none_or(|c| e.completed_at >= c))
                .filter(|e| filter.status.is_none_or(|s| e.status == s))
                .filter(|e| filter.category.as_ref().is_none_or(|c| &e.category == c))
                .filter(|e| {
                    filter
                        .search
                        .as_ref()
                        .is_none_or(|s| e.name.to_lowercase().contains(&s.to_lowercase()))
                })
                .collect();
            for (offset, limit) in [(0, 50), (0, 3), (2, 5), (100, 10)] {
                let (page, total) = db.history_page(filter, offset, limit).unwrap();
                assert_eq!(total, expected.len(), "{filter:?}");
                let want: Vec<&str> = expected
                    .iter()
                    .skip(offset)
                    .take(limit)
                    .map(|e| e.id.as_str())
                    .collect();
                let got: Vec<&str> = page.iter().map(|e| e.id.as_str()).collect();
                assert_eq!(got, want, "{filter:?} offset {offset} limit {limit}");
            }
            let summaries: Vec<String> = db
                .history_summaries(filter)
                .unwrap()
                .into_iter()
                .map(|s| s.id)
                .collect();
            let want: Vec<String> = expected.iter().map(|e| e.id.clone()).collect();
            assert_eq!(summaries, want, "{filter:?}");
        }
        assert_eq!(db.history_categories().unwrap(), vec!["movies", "tv"]);
    }

    /// The panel's statistics now come from SQL aggregates plus the failed
    /// rows' error text. They must equal the fold the endpoint used to run
    /// over whole summaries, for every filter the panel sends: the unfiltered
    /// window, a days window, a status filter and a category+search filter.
    #[test]
    fn history_window_stats_match_the_fold_over_summaries() {
        let (db, all) = seeded_history_db();
        let now = Utc::now();
        let filters = [
            HistoryFilter::default(),
            HistoryFilter {
                completed_since: Some(now - chrono::Duration::days(3)),
                ..Default::default()
            },
            HistoryFilter {
                status: Some(JobStatus::Failed),
                ..Default::default()
            },
            HistoryFilter {
                completed_since: Some(now - chrono::Duration::days(7)),
                status: Some(JobStatus::Completed),
                category: Some("movies".into()),
                search: Some("MOVIE".into()),
            },
        ];
        for filter in &filters {
            let in_window: Vec<&HistoryEntry> = all
                .iter()
                .filter(|e| filter.completed_since.is_none_or(|c| e.completed_at >= c))
                .filter(|e| filter.status.is_none_or(|s| e.status == s))
                .filter(|e| filter.category.as_ref().is_none_or(|c| &e.category == c))
                .filter(|e| {
                    filter
                        .search
                        .as_ref()
                        .is_none_or(|s| e.name.to_lowercase().contains(&s.to_lowercase()))
                })
                .collect();

            let completed: Vec<&&HistoryEntry> = in_window
                .iter()
                .filter(|e| e.status == JobStatus::Completed)
                .collect();
            let expected = HistoryWindowStats {
                completed: completed.len() as u64,
                completed_bytes: completed.iter().map(|e| e.total_bytes).sum(),
                completed_duration_ms: completed
                    .iter()
                    .map(|e| (e.completed_at - e.added_at).num_milliseconds())
                    .sum(),
                failed: in_window
                    .iter()
                    .filter(|e| e.status == JobStatus::Failed)
                    .count() as u64,
            };
            assert_eq!(
                db.history_window_stats(filter).unwrap(),
                expected,
                "{filter:?}"
            );

            // Failure reasons are grouped from the messages, newest first.
            let want_messages: Vec<Option<String>> = {
                let mut failed: Vec<&&HistoryEntry> = in_window
                    .iter()
                    .filter(|e| e.status == JobStatus::Failed)
                    .collect();
                failed.sort_by_key(|e| std::cmp::Reverse(e.completed_at));
                failed.iter().map(|e| e.error_message.clone()).collect()
            };
            assert_eq!(
                db.history_failure_messages(filter).unwrap(),
                want_messages,
                "{filter:?}"
            );
        }
    }

    /// Reason text that differs only after the first clause must group the way
    /// the old Rust fold grouped it: split on the first `.` or `:`, keep 32
    /// chars, and treat a missing message as `unknown`.
    #[test]
    fn history_window_stats_aggregate_edge_case_statuses_and_messages() {
        let db = Database::open_memory().unwrap();
        let now = Utc::now();
        let cases: [(&str, JobStatus, Option<&str>); 5] = [
            ("a", JobStatus::Completed, None),
            ("b", JobStatus::Completed, None),
            ("c", JobStatus::Failed, Some("CRC mismatch: volume 2")),
            ("d", JobStatus::Failed, Some("CRC mismatch. volume 3")),
            ("e", JobStatus::Failed, None),
        ];
        for (i, (id, status, message)) in cases.iter().enumerate() {
            let mut entry = make_history(id, id);
            entry.status = *status;
            if *status == JobStatus::Failed {
                entry.failure_code = Some(JobFailureCode::ArchiveInvalid);
            }
            entry.total_bytes = 1_000;
            entry.completed_at = now - chrono::Duration::minutes(i as i64);
            entry.added_at = entry.completed_at - chrono::Duration::seconds(30);
            entry.error_message = message.map(str::to_owned);
            db.history_insert(&entry).unwrap();
        }

        let stats = db.history_window_stats(&HistoryFilter::default()).unwrap();
        assert_eq!(
            stats,
            HistoryWindowStats {
                completed: 2,
                completed_bytes: 2_000,
                completed_duration_ms: 60_000,
                failed: 3,
            }
        );
        // Newest first, and a NULL message survives as NULL so the caller can
        // render its `unknown` fallback.
        assert_eq!(
            db.history_failure_messages(&HistoryFilter::default())
                .unwrap(),
            vec![
                Some("CRC mismatch: volume 2".into()),
                Some("CRC mismatch. volume 3".into()),
                None,
            ]
        );

        // A trashed row is in neither aggregate nor the message list.
        db.history_trash(&["c".to_string()], now + chrono::Duration::seconds(5))
            .unwrap();
        let stats = db.history_window_stats(&HistoryFilter::default()).unwrap();
        assert_eq!((stats.completed, stats.failed), (2, 2));
        assert_eq!(
            db.history_failure_messages(&HistoryFilter::default())
                .unwrap(),
            vec![Some("CRC mismatch. volume 3".into()), None]
        );
    }

    /// The history panel's aggregates, category list and page count must all
    /// run off an index and never `SCAN history`, so a cold load neither
    /// walks the wide table nor reaches its overflow pages, where the stored
    /// NZB blobs live. The panel sends two windows: the whole history (the
    /// aggregates and page) and a days window.
    ///
    /// The category list is deliberately unfiltered — the panel offers every
    /// category in the history — and a days filter on it would have no index
    /// to scan in date order, so it is asserted once, outside the window loop.
    #[test]
    fn history_panel_aggregates_use_covering_indexes() {
        let (db, _) = seeded_history_db();
        let filters = [
            HistoryFilter::default(),
            HistoryFilter {
                completed_since: Some(Utc::now() - chrono::Duration::days(3)),
                ..Default::default()
            },
        ];
        for filter in &filters {
            let (where_sql, args) = filter.to_sql();
            // Without a date bound the count is answered by the implicit
            // unique index on `history.id`; a days window narrows it through
            // the statistics' covering index instead.
            let count_index = if filter.completed_since.is_some() {
                "idx_h_stats_cover"
            } else {
                "sqlite_autoindex_history_1"
            };
            let statements = [
                (
                    "window stats",
                    history_window_stats_sql(&where_sql),
                    "idx_h_stats_cover",
                ),
                (
                    "failure messages",
                    history_failure_messages_sql(&where_sql),
                    "idx_h_fail_cover",
                ),
                (
                    "page count",
                    format!(
                        "SELECT COUNT(*) FROM history h{}",
                        with_condition(
                            &where_sql,
                            "h.id NOT IN (SELECT id FROM history_list_meta WHERE trash = 1)"
                        )
                    ),
                    count_index,
                ),
            ];
            for (label, sql, index) in &statements {
                let mut stmt = db
                    .conn
                    .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
                    .unwrap();
                let bound: Vec<&dyn rusqlite::ToSql> =
                    args.iter().map(|a| a as &dyn rusqlite::ToSql).collect();
                let plan: Vec<String> = stmt
                    .query_map(rusqlite::params_from_iter(bound), |row| row.get(3))
                    .unwrap()
                    .collect::<Result<_, _>>()
                    .unwrap();
                let plan = plan.join("\n");
                assert!(
                    !plan.contains("SCAN history"),
                    "{label} scans the history table: {sql}\n{plan}"
                );
                assert!(
                    plan.contains(&format!("USING INDEX {index}"))
                        || plan.contains(&format!("USING COVERING INDEX {index}")),
                    "{label} does not use {index}: {sql}\n{plan}"
                );
            }
        }

        let mut stmt = db
            .conn
            .prepare(&format!("EXPLAIN QUERY PLAN {}", history_categories_sql()))
            .unwrap();
        let plan: Vec<String> = stmt
            .query_map([], |row| row.get(3))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        let plan = plan.join("\n");
        assert!(
            plan.contains("USING COVERING INDEX idx_h_cat_cover"),
            "categories do not use idx_h_cat_cover\n{plan}"
        );
    }

    #[test]
    fn history_list_rows_flag_stored_nzbs_and_keep_full_fields() {
        let (db, all) = seeded_history_db();
        let (page, _) = db.history_page(&HistoryFilter::default(), 0, 100).unwrap();
        assert_eq!(page.len(), all.len());
        for entry in &page {
            let has_nzb = entry.id[1..].parse::<u32>().unwrap() % 5 == 0;
            assert_eq!(entry.nzb_data.is_some(), has_nzb, "{}", entry.id);
            assert!(entry.nzb_data.as_ref().is_none_or(Vec::is_empty));
            assert!(entry.retry_data.is_none());
            assert_eq!(entry.stages.len(), 1);
        }
        let ids = vec!["h05".to_string(), "missing".to_string(), "h01".to_string()];
        let listed = db.history_get_listed(&ids).unwrap();
        let got: Vec<&str> = listed.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(got, ["h05", "h01"]);
        assert!(listed[0].nzb_data.is_some());
    }

    /// A list page must never load `nzb_data` or `retry_data` content: the
    /// only access to `nzb_data` is the header-only `typeof()` probe
    /// (OP_Column with OPFLAG_TYPEOFARG), and `retry_data` is not touched.
    #[test]
    fn history_page_query_does_not_read_blob_columns() {
        const OPFLAG_TYPEOFARG: i64 = 0x80;
        let (db, _) = seeded_history_db();
        let column_index = |name: &str| -> i64 {
            db.conn
                .query_row(
                    "SELECT cid FROM pragma_table_info('history') WHERE name = ?1",
                    params![name],
                    |row| row.get(0),
                )
                .unwrap()
        };
        let nzb_col = column_index("nzb_data");
        let retry_col = column_index("retry_data");
        let root: i64 = db
            .conn
            .query_row(
                "SELECT rootpage FROM sqlite_master WHERE type = 'table' AND name = 'history'",
                [],
                |row| row.get(0),
            )
            .unwrap();

        let filters = [
            HistoryFilter::default(),
            HistoryFilter {
                completed_since: Some(Utc::now()),
                status: Some(JobStatus::Failed),
                category: Some("tv".into()),
                search: Some("x".into()),
            },
        ];
        for filter in &filters {
            let (where_sql, args) = filter.to_sql();
            let sql = format!("EXPLAIN {}", history_page_sql(&where_sql, args.len()));
            let mut stmt = db.conn.prepare(&sql).unwrap();
            let ops: Vec<(String, i64, i64, i64)> = stmt
                .query_map(
                    rusqlite::params_from_iter(
                        args.iter()
                            .map(|a| a.as_ref())
                            .chain([&1i64 as &dyn rusqlite::ToSql, &0i64]),
                    ),
                    |row| Ok((row.get(1)?, row.get(2)?, row.get(3)?, row.get(6)?)),
                )
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap();
            let table_cursors: Vec<i64> = ops
                .iter()
                .filter(|(op, _, p2, _)| op == "OpenRead" && *p2 == root)
                .map(|(_, p1, _, _)| *p1)
                .collect();
            assert!(!table_cursors.is_empty());
            let mut probed_nzb = false;
            for (op, p1, p2, p5) in &ops {
                if op != "Column" || !table_cursors.contains(p1) {
                    continue;
                }
                assert_ne!(*p2, retry_col, "list page reads retry_data");
                assert!(
                    *p2 <= nzb_col,
                    "list page reads history column {p2}, stored after nzb_data"
                );
                if *p2 == nzb_col {
                    assert_ne!(p5 & OPFLAG_TYPEOFARG, 0, "list page loads nzb_data");
                    probed_nzb = true;
                }
            }
            assert!(probed_nzb, "has_nzb probe missing from the plan");
        }
    }

    #[test]
    fn history_client_removal_marks_the_row_until_it_is_deleted() {
        let db = Database::open_memory().unwrap();
        db.history_insert(&make_history("kept", "Kept")).unwrap();
        db.history_insert(&make_history("plain", "Plain")).unwrap();
        let marked = db
            .history_mark_client_removed(
                &["kept".to_string(), "missing".to_string()],
                "Sonarr",
                Utc::now(),
                true,
            )
            .unwrap();
        assert_eq!(marked, 1, "an unknown id is skipped");

        let removal = db.history_get("kept").unwrap().unwrap().client_removal;
        assert_eq!(
            removal.as_ref().map(|r| r.removed_by.as_str()),
            Some("Sonarr")
        );
        assert!(removal.unwrap().files_deleted);
        let listed = db.history_list(10).unwrap();
        assert_eq!(listed.len(), 2);
        assert!(
            listed
                .iter()
                .any(|e| e.id == "kept" && e.client_removal.is_some())
        );
        assert!(
            listed
                .iter()
                .any(|e| e.id == "plain" && e.client_removal.is_none())
        );
        assert_eq!(
            db.history_client_removed_ids().unwrap(),
            std::collections::HashSet::from(["kept".to_string()])
        );

        // A UI delete still goes through the undo window, then drops the
        // marker with the row.
        let trashed = db
            .history_trash(
                &["kept".to_string()],
                Utc::now() + chrono::Duration::hours(1),
            )
            .unwrap();
        assert_eq!(trashed.len(), 1);
        assert!(db.history_get("kept").unwrap().is_none());
        db.history_remove("kept").unwrap();
        assert!(db.history_client_removed_ids().unwrap().is_empty());
    }

    #[test]
    fn history_list_meta_follows_inserts_deletes_and_retention() {
        let (db, _) = seeded_history_db();
        let meta_count = |db: &Database| -> i64 {
            db.conn
                .query_row("SELECT COUNT(*) FROM history_list_meta", [], |row| {
                    row.get(0)
                })
                .unwrap()
        };
        let history_count: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM history", [], |row| row.get(0))
            .unwrap();
        assert_eq!(meta_count(&db), history_count);

        let listed = db.history_list(1).unwrap();
        let first = listed[0].clone();
        let full = db.history_get(&first.id).unwrap().unwrap();
        assert_eq!(full.failure_code, first.failure_code);
        assert_eq!(full.download_time_secs, first.download_time_secs);

        db.history_remove(&first.id).unwrap();
        assert_eq!(meta_count(&db), history_count - 1);
        db.history_enforce_retention(2).unwrap();
        assert_eq!(meta_count(&db), 2);
        db.history_clear().unwrap();
        assert_eq!(meta_count(&db), 0);
    }

    // --- Damage-ledger pruning (WI-1868) ---

    use crate::damage_ledger::{ArticleKey, Outcome};

    fn ledger_t(secs: i64) -> chrono::DateTime<Utc> {
        chrono::DateTime::from_timestamp(1_700_000_000 + secs, 0).unwrap()
    }

    /// Record a confirmed hole for `scope`, confirmed at `ledger_t(2)`, so it
    /// expires at `ledger_t(2) + HOLE_TTL_SECS`.
    fn note_hole(db: &Database, scope: &str, segment: u32) {
        let key = ArticleKey::new(scope, 0, segment);
        let srv = vec!["a".to_string()];
        let refused = Outcome::Refused { server: "a".into() };
        db.ledger_note(&key, "<m>", &refused, &srv, ledger_t(0))
            .unwrap();
        db.ledger_note(&key, "<m>", &refused, &srv, ledger_t(2))
            .unwrap();
    }

    fn note_present(db: &Database, scope: &str, segment: u32) {
        let key = ArticleKey::new(scope, 0, segment);
        db.ledger_note(
            &key,
            "<m>",
            &Outcome::Present,
            &["a".to_string()],
            ledger_t(0),
        )
        .unwrap();
    }

    fn ledger_rows(db: &Database, scope: &str) -> usize {
        db.ledger_load_file(scope, 0).unwrap().len()
    }

    #[test]
    fn history_remove_drops_the_jobs_ledger_rows() {
        let db = Database::open_memory().unwrap();
        db.history_insert(&make_history("done-1", "A")).unwrap();
        db.history_insert(&make_history("done-2", "B")).unwrap();
        note_present(&db, "done-1", 0);
        note_hole(&db, "done-1", 1);
        note_present(&db, "done-2", 0);

        db.history_remove("done-1").unwrap();

        assert_eq!(ledger_rows(&db, "done-1"), 0);
        assert_eq!(ledger_rows(&db, "done-2"), 1, "other jobs keep their rows");
    }

    #[test]
    fn rows_of_a_retried_job_survive_deleting_its_history_entry() {
        let db = Database::open_memory().unwrap();
        // The failed original sits in history; its retry runs under a new id.
        db.history_insert(&make_history("orig", "Release")).unwrap();
        db.queue_insert(&make_job("retry", "Release")).unwrap();
        note_hole(&db, "orig", 0);
        note_hole(&db, "retry", 0);
        note_present(&db, "retry", 1);

        db.history_remove("orig").unwrap();

        assert_eq!(ledger_rows(&db, "orig"), 0);
        assert_eq!(ledger_rows(&db, "retry"), 2);
    }

    #[test]
    fn a_job_still_queued_keeps_its_rows_when_its_history_row_goes() {
        // A failed job can be in history and the queue at once until the
        // queue view is removed.
        let db = Database::open_memory().unwrap();
        db.queue_insert(&make_job("both", "R")).unwrap();
        db.history_insert(&make_history("both", "R")).unwrap();
        note_hole(&db, "both", 0);

        db.history_remove("both").unwrap();
        assert_eq!(ledger_rows(&db, "both"), 1);

        db.queue_remove("both").unwrap();
        assert_eq!(db.ledger_prune(ledger_t(3)).unwrap(), 1);
        assert_eq!(ledger_rows(&db, "both"), 0);
    }

    #[test]
    fn history_clear_and_retention_drop_ledger_rows() {
        let db = Database::open_memory().unwrap();
        let mut old = make_history("old", "Old");
        old.completed_at = Utc::now() - chrono::Duration::days(2);
        db.history_insert(&old).unwrap();
        db.history_insert(&make_history("new", "New")).unwrap();
        db.queue_insert(&make_job("live", "Live")).unwrap();
        for scope in ["old", "new", "live"] {
            note_present(&db, scope, 0);
        }

        db.history_enforce_retention(1).unwrap();
        assert_eq!(ledger_rows(&db, "old"), 0, "aged out with its history row");
        assert_eq!(ledger_rows(&db, "new"), 1);

        db.history_clear().unwrap();
        assert_eq!(ledger_rows(&db, "new"), 0);
        assert_eq!(ledger_rows(&db, "live"), 1, "queued jobs are untouched");
    }

    #[test]
    fn prune_drops_expired_holes_only_for_jobs_no_longer_queued() {
        let db = Database::open_memory().unwrap();
        db.history_insert(&make_history("hist", "H")).unwrap();
        db.queue_insert(&make_job("live", "L")).unwrap();
        note_hole(&db, "hist", 0);
        note_present(&db, "hist", 1);
        note_hole(&db, "live", 0);
        // A late ledger write for a job already removed everywhere.
        note_present(&db, "gone", 0);

        // Before the TTL: only the orphan goes.
        let before_expiry = ledger_t(2 + crate::damage_ledger::HOLE_TTL_SECS - 1);
        assert_eq!(db.ledger_prune(before_expiry).unwrap(), 1);
        assert_eq!(ledger_rows(&db, "gone"), 0);
        assert_eq!(ledger_rows(&db, "hist"), 2);

        // At the TTL: the history job's expired hole goes; its present row and
        // the queued job's hole stay.
        let at_expiry = ledger_t(2 + crate::damage_ledger::HOLE_TTL_SECS);
        assert_eq!(db.ledger_prune(at_expiry).unwrap(), 1);
        let left = db.ledger_load_file("hist", 0).unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].key.segment_number, 1);
        assert_eq!(ledger_rows(&db, "live"), 1);
    }
}
