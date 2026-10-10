//! Database operations for newsgroup browsing.

use rusqlite::params;

use crate::db::Database;
use crate::error::NzbError;
use crate::models::{GroupRow, HeaderRow, ThreadArticle, ThreadSummary};

impl Database {
    // ---- Groups ----

    pub fn group_upsert_batch(&self, groups: &[(String, u64, u64)]) -> Result<u64, NzbError> {
        let tx = self.conn.unchecked_transaction()?;
        let mut count = 0u64;
        {
            let mut stmt = tx.prepare_cached(
                "INSERT INTO groups (name, article_count, first_article, last_article)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(name) DO UPDATE SET
                    article_count = excluded.article_count,
                    first_article = excluded.first_article,
                    last_article = excluded.last_article,
                    last_updated = datetime('now')",
            )?;
            for (name, high, low) in groups {
                let article_count = high.saturating_sub(*low) as i64;
                stmt.execute(params![name, article_count, *low as i64, *high as i64])?;
                count += 1;
            }
        }
        tx.commit()?;
        Ok(count)
    }

    pub fn group_list(
        &self,
        subscribed_only: bool,
        search: Option<&str>,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<GroupRow>, NzbError> {
        // Paginate first, then compute the unread count only for the page.
        let sql = format!(
            "SELECT g.id, g.name, g.description, g.subscribed, g.article_count,
             g.first_article, g.last_article, g.last_scanned, g.last_updated, g.created_at,
             (SELECT COUNT(*) FROM headers h WHERE h.group_id = g.id AND h.read = 0) as unread_count
             FROM (SELECT * FROM groups WHERE {GROUP_FILTER}
                   ORDER BY name LIMIT ?3 OFFSET ?4) g
             ORDER BY g.name"
        );

        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt
            .query_map(
                params![subscribed_only, search, sql_count(limit), sql_count(offset)],
                |row| {
                    Ok(GroupRow {
                        id: row.get(0)?,
                        name: row.get(1)?,
                        description: row.get(2)?,
                        subscribed: row.get::<_, i32>(3)? != 0,
                        article_count: row.get(4)?,
                        first_article: row.get(5)?,
                        last_article: row.get(6)?,
                        last_scanned: row.get(7)?,
                        last_updated: row.get(8)?,
                        created_at: row.get::<_, Option<String>>(9)?.unwrap_or_default(),
                        unread_count: row.get(10)?,
                    })
                },
            )?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn group_get(&self, id: i64) -> Result<Option<GroupRow>, NzbError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, name, description, subscribed, article_count,
             first_article, last_article, last_scanned, last_updated, created_at,
             (SELECT COUNT(*) FROM headers h WHERE h.group_id = groups.id AND h.read = 0)
             FROM groups WHERE id = ?1",
        )?;
        let result = stmt.query_row(params![id], |row| {
            Ok(GroupRow {
                id: row.get(0)?,
                name: row.get(1)?,
                description: row.get(2)?,
                subscribed: row.get::<_, i32>(3)? != 0,
                article_count: row.get(4)?,
                first_article: row.get(5)?,
                last_article: row.get(6)?,
                last_scanned: row.get(7)?,
                last_updated: row.get(8)?,
                created_at: row.get::<_, Option<String>>(9)?.unwrap_or_default(),
                unread_count: row.get(10)?,
            })
        });
        match result {
            Ok(g) => Ok(Some(g)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(NzbError::Database(e)),
        }
    }

    pub fn group_count(
        &self,
        subscribed_only: bool,
        search: Option<&str>,
    ) -> Result<i64, NzbError> {
        let count: i64 = self.conn.query_row(
            &format!("SELECT COUNT(*) FROM groups WHERE {GROUP_FILTER}"),
            params![subscribed_only, search],
            |row| row.get(0),
        )?;
        Ok(count)
    }

    pub fn group_set_subscribed(&self, id: i64, subscribed: bool) -> Result<(), NzbError> {
        self.conn.execute(
            "UPDATE groups SET subscribed = ?2 WHERE id = ?1",
            params![id, subscribed as i32],
        )?;
        Ok(())
    }

    pub fn group_update_watermark(
        &self,
        id: i64,
        last_scanned: i64,
        scan_server_id: Option<&str>,
    ) -> Result<(), NzbError> {
        self.conn.execute(
            "UPDATE groups SET last_scanned = ?2, scan_server_id = ?3, last_updated = datetime('now')
             WHERE id = ?1",
            params![id, last_scanned, scan_server_id],
        )?;
        Ok(())
    }

    /// Returns the id of the server that produced the current watermark, if known.
    pub fn group_scan_server(&self, id: i64) -> Result<Option<String>, NzbError> {
        match self.conn.query_row(
            "SELECT scan_server_id FROM groups WHERE id = ?1",
            params![id],
            |row| row.get::<_, Option<String>>(0),
        ) {
            Ok(v) => Ok(v),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(NzbError::Database(e)),
        }
    }

    // ---- Headers ----

    pub fn header_insert_batch(
        &self,
        group_id: i64,
        entries: &[nzb_nntp::XoverEntry],
    ) -> Result<u64, NzbError> {
        let tx = self.conn.unchecked_transaction()?;
        let mut count = 0u64;
        {
            let mut stmt = tx.prepare_cached(
                "INSERT OR IGNORE INTO headers (group_id, article_num, subject, author, date, message_id, references_, bytes, lines)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            )?;
            for e in entries {
                count += stmt.execute(params![
                    group_id,
                    e.article_num as i64,
                    e.subject,
                    e.from,
                    e.date,
                    e.message_id,
                    e.references,
                    e.bytes as i64,
                    e.lines as i64,
                ])? as u64;
            }
        }
        tx.commit()?;
        Ok(count)
    }

    pub fn header_list(
        &self,
        group_id: i64,
        search: Option<&str>,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<HeaderRow>, NzbError> {
        let (limit, offset) = (sql_count(limit), sql_count(offset));
        let query = |sql: &str, term: &dyn rusqlite::ToSql| -> rusqlite::Result<Vec<HeaderRow>> {
            self.conn
                .prepare(sql)?
                .query_map(params![group_id, term, limit, offset], map_header_row)?
                .collect()
        };

        match HeaderSearch::new(search) {
            HeaderSearch::All => Ok(query(
                &format!(
                    "SELECT {HEADER_COLUMNS} FROM headers WHERE group_id = ?1 AND ?2 IS NULL
                     ORDER BY article_num DESC LIMIT ?3 OFFSET ?4"
                ),
                &rusqlite::types::Null,
            )?),
            HeaderSearch::Text { phrase, raw } => {
                let fts = query(
                    &format!(
                        "SELECT {HEADER_COLUMNS_H} FROM headers h
                         INNER JOIN headers_fts f ON h.id = f.rowid
                         WHERE h.group_id = ?1 AND headers_fts MATCH ?2
                         ORDER BY rank LIMIT ?3 OFFSET ?4"
                    ),
                    &phrase,
                );
                match fts {
                    Ok(rows) => Ok(rows),
                    // Full-text search unavailable or rejected the query:
                    // fall back to substring matching (as header_count does).
                    Err(_) => Ok(query(
                        &format!(
                            "SELECT {HEADER_COLUMNS} FROM headers WHERE group_id = ?1 AND {LIKE_FILTER}
                             ORDER BY article_num DESC LIMIT ?3 OFFSET ?4"
                        ),
                        &raw,
                    )?),
                }
            }
        }
    }

    /// Number of headers [`Database::header_list`] can page through for the
    /// same `search`, including its substring fallback when full-text
    /// search fails.
    pub fn header_count(&self, group_id: i64, search: Option<&str>) -> Result<i64, NzbError> {
        let count = |sql: &str, term: &dyn rusqlite::ToSql| -> rusqlite::Result<i64> {
            self.conn
                .query_row(sql, params![group_id, term], |row| row.get(0))
        };
        match HeaderSearch::new(search) {
            HeaderSearch::All => Ok(count(
                "SELECT COUNT(*) FROM headers WHERE group_id = ?1 AND ?2 IS NULL",
                &rusqlite::types::Null,
            )?),
            HeaderSearch::Text { phrase, raw } => {
                match count(
                    "SELECT COUNT(*) FROM headers h INNER JOIN headers_fts f ON h.id = f.rowid
                     WHERE h.group_id = ?1 AND headers_fts MATCH ?2",
                    &phrase,
                ) {
                    Ok(n) => Ok(n),
                    Err(_) => Ok(count(
                        &format!(
                            "SELECT COUNT(*) FROM headers WHERE group_id = ?1 AND {LIKE_FILTER}"
                        ),
                        &raw,
                    )?),
                }
            }
        }
    }

    pub fn header_get_by_message_id(
        &self,
        message_id: &str,
    ) -> Result<Option<HeaderRow>, NzbError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, group_id, article_num, subject, author, date, message_id, references_, bytes, lines, read, downloaded_at
             FROM headers WHERE message_id = ?1 LIMIT 1",
        )?;
        let result = stmt.query_row(params![message_id], map_header_row);
        match result {
            Ok(h) => Ok(Some(h)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(NzbError::Database(e)),
        }
    }

    pub fn header_mark_read(&self, header_id: i64) -> Result<(), NzbError> {
        self.conn.execute(
            "UPDATE headers SET read = 1 WHERE id = ?1",
            params![header_id],
        )?;
        Ok(())
    }

    pub fn header_mark_all_read(&self, group_id: i64) -> Result<u64, NzbError> {
        let changes = self.conn.execute(
            "UPDATE headers SET read = 1 WHERE group_id = ?1 AND read = 0",
            params![group_id],
        )?;
        Ok(changes as u64)
    }

    pub fn header_unread_count(&self, group_id: i64) -> Result<i64, NzbError> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM headers WHERE group_id = ?1 AND read = 0",
            params![group_id],
            |row| row.get(0),
        )?;
        Ok(count)
    }

