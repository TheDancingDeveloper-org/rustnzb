//! Database operations for newsgroup browsing.

use std::collections::HashMap;

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
        let mut sql = String::from(
            "SELECT g.id, g.name, g.description, g.subscribed, g.article_count,
             g.first_article, g.last_article, g.last_scanned, g.last_updated, g.created_at,
             (SELECT COUNT(*) FROM headers h WHERE h.group_id = g.id AND h.read = 0) as unread_count
             FROM groups g WHERE 1=1",
        );
        if subscribed_only {
            sql.push_str(" AND g.subscribed = 1");
        }
        if let Some(s) = search {
            sql.push_str(&format!(" AND g.name LIKE '%{}%'", s.replace('\'', "''")));
        }
        sql.push_str(&format!(" ORDER BY g.name LIMIT {limit} OFFSET {offset}"));

        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt
            .query_map([], |row| {
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
            })?
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
        let mut sql = String::from("SELECT COUNT(*) FROM groups WHERE 1=1");
        if subscribed_only {
            sql.push_str(" AND subscribed = 1");
        }
        if let Some(s) = search {
            sql.push_str(&format!(" AND name LIKE '%{}%'", s.replace('\'', "''")));
        }
        let count: i64 = self.conn.query_row(&sql, [], |row| row.get(0))?;
        Ok(count)
    }

    pub fn group_set_subscribed(&self, id: i64, subscribed: bool) -> Result<(), NzbError> {
        self.conn.execute(
            "UPDATE groups SET subscribed = ?2 WHERE id = ?1",
            params![id, subscribed as i32],
        )?;
        Ok(())
    }

    pub fn group_update_watermark(&self, id: i64, last_scanned: i64) -> Result<(), NzbError> {
        self.conn.execute(
            "UPDATE groups SET last_scanned = ?2, last_updated = datetime('now') WHERE id = ?1",
            params![id, last_scanned],
        )?;
        Ok(())
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
                stmt.execute(params![
                    group_id,
                    e.article_num as i64,
                    e.subject,
                    e.from,
                    e.date,
                    e.message_id,
                    e.references,
                    e.bytes as i64,
                    e.lines as i64,
                ])?;
                count += 1;
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
        let (sql, use_fts) = if let Some(s) = search {
            let safe = s.replace(['"', '\''], "").trim().to_string();
            if safe.is_empty() {
                (format!(
                    "SELECT id, group_id, article_num, subject, author, date, message_id, references_, bytes, lines, read, downloaded_at
                     FROM headers WHERE group_id = ?1 ORDER BY article_num DESC LIMIT {limit} OFFSET {offset}"
                ), false)
            } else {
                (format!(
                    "SELECT h.id, h.group_id, h.article_num, h.subject, h.author, h.date, h.message_id, h.references_, h.bytes, h.lines, h.read, h.downloaded_at
                     FROM headers h INNER JOIN headers_fts f ON h.id = f.rowid
                     WHERE h.group_id = ?1 AND headers_fts MATCH '\"{safe}\"'
                     ORDER BY rank LIMIT {limit} OFFSET {offset}"
                ), true)
            }
        } else {
            (format!(
                "SELECT id, group_id, article_num, subject, author, date, message_id, references_, bytes, lines, read, downloaded_at
                 FROM headers WHERE group_id = ?1 ORDER BY article_num DESC LIMIT {limit} OFFSET {offset}"
            ), false)
        };

        let map_row = |row: &rusqlite::Row| -> rusqlite::Result<HeaderRow> {
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
        };

        let result = self
            .conn
            .prepare(&sql)
            .and_then(|mut stmt| stmt.query_map(params![group_id], map_row)?.collect());

        match result {
            Ok(rows) => Ok(rows),
            Err(_) if use_fts => {
                let s = search.unwrap_or("");
                let fallback = format!(
                    "SELECT id, group_id, article_num, subject, author, date, message_id, references_, bytes, lines, read, downloaded_at
                     FROM headers WHERE group_id = ?1 AND (subject LIKE '%{0}%' OR author LIKE '%{0}%')
                     ORDER BY article_num DESC LIMIT {1} OFFSET {2}",
                    s.replace('\'', "''"),
                    limit,
                    offset
                );
                let mut stmt = self.conn.prepare(&fallback)?;
                let rows = stmt
                    .query_map(params![group_id], map_row)?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(rows)
            }
            Err(e) => Err(NzbError::Database(e)),
        }
    }

    pub fn header_count(&self, group_id: i64, search: Option<&str>) -> Result<i64, NzbError> {
        let sql = if let Some(s) = search {
            let safe = s.replace(['"', '\''], "").trim().to_string();
            if safe.is_empty() {
                "SELECT COUNT(*) FROM headers WHERE group_id = ?1".to_string()
            } else {
                format!(
                    "SELECT COUNT(*) FROM headers h INNER JOIN headers_fts f ON h.id = f.rowid
                     WHERE h.group_id = ?1 AND headers_fts MATCH '\"{safe}\"'"
                )
            }
        } else {
            "SELECT COUNT(*) FROM headers WHERE group_id = ?1".to_string()
        };

        let count: i64 = self
            .conn
            .query_row(&sql, params![group_id], |row| row.get(0))
            .unwrap_or(0);
        Ok(count)
    }

    pub fn header_get_by_message_id(
        &self,
        message_id: &str,
    ) -> Result<Option<HeaderRow>, NzbError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, group_id, article_num, subject, author, date, message_id, references_, bytes, lines, read, downloaded_at
             FROM headers WHERE message_id = ?1 LIMIT 1",
        )?;
        let result = stmt.query_row(params![message_id], |row| {
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
        });
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

    // ---- Threading ----

    pub fn header_list_threads(
        &self,
        group_id: i64,
        limit: usize,
        offset: usize,
    ) -> Result<(Vec<ThreadSummary>, i64), NzbError> {
        let all = self.header_list(group_id, None, 100_000, 0)?;

        let mut msg_to_root: HashMap<String, String> = HashMap::new();
        let mut root_threads: HashMap<String, Vec<&HeaderRow>> = HashMap::new();

        for h in &all {
            let root_id = if h.references_.is_empty() {
                h.message_id.clone()
            } else {
                let first_ref = h
                    .references_
                    .split_whitespace()
                    .next()
                    .unwrap_or(&h.message_id);
                msg_to_root
                    .get(first_ref)
                    .cloned()
                    .unwrap_or_else(|| first_ref.to_string())
            };
            msg_to_root.insert(h.message_id.clone(), root_id.clone());
            root_threads.entry(root_id).or_default().push(h);
        }

        let mut summaries: Vec<ThreadSummary> = root_threads
            .iter()
            .map(|(root_id, articles)| {
                let root = articles
                    .iter()
                    .find(|a| a.message_id == *root_id)
                    .unwrap_or(&articles[0]);
                let last = articles.iter().max_by_key(|a| &a.date).unwrap_or(root);
                let mut subject = root.subject.as_str();
                while let Some(rest) = subject
                    .strip_prefix("Re: ")
                    .or_else(|| subject.strip_prefix("RE: "))
                {
                    subject = rest;
                }
                ThreadSummary {
                    root_message_id: root_id.clone(),
                    subject: subject.to_string(),
                    author: root.author.clone(),
                    date: root.date.clone(),
                    last_reply_date: last.date.clone(),
                    reply_count: (articles.len() as i64) - 1,
                    unread_count: articles.iter().filter(|a| !a.read).count() as i64,
                }
            })
            .collect();

        summaries.sort_by(|a, b| b.last_reply_date.cmp(&a.last_reply_date));
        let total = summaries.len() as i64;
        let page: Vec<ThreadSummary> = summaries.into_iter().skip(offset).take(limit).collect();
        Ok((page, total))
    }

    pub fn header_get_thread(
        &self,
        group_id: i64,
        root_message_id: &str,
    ) -> Result<Vec<ThreadArticle>, NzbError> {
        let all = self.header_list(group_id, None, 100_000, 0)?;

        let mut msg_to_root: HashMap<String, String> = HashMap::new();
        for h in &all {
            let root_id = if h.references_.is_empty() {
                h.message_id.clone()
            } else {
                let first_ref = h
                    .references_
                    .split_whitespace()
                    .next()
                    .unwrap_or(&h.message_id);
                msg_to_root
                    .get(first_ref)
                    .cloned()
                    .unwrap_or_else(|| first_ref.to_string())
            };
            msg_to_root.insert(h.message_id.clone(), root_id);
        }

        let result: Vec<ThreadArticle> = all
            .into_iter()
            .filter(|h| {
                msg_to_root
                    .get(&h.message_id)
                    .is_some_and(|r| r == root_message_id)
            })
            .map(|h| {
                let depth = if h.references_.is_empty() {
                    0
                } else {
                    h.references_.split_whitespace().count() as i32
                };
                ThreadArticle { header: h, depth }
            })
            .collect();

        Ok(result)
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
}