    /// Keeps only the newest `keep` headers in the group; returns rows deleted.
    pub fn header_prune_group(&self, group_id: i64, keep: usize) -> Result<u64, NzbError> {
        if keep == 0 {
            return Ok(0);
        }
        let keep = i64::try_from(keep).unwrap_or(i64::MAX);
        let deleted = self.conn.execute(
            "DELETE FROM headers WHERE group_id = ?1 AND id NOT IN (
                 SELECT id FROM headers WHERE group_id = ?1
                 ORDER BY article_num DESC, id DESC LIMIT ?2
             )",
            params![group_id, keep],
        )?;
        Ok(deleted as u64)
    }

    /// Deletes all headers in the group and resets its scan watermark; returns rows deleted.
    pub fn header_clear_group(&self, group_id: i64) -> Result<u64, NzbError> {
        let tx = self.conn.unchecked_transaction()?;
        let deleted = tx.execute("DELETE FROM headers WHERE group_id = ?1", params![group_id])?;
        tx.execute(
            "UPDATE groups SET last_scanned = 0, scan_server_id = NULL, last_updated = datetime('now')
             WHERE id = ?1",
            params![group_id],
        )?;
        tx.commit()?;
        Ok(deleted as u64)
    }

    // ---- Threading ----
    //
    // A thread is keyed by its root message-id: the first entry of an
    // article's References header, or the article's own message-id when it
    // has none. Grouping, counting and pagination run in SQL so a request
    // never loads the whole group into memory.

    pub fn header_list_threads(
        &self,
        group_id: i64,
        limit: usize,
        offset: usize,
    ) -> Result<(Vec<ThreadSummary>, i64), NzbError> {
        let total: i64 = self.conn.query_row(
            &format!("SELECT COUNT(DISTINCT {THREAD_ROOT}) FROM headers WHERE group_id = ?1"),
            params![group_id],
            |row| row.get(0),
        )?;

        // `MAX(article_num)` makes SQLite take the bare subject/author/date
        // columns from each thread's newest article; they are used when the
        // root article itself is not stored.
        let sql = format!(
            "WITH threads AS (
                 SELECT {THREAD_ROOT} AS root,
                        MAX(article_num) AS newest,
                        subject, author, date,
                        COUNT(*) AS articles,
                        SUM(read = 0) AS unread,
                        MAX(date) AS last_reply_date
                   FROM headers WHERE group_id = ?1
                  GROUP BY root
                  ORDER BY last_reply_date DESC, root
                  LIMIT ?2 OFFSET ?3)
             SELECT t.root,
                    COALESCE(r.subject, t.subject),
                    COALESCE(r.author, t.author),
                    COALESCE(r.date, t.date),
                    t.last_reply_date, t.articles, t.unread
               FROM threads t
               LEFT JOIN headers r ON r.id = (
                    SELECT id FROM headers
                     WHERE message_id = t.root AND group_id = ?1
                     ORDER BY id LIMIT 1)
              ORDER BY t.last_reply_date DESC, t.root"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let page = stmt
            .query_map(
                params![group_id, sql_count(limit), sql_count(offset)],
                |row| {
                    let subject: String = row.get(1)?;
                    Ok(ThreadSummary {
                        root_message_id: row.get(0)?,
                        subject: strip_reply_prefixes(&subject).to_string(),
                        author: row.get(2)?,
                        date: row.get(3)?,
                        last_reply_date: row.get(4)?,
                        reply_count: row.get::<_, i64>(5)? - 1,
                        unread_count: row.get(6)?,
                    })
                },
            )?
            .collect::<Result<Vec<_>, _>>()?;
        Ok((page, total))
    }

    pub fn header_get_thread(
        &self,
        group_id: i64,
        root_message_id: &str,
    ) -> Result<Vec<ThreadArticle>, NzbError> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {HEADER_COLUMNS} FROM headers
              WHERE group_id = ?1 AND {THREAD_ROOT} = ?2
              ORDER BY article_num DESC"
        ))?;
        let rows = stmt
            .query_map(params![group_id, root_message_id], map_header_row)?
            .map(|row| {
                row.map(|header| {
                    let depth = header.references_.split_whitespace().count() as i32;
                    ThreadArticle { header, depth }
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }
}

/// Group filter shared by `group_list` and `group_count`; binds `?1` =
/// subscribed-only flag, `?2` = optional name substring.
const GROUP_FILTER: &str =
    "(?1 = 0 OR subscribed = 1) AND (?2 IS NULL OR instr(lower(name), lower(?2)) > 0)";

/// Substring filter for the non-FTS header search; binds `?2`.
const LIKE_FILTER: &str =
    "(instr(lower(subject), lower(?2)) > 0 OR instr(lower(author), lower(?2)) > 0)";

const HEADER_COLUMNS: &str = "id, group_id, article_num, subject, author, date, message_id, \
     references_, bytes, lines, read, downloaded_at";

const HEADER_COLUMNS_H: &str = "h.id, h.group_id, h.article_num, h.subject, h.author, h.date, \
     h.message_id, h.references_, h.bytes, h.lines, h.read, h.downloaded_at";

/// SQL expression for an article's thread root: the first References entry,
/// or its own message-id.
const THREAD_ROOT: &str = "CASE WHEN trim(references_) = '' THEN message_id \
     ELSE substr(trim(references_), 1, instr(trim(references_) || ' ', ' ') - 1) END";

/// Clamp a pagination value to SQLite's signed 64-bit integer range.
fn sql_count(value: usize) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

fn map_header_row(row: &rusqlite::Row) -> rusqlite::Result<HeaderRow> {
    Ok(HeaderRow {
        id: row.get(0)?,
        group_id: row.get(1)?,
        article_num: row.get(2)?,
        subject: row.get(3)?,
        author: row.get(4)?,
        date: row.get(5)?,
        message_id: row.get(6)?,
        references_: row.get(7)?,
        bytes: row.get(8)?,
        lines: row.get(9)?,
        read: row.get::<_, i32>(10)? != 0,
        downloaded_at: row.get::<_, Option<String>>(11)?.unwrap_or_default(),
    })
}

fn strip_reply_prefixes(mut subject: &str) -> &str {
    while let Some(rest) = subject
        .strip_prefix("Re: ")
        .or_else(|| subject.strip_prefix("RE: "))
    {
        subject = rest;
    }
    subject
}

/// How a header search is executed.
enum HeaderSearch {
    All,
    /// `phrase` is the FTS5 phrase query, `raw` the substring for the
    /// fallback.
    Text {
        phrase: String,
        raw: String,
    },
}

impl HeaderSearch {
    fn new(search: Option<&str>) -> Self {
        let Some(search) = search else {
            return Self::All;
        };
        let safe = search.replace(['"', '\''], "").trim().to_string();
        if safe.is_empty() {
            return Self::All;
        }
        Self::Text {
            phrase: format!("\"{safe}\""),
            raw: search.to_string(),
        }
    }
}

#[cfg(all(test, feature = "groups-db"))]
mod tests {
    use super::*;
    use nzb_nntp::XoverEntry;

    #[test]
    fn groups_headers_and_threads_persist_with_read_state() {
        let db = Database::open_memory().unwrap();
        db.group_upsert_batch(&[("alt.binaries.test".into(), 30, 10)])
            .unwrap();
        let group = db.group_list(false, None, 10, 0).unwrap().pop().unwrap();
        db.group_set_subscribed(group.id, true).unwrap();
        db.header_insert_batch(
            group.id,
            &[
                XoverEntry {
                    article_num: 10,
                    subject: "Release".into(),
                    from: "poster".into(),
                    date: "2026-01-01".into(),
                    message_id: "root@test".into(),
                    references: "".into(),
                    bytes: 42,
                    lines: 1,
                },
                XoverEntry {
                    article_num: 11,
                    subject: "Re: Release".into(),
                    from: "reply".into(),
                    date: "2026-01-02".into(),
                    message_id: "reply@test".into(),
                    references: "root@test".into(),
                    bytes: 84,
                    lines: 2,
                },
            ],
        )
        .unwrap();

        assert_eq!(db.header_unread_count(group.id).unwrap(), 2);
        let headers = db.header_list(group.id, Some("Release"), 10, 0).unwrap();
        assert_eq!(headers.len(), 2);
        db.header_mark_read(headers[0].id).unwrap();
        assert_eq!(db.header_unread_count(group.id).unwrap(), 1);
        let (threads, total) = db.header_list_threads(group.id, 10, 0).unwrap();
        assert_eq!(total, 1);
        assert_eq!(threads[0].root_message_id, "root@test");
        assert_eq!(threads[0].reply_count, 1);
        assert_eq!(threads[0].unread_count, 1);
        assert_eq!(db.group_get(group.id).unwrap().unwrap().unread_count, 1);
    }

    fn entry(article_num: u64, message_id: &str, references: &str) -> XoverEntry {
        XoverEntry {
            article_num,
            subject: format!("Subject {article_num}"),
            from: "poster".into(),
            date: format!("2026-01-01 {article_num:08}"),
            message_id: message_id.into(),
            references: references.into(),
            bytes: 1,
            lines: 1,
        }
    }

    #[test]
    fn header_insert_batch_dedupes_by_message_id() {
        let db = Database::open_memory().unwrap();
        db.group_upsert_batch(&[("alt.binaries.test".into(), 30, 10)])
            .unwrap();
        let group = db.group_list(false, None, 10, 0).unwrap().pop().unwrap();

        db.header_insert_batch(group.id, &[entry(10, "a@test", "")])
            .unwrap();
        let inserted = db
            .header_insert_batch(
                group.id,
                &[entry(10, "a@test", ""), entry(11, "b@test", "")],
            )
            .unwrap();

        assert_eq!(inserted, 1);
        assert_eq!(db.header_count(group.id, None).unwrap(), 2);
    }

    #[test]
    fn header_prune_group_keeps_newest() {
        let db = Database::open_memory().unwrap();
        db.group_upsert_batch(&[("alt.binaries.test".into(), 30, 10)])
            .unwrap();
        let group = db.group_list(false, None, 10, 0).unwrap().pop().unwrap();
        db.header_insert_batch(
            group.id,
            &[
                entry(10, "a@test", ""),
                entry(11, "b@test", ""),
                entry(12, "c@test", ""),
            ],
        )
        .unwrap();

        let deleted = db.header_prune_group(group.id, 2).unwrap();

        assert_eq!(deleted, 1);
        assert_eq!(db.header_count(group.id, None).unwrap(), 2);
        assert!(db.header_get_by_message_id("b@test").unwrap().is_some());
        assert!(db.header_get_by_message_id("c@test").unwrap().is_some());
        assert!(db.header_get_by_message_id("a@test").unwrap().is_none());
        assert_eq!(db.header_prune_group(group.id, 0).unwrap(), 0);
    }

    #[test]
    fn header_clear_group_deletes_headers_and_resets_watermark() {
        let db = Database::open_memory().unwrap();
        db.group_upsert_batch(&[("alt.binaries.test".into(), 30, 10)])
            .unwrap();
        let group = db.group_list(false, None, 10, 0).unwrap().pop().unwrap();
        db.header_insert_batch(group.id, &[entry(10, "a@test", "")])
            .unwrap();
        db.group_update_watermark(group.id, 42, Some("srv-1"))
            .unwrap();

        let deleted = db.header_clear_group(group.id).unwrap();

        assert_eq!(deleted, 1);
        assert_eq!(db.header_count(group.id, None).unwrap(), 0);
        let updated = db.group_get(group.id).unwrap().unwrap();
        assert_eq!(updated.last_scanned, 0);
        assert!(db.group_scan_server(group.id).unwrap().is_none());
    }

    #[test]
    fn group_watermark_remembers_scan_server() {
        let db = Database::open_memory().unwrap();
        db.group_upsert_batch(&[("alt.binaries.test".into(), 30, 10)])
            .unwrap();
        let group = db.group_list(false, None, 10, 0).unwrap().pop().unwrap();

        db.group_update_watermark(group.id, 100, Some("srv-a"))
            .unwrap();
        assert_eq!(
            db.group_scan_server(group.id).unwrap().as_deref(),
            Some("srv-a")
        );

        db.group_update_watermark(group.id, 200, None).unwrap();
        assert_eq!(db.group_scan_server(group.id).unwrap(), None);
    }

    fn seeded_group(db: &Database) -> i64 {
        db.group_upsert_batch(&[("alt.binaries.test".into(), 30, 10)])
            .unwrap();
        db.group_list(false, None, 10, 0).unwrap().pop().unwrap().id
    }

    /// BUG-111: limit/offset beyond i64 used to be formatted into the SQL and
    /// fail with "datatype mismatch".
    #[test]
    fn huge_limits_and_offsets_do_not_fail() {
        let db = Database::open_memory().unwrap();
        let group_id = seeded_group(&db);
        db.header_insert_batch(group_id, &[entry(1, "<a@test>", "")])
            .unwrap();

        assert_eq!(db.group_list(false, None, usize::MAX, 0).unwrap().len(), 1);
        assert!(
            db.group_list(false, None, 10, usize::MAX)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            db.header_list(group_id, None, usize::MAX, 0).unwrap().len(),
            1
        );
        assert_eq!(
            db.header_list(group_id, Some("Subject"), usize::MAX, 0)
                .unwrap()
                .len(),
            1
        );
        assert!(
            db.header_list(group_id, None, 10, usize::MAX)
                .unwrap()
                .is_empty()
        );
        let (threads, total) = db.header_list_threads(group_id, usize::MAX, 0).unwrap();
        assert_eq!((threads.len(), total), (1, 1));
    }

    /// BUG-111: search text is bound, not spliced into the SQL.
    #[test]
    fn search_text_is_not_interpreted_as_sql() {
        let db = Database::open_memory().unwrap();
        let group_id = seeded_group(&db);
        db.header_insert_batch(group_id, &[entry(1, "<a@test>", "")])
            .unwrap();
        for search in ["%", "' OR 1=1 --", "x' OR name LIKE '%"] {
            assert!(
                db.group_list(false, Some(search), 10, 0).is_ok(),
                "{search}"
            );
            assert!(db.header_list(group_id, Some(search), 10, 0).is_ok());
        }
        assert_eq!(db.group_count(false, Some("' OR 1=1 --")).unwrap(), 0);
        assert!(
            db.group_list(false, Some("' OR 1=1 --"), 10, 0)
                .unwrap()
                .is_empty()
        );
    }

    /// BUG-111: when full-text search is unusable, header_list falls back to
    /// LIKE; header_count must count the same rows instead of reporting 0.
    #[test]
    fn header_count_agrees_with_header_list_when_fts_is_unavailable() {
        let db = Database::open_memory().unwrap();
        let group_id = seeded_group(&db);
        db.header_insert_batch(
            group_id,
            &[entry(1, "<a@test>", ""), entry(2, "<b@test>", "")],
        )
        .unwrap();
        db.conn
            .execute_batch(
                "DROP TRIGGER headers_fts_ins; DROP TRIGGER headers_fts_del; DROP TABLE headers_fts;",
            )
            .unwrap();

        let listed = db.header_list(group_id, Some("Subject"), 10, 0).unwrap();
        assert_eq!(listed.len(), 2);
        assert_eq!(db.header_count(group_id, Some("Subject")).unwrap(), 2);
    }

    /// BUG-108: thread totals are exact past the old 100,000-row load cap.
    #[test]
    fn thread_totals_are_exact_beyond_one_hundred_thousand_headers() {
        let db = Database::open_memory().unwrap();
        let group_id = seeded_group(&db);
        // Bulk-insert in SQL; going through header_insert_batch row by row
        // makes this test needlessly slow in debug builds.
        db.conn
            .execute(
                "WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM n WHERE x < 100002)
                 INSERT INTO headers (group_id, article_num, subject, author, date, message_id)
                 SELECT ?1, x, 'Subject ' || x, 'poster', printf('2026-01-01 %08d', x), '<' || x || '@test>'
                   FROM n",
                params![group_id],
            )
            .unwrap();

        let (page, total) = db.header_list_threads(group_id, 2, 0).unwrap();
        assert_eq!(total, 100_002);
        assert_eq!(page.len(), 2);
        assert_eq!(page[0].root_message_id, "<100002@test>");
    }

    /// BUG-108: aggregation in SQL keeps the thread summary semantics.
    #[test]
    fn thread_summaries_aggregate_replies_and_paginate() {
        let db = Database::open_memory().unwrap();
        let group_id = seeded_group(&db);
        db.header_insert_batch(
            group_id,
            &[
                entry(1, "<r1@test>", ""),
                entry(2, "<r2@test>", ""),
                entry(3, "<c1@test>", "<r1@test>"),
                entry(4, "<c2@test>", "<r1@test> <c1@test>"),
                // Reply whose root is not stored: the thread is keyed by the
                // referenced root and summarised from its newest article.
                entry(5, "<orphan@test>", "<missing@test>"),
            ],
        )
        .unwrap();
        let read = db.header_get_by_message_id("<c1@test>").unwrap().unwrap();
        db.header_mark_read(read.id).unwrap();

        let (threads, total) = db.header_list_threads(group_id, 10, 0).unwrap();
        assert_eq!(total, 3);
        let roots: Vec<_> = threads.iter().map(|t| t.root_message_id.as_str()).collect();
        assert_eq!(roots, ["<missing@test>", "<r1@test>", "<r2@test>"]);
        let r1 = &threads[1];
        assert_eq!(r1.subject, "Subject 1");
        assert_eq!(r1.reply_count, 2);
        assert_eq!(r1.unread_count, 2);
        assert_eq!(r1.last_reply_date, "2026-01-01 00000004");
        assert_eq!(threads[0].subject, "Subject 5");
        assert_eq!(threads[0].reply_count, 0);

        let (page, total) = db.header_list_threads(group_id, 1, 1).unwrap();
        assert_eq!(total, 3);
        assert_eq!(page[0].root_message_id, "<r1@test>");

        let thread = db.header_get_thread(group_id, "<r1@test>").unwrap();
        let mut members: Vec<_> = thread
            .iter()
            .map(|a| (a.header.message_id.as_str(), a.depth))
            .collect();
        members.sort();
        assert_eq!(
            members,
            [("<c1@test>", 1), ("<c2@test>", 2), ("<r1@test>", 0)]
        );
    }
}
