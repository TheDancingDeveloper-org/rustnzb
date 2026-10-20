//! Per-article damage ledger (WI-143).
//!
//! One durable record per article says whether it is `present`, missing, or
//! not yet established, together with the per-server evidence behind that
//! verdict. Both the download engine and the streaming provider consult and
//! update this ledger, so a hole learned once is never rediscovered.
//!
//! The rules that decide a verdict live in exactly one function,
//! [`apply_outcome`], and follow zurg's `internal/nzb/holes.go`:
//!
//! - A transport failure (timeout, auth error, pool exhaustion, decode error)
//!   NEVER creates or refreshes a missing record. Only a `430` does.
//! - A single `430` is provisional. It confirms only once every enabled server
//!   has refused the article and at least [`HOLE_CONFIRM_DELAY_SECS`] seconds
//!   have passed since the first refusal — a provider is known to answer `430`
//!   for an article it holds while busy.
//! - A confirmed hole is believed for [`HOLE_TTL_SECS`]; after that a caller
//!   re-probes.
//! - The record is keyed to the set of servers that were asked
//!   ([`server_fingerprint`]); adding or removing a provider invalidates it.
//!
//! This crate does not depend on the dispatcher, so callers translate their
//! own failure taxonomy into an [`Outcome`] before calling [`Database::ledger_note`]:
//! a `2xx` body is [`Outcome::Present`], a `430` is [`Outcome::Refused`], and
//! every other failure is [`Outcome::Transport`].

use std::collections::BTreeSet;

use chrono::{DateTime, Duration, Utc};
use rusqlite::params;
use serde::{Deserialize, Serialize};

use crate::db::Database;
use crate::error::NzbError;

/// How long a confirmed hole is believed before a re-probe (zurg `holeTTL`).
pub const HOLE_TTL_SECS: i64 = 24 * 60 * 60;
/// Minimum gap between the first `430` and the confirming `430`
/// (zurg `holeConfirmDelay`).
pub const HOLE_CONFIRM_DELAY_SECS: i64 = 1;

/// The verdict recorded for one article.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LedgerState {
    /// Never asked, or only inconclusive (transport) answers so far.
    Unknown,
    /// One or more `430`s, but not yet confirmed missing everywhere.
    ProvisionalMissing,
    /// Every enabled server refused it and the busy-provider guard elapsed.
    ConfirmedMissing,
    /// A definite `2xx` body was seen; the article is present.
    Present,
    /// The file this article belongs to is past the unservable threshold.
    UnservableFile,
}

impl LedgerState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::ProvisionalMissing => "provisional_missing",
            Self::ConfirmedMissing => "confirmed_missing",
            Self::Present => "present",
            Self::UnservableFile => "unservable_file",
        }
    }

    fn from_str(s: &str) -> Self {
        match s {
            "provisional_missing" => Self::ProvisionalMissing,
            "confirmed_missing" => Self::ConfirmedMissing,
            "present" => Self::Present,
            "unservable_file" => Self::UnservableFile,
            _ => Self::Unknown,
        }
    }
}

/// Identity of one article within a job or streamed item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArticleKey {
    pub scope_id: String,
    pub file_index: u32,
    pub segment_number: u32,
}

impl ArticleKey {
    pub fn new(scope_id: impl Into<String>, file_index: u32, segment_number: u32) -> Self {
        Self {
            scope_id: scope_id.into(),
            file_index,
            segment_number,
        }
    }
}

/// The result of a single fetch attempt, in the ledger's own vocabulary.
/// Callers map their transport/decode/auth errors onto [`Outcome::Transport`]
/// and only a real `430` onto [`Outcome::Refused`].
#[derive(Debug, Clone)]
pub enum Outcome {
    Present,
    Refused { server: String },
    Transport { server: String },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum EvidenceKind {
    Refused,
    Transport,
}

/// What one server last said about this article.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ServerEvidence {
    pub server: String,
    pub kind: EvidenceKind,
    pub at: DateTime<Utc>,
}

/// A persisted per-article damage record.
#[derive(Debug, Clone)]
pub struct LedgerRecord {
    pub key: ArticleKey,
    pub message_id: String,
    pub state: LedgerState,
    pub evidence: Vec<ServerEvidence>,
    pub first_refused_at: Option<DateTime<Utc>>,
    pub confirmed_at: Option<DateTime<Utc>>,
    pub expires_at: Option<DateTime<Utc>>,
    pub server_fingerprint: String,
}

impl LedgerRecord {
    fn new(key: ArticleKey, message_id: String) -> Self {
        Self {
            key,
            message_id,
            state: LedgerState::Unknown,
            evidence: Vec::new(),
            first_refused_at: None,
            confirmed_at: None,
            expires_at: None,
            server_fingerprint: String::new(),
        }
    }

    fn refused_servers(&self) -> BTreeSet<&str> {
        self.evidence
            .iter()
            .filter(|e| e.kind == EvidenceKind::Refused)
            .map(|e| e.server.as_str())
            .collect()
    }
}

/// Whether the caller should schedule a confirming re-ask before trusting a
/// provisional refusal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConfirmNeeded(pub bool);

/// Guidance from [`Database::ledger_consult`] before fetching an article.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Consult {
    /// The article is a confirmed hole within its TTL — do not fetch it.
    pub skip: bool,
    /// The confirmed hole's TTL has expired — one caller should re-probe.
    pub reprobe: bool,
}

/// A stable fingerprint of the set of servers that were asked. Adding or
/// removing a provider changes it, which invalidates any prior verdict.
pub fn server_fingerprint(servers: &[String]) -> String {
    let mut set: Vec<&str> = servers.iter().map(String::as_str).collect();
    set.sort_unstable();
    set.dedup();
    set.join(",")
}

fn record_evidence(rec: &mut LedgerRecord, server: &str, kind: EvidenceKind, now: DateTime<Utc>) {
    if let Some(existing) = rec.evidence.iter_mut().find(|e| e.server == server) {
        existing.kind = kind;
        existing.at = now;
    } else {
        rec.evidence.push(ServerEvidence {
            server: server.to_string(),
            kind,
            at: now,
        });
    }
}

/// The single place that decides how one outcome mutates a record. Every rule
/// about what may be recorded as missing lives here and nowhere else. Returns
/// whether a confirming re-ask is still needed.
pub fn apply_outcome(
    rec: &mut LedgerRecord,
    outcome: &Outcome,
    now: DateTime<Utc>,
    enabled_servers: &[String],
) -> ConfirmNeeded {
    let fingerprint = server_fingerprint(enabled_servers);
    if rec.server_fingerprint != fingerprint {
        // The set of servers asked changed; a hole keyed to the old set is no
        // longer trustworthy. Start the record over.
        rec.state = LedgerState::Unknown;
        rec.evidence.clear();
        rec.first_refused_at = None;
        rec.confirmed_at = None;
        rec.expires_at = None;
        rec.server_fingerprint = fingerprint;
    }

    match outcome {
        Outcome::Present => {
            // A definite present answer is terminal and clears any hole.
            rec.state = LedgerState::Present;
            rec.first_refused_at = None;
            rec.confirmed_at = None;
            rec.expires_at = None;
            ConfirmNeeded(false)
        }
        Outcome::Transport { server } => {
            // A transport failure is inconclusive: record who was unreachable,
            // but never let it create or advance a missing verdict.
            record_evidence(rec, server, EvidenceKind::Transport, now);
            ConfirmNeeded(false)
        }
        Outcome::Refused { server } => {
            record_evidence(rec, server, EvidenceKind::Refused, now);

            // Present is sticky (we already hold the bytes); a confirmed hole
            // stays confirmed until its TTL is handled by consult().
            if matches!(
                rec.state,
                LedgerState::Present | LedgerState::ConfirmedMissing
            ) {
                return ConfirmNeeded(false);
            }

            if rec.first_refused_at.is_none() {
                rec.first_refused_at = Some(now);
            }
            rec.state = LedgerState::ProvisionalMissing;

            let refused = rec.refused_servers();
            let all_refused = !enabled_servers.is_empty()
                && enabled_servers.iter().all(|s| refused.contains(s.as_str()));
            let elapsed_ok = rec
                .first_refused_at
                .is_some_and(|first| now - first >= Duration::seconds(HOLE_CONFIRM_DELAY_SECS));

            if all_refused && elapsed_ok {
                rec.state = LedgerState::ConfirmedMissing;
                rec.confirmed_at = Some(now);
                rec.expires_at = Some(now + Duration::seconds(HOLE_TTL_SECS));
                ConfirmNeeded(false)
            } else {
                ConfirmNeeded(true)
            }
        }
    }
}

/// Whether a fetch should be skipped or re-probed given the current record.
pub fn consult(rec: Option<&LedgerRecord>, now: DateTime<Utc>) -> Consult {
    match rec.map(|r| (r.state, r.expires_at)) {
        Some((LedgerState::ConfirmedMissing | LedgerState::UnservableFile, expires)) => {
            match expires {
                Some(exp) if now >= exp => Consult {
                    skip: false,
                    reprobe: true,
                },
                _ => Consult {
                    skip: true,
                    reprobe: false,
                },
            }
        }
        _ => Consult {
            skip: false,
            reprobe: false,
        },
    }
}

/// Whether a reader blocked on this article is waiting on a provisional
/// refusal that a confirm may yet clear.
pub fn held_provisionally(rec: Option<&LedgerRecord>) -> bool {
    matches!(rec.map(|r| r.state), Some(LedgerState::ProvisionalMissing))
}

fn parse_dt(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|d| d.with_timezone(&Utc))
}

impl Database {
    /// Record one fetch outcome for an article and return whether a confirming
    /// re-ask is still needed. `enabled_servers` is the full set of servers
    /// this fetch could have used; it fingerprints the record.
    pub fn ledger_note(
        &self,
        key: &ArticleKey,
        message_id: &str,
        outcome: &Outcome,
        enabled_servers: &[String],
        now: DateTime<Utc>,
    ) -> Result<ConfirmNeeded, NzbError> {
        let mut rec = self
            .ledger_get(key)?
            .unwrap_or_else(|| LedgerRecord::new(key.clone(), message_id.to_string()));
        if rec.message_id.is_empty() {
            rec.message_id = message_id.to_string();
        }
        let confirm = apply_outcome(&mut rec, outcome, now, enabled_servers);
        self.ledger_upsert(&rec)?;
        Ok(confirm)
    }

    /// Record many fetch outcomes in one transaction, so a batch costs one
    /// commit rather than one per article. Each note carries the time its
    /// outcome was observed. Returns each outcome's [`ConfirmNeeded`], in
    /// order. Outcomes for the same article apply in the order given.
    pub fn ledger_note_batch(
        &self,
        notes: &[(ArticleKey, String, Outcome, DateTime<Utc>)],
        enabled_servers: &[String],
    ) -> Result<Vec<ConfirmNeeded>, NzbError> {
        let tx = self.conn.unchecked_transaction()?;
        let mut confirms = Vec::with_capacity(notes.len());
        for (key, message_id, outcome, at) in notes {
            confirms.push(self.ledger_note(key, message_id, outcome, enabled_servers, *at)?);
        }
        tx.commit()?;
        Ok(confirms)
    }

    /// Guidance before fetching an article: skip a live hole, re-probe an
    /// expired one, otherwise fetch normally.
    pub fn ledger_consult(
        &self,
        key: &ArticleKey,
        now: DateTime<Utc>,
    ) -> Result<Consult, NzbError> {
        Ok(consult(self.ledger_get(key)?.as_ref(), now))
    }

    /// Whether the article is currently held on a provisional refusal.
    pub fn ledger_held_provisionally(&self, key: &ArticleKey) -> Result<bool, NzbError> {
        Ok(held_provisionally(self.ledger_get(key)?.as_ref()))
    }

    /// Fraction of a file's recorded articles that are confirmed gone
    /// (confirmed_missing or an unservable file). Returns 0.0 with no records.
    pub fn ledger_file_gone_ratio(&self, scope_id: &str, file_index: u32) -> Result<f64, NzbError> {
        let total: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM damage_ledger WHERE scope_id = ?1 AND file_index = ?2",
            params![scope_id, file_index as i64],
            |row| row.get(0),
        )?;
        if total == 0 {
            return Ok(0.0);
        }
        let gone: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM damage_ledger
             WHERE scope_id = ?1 AND file_index = ?2
               AND state IN ('confirmed_missing', 'unservable_file')",
            params![scope_id, file_index as i64],
            |row| row.get(0),
        )?;
        Ok(gone as f64 / total as f64)
    }

    /// All ledger records for one file, ordered by segment number.
    pub fn ledger_load_file(
        &self,
        scope_id: &str,
        file_index: u32,
    ) -> Result<Vec<LedgerRecord>, NzbError> {
        let mut stmt = self.conn.prepare(
            "SELECT segment_number, message_id, state, evidence,
                    first_refused_at, confirmed_at, expires_at, server_fingerprint
             FROM damage_ledger
             WHERE scope_id = ?1 AND file_index = ?2
             ORDER BY segment_number",
        )?;
        let rows = stmt.query_map(params![scope_id, file_index as i64], |row| {
            let segment_number = row.get::<_, i64>(0)? as u32;
            let message_id: String = row.get(1)?;
            let state: String = row.get(2)?;
            let evidence_json: String = row.get::<_, Option<String>>(3)?.unwrap_or_default();
            let first: Option<String> = row.get(4)?;
            let confirmed: Option<String> = row.get(5)?;
            let expires: Option<String> = row.get(6)?;
            let fingerprint: String = row.get(7)?;
            Ok(LedgerRecord {
                key: ArticleKey::new(scope_id, file_index, segment_number),
                message_id,
                state: LedgerState::from_str(&state),
                evidence: serde_json::from_str(&evidence_json).unwrap_or_default(),
                first_refused_at: first.as_deref().and_then(parse_dt),
                confirmed_at: confirmed.as_deref().and_then(parse_dt),
                expires_at: expires.as_deref().and_then(parse_dt),
                server_fingerprint: fingerprint,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Drop ledger rows nothing can use any more, and return how many went.
    ///
    /// In this database a `scope_id` is a job id. A job in the queue keeps
    /// every row. Otherwise rows go when the job is in neither the queue nor
    /// history (removed, or aged out), and confirmed holes past `expires_at`
    /// go even while the job is still in history, since a re-probe would
    /// ignore them anyway. A retry runs under a new job id, so it never
    /// depends on the rows of the history entry it came from.
    pub fn ledger_prune(&self, now: DateTime<Utc>) -> Result<usize, NzbError> {
        Ok(self.conn.execute(
            "DELETE FROM damage_ledger
             WHERE scope_id NOT IN (SELECT id FROM queue)
               AND (scope_id NOT IN (SELECT id FROM history)
                    OR (state = 'confirmed_missing'
                        AND expires_at IS NOT NULL
                        AND julianday(expires_at) <= julianday(?1)))",
            params![now.to_rfc3339()],
        )?)
    }

    /// Drop the rows of jobs that are in neither the queue nor history.
    pub(crate) fn ledger_drop_orphans(&self) -> Result<usize, NzbError> {
        Ok(self.conn.execute(
            "DELETE FROM damage_ledger
             WHERE scope_id NOT IN (SELECT id FROM queue)
               AND scope_id NOT IN (SELECT id FROM history)",
            [],
        )?)
    }

    fn ledger_get(&self, key: &ArticleKey) -> Result<Option<LedgerRecord>, NzbError> {
        let result = self.conn.query_row(
            "SELECT message_id, state, evidence, first_refused_at,
                    confirmed_at, expires_at, server_fingerprint
             FROM damage_ledger
             WHERE scope_id = ?1 AND file_index = ?2 AND segment_number = ?3",
            params![
                key.scope_id,
                key.file_index as i64,
                key.segment_number as i64
            ],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?.unwrap_or_default(),
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, Option<String>>(5)?,
                    row.get::<_, String>(6)?,
                ))
            },
        );
        match result {
            Ok((message_id, state, evidence_json, first, confirmed, expires, fingerprint)) => {
                Ok(Some(LedgerRecord {
                    key: key.clone(),
                    message_id,
                    state: LedgerState::from_str(&state),
                    evidence: serde_json::from_str(&evidence_json).unwrap_or_default(),
                    first_refused_at: first.as_deref().and_then(parse_dt),
                    confirmed_at: confirmed.as_deref().and_then(parse_dt),
                    expires_at: expires.as_deref().and_then(parse_dt),
                    server_fingerprint: fingerprint,
                }))
            }
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(NzbError::Database(e)),
        }
    }

    fn ledger_upsert(&self, rec: &LedgerRecord) -> Result<(), NzbError> {
        let evidence = serde_json::to_string(&rec.evidence).unwrap_or_else(|_| "[]".to_string());
        self.conn.execute(
            "INSERT INTO damage_ledger
                (scope_id, file_index, segment_number, message_id, state, evidence,
                 first_refused_at, confirmed_at, expires_at, server_fingerprint)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
             ON CONFLICT(scope_id, file_index, segment_number) DO UPDATE SET
                message_id = excluded.message_id,
                state = excluded.state,
                evidence = excluded.evidence,
                first_refused_at = excluded.first_refused_at,
                confirmed_at = excluded.confirmed_at,
                expires_at = excluded.expires_at,
                server_fingerprint = excluded.server_fingerprint",
            params![
                rec.key.scope_id,
                rec.key.file_index as i64,
                rec.key.segment_number as i64,
                rec.message_id,
                rec.state.as_str(),
                evidence,
                rec.first_refused_at.map(|d| d.to_rfc3339()),
                rec.confirmed_at.map(|d| d.to_rfc3339()),
                rec.expires_at.map(|d| d.to_rfc3339()),
                rec.server_fingerprint,
            ],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn servers(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    fn key() -> ArticleKey {
        ArticleKey::new("job-1", 0, 3)
    }

    fn t(secs: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_700_000_000 + secs, 0).unwrap()
    }

    #[test]
    fn transient_430_then_200_is_present() {
        let db = Database::open_memory().unwrap();
        let srv = servers(&["a", "b"]);

        let c = db
            .ledger_note(
                &key(),
                "<m>",
                &Outcome::Refused { server: "a".into() },
                &srv,
                t(0),
            )
            .unwrap();
        assert_eq!(c, ConfirmNeeded(true));
        assert!(db.ledger_held_provisionally(&key()).unwrap());

        db.ledger_note(&key(), "<m>", &Outcome::Present, &srv, t(1))
            .unwrap();

        assert!(!db.ledger_held_provisionally(&key()).unwrap());
        assert_eq!(
            db.ledger_consult(&key(), t(2)).unwrap(),
            Consult {
                skip: false,
                reprobe: false
            }
        );
        assert_eq!(
            db.ledger_load_file("job-1", 0).unwrap()[0].state,
            LedgerState::Present
        );
    }

    #[test]
    fn refused_on_every_server_confirms_after_delay() {
        let db = Database::open_memory().unwrap();
        let srv = servers(&["a", "b"]);

        db.ledger_note(
            &key(),
            "<m>",
            &Outcome::Refused { server: "a".into() },
            &srv,
            t(0),
        )
        .unwrap();
        // Both refused but within the confirm delay: still provisional.
        let c = db
            .ledger_note(
                &key(),
                "<m>",
                &Outcome::Refused { server: "b".into() },
                &srv,
                t(0),
            )
            .unwrap();
        assert_eq!(c, ConfirmNeeded(true));
        assert!(db.ledger_held_provisionally(&key()).unwrap());

        // A later refusal past the delay confirms the hole.
        let c = db
            .ledger_note(
                &key(),
                "<m>",
                &Outcome::Refused { server: "b".into() },
                &srv,
                t(2),
            )
            .unwrap();
        assert_eq!(c, ConfirmNeeded(false));
        assert_eq!(
            db.ledger_consult(&key(), t(2)).unwrap(),
            Consult {
                skip: true,
                reprobe: false
            }
        );
    }

    #[test]
    fn transport_on_last_server_never_confirms() {
        let db = Database::open_memory().unwrap();
        let srv = servers(&["a", "b"]);

        db.ledger_note(
            &key(),
            "<m>",
            &Outcome::Refused { server: "a".into() },
            &srv,
            t(0),
        )
        .unwrap();
        // b only ever fails transport — it never definitively refused.
        db.ledger_note(
            &key(),
            "<m>",
            &Outcome::Transport { server: "b".into() },
            &srv,
            t(5),
        )
        .unwrap();
        db.ledger_note(
            &key(),
            "<m>",
            &Outcome::Refused { server: "a".into() },
            &srv,
            t(10),
        )
        .unwrap();

        assert!(db.ledger_held_provisionally(&key()).unwrap());
        assert_eq!(
            db.ledger_consult(&key(), t(10)).unwrap(),
            Consult {
                skip: false,
                reprobe: false
            }
        );
    }

    #[test]
    fn confirmed_hole_reprobes_after_ttl() {
        let db = Database::open_memory().unwrap();
        let srv = servers(&["a", "b"]);

        db.ledger_note(
            &key(),
            "<m>",
            &Outcome::Refused { server: "a".into() },
            &srv,
            t(0),
        )
        .unwrap();
        db.ledger_note(
            &key(),
            "<m>",
            &Outcome::Refused { server: "b".into() },
            &srv,
            t(2),
        )
        .unwrap();
        assert_eq!(
            db.ledger_consult(&key(), t(2)).unwrap(),
            Consult {
                skip: true,
                reprobe: false
            }
        );

        // Within TTL: still skip. Past TTL: re-probe.
        assert_eq!(
            db.ledger_consult(&key(), t(2 + HOLE_TTL_SECS - 1)).unwrap(),
            Consult {
                skip: true,
                reprobe: false
            }
        );
        assert_eq!(
            db.ledger_consult(&key(), t(2 + HOLE_TTL_SECS + 1)).unwrap(),
            Consult {
                skip: false,
                reprobe: true
            }
        );
    }

    #[test]
    fn adding_a_server_invalidates_the_hole() {
        let db = Database::open_memory().unwrap();

        // Confirm a hole against {a, b}.
        db.ledger_note(
            &key(),
            "<m>",
            &Outcome::Refused { server: "a".into() },
            &servers(&["a", "b"]),
            t(0),
        )
        .unwrap();
        db.ledger_note(
            &key(),
            "<m>",
            &Outcome::Refused { server: "b".into() },
            &servers(&["a", "b"]),
            t(2),
        )
        .unwrap();
        assert_eq!(
            db.ledger_load_file("job-1", 0).unwrap()[0].state,
            LedgerState::ConfirmedMissing
        );

        // A newly added server c changes the fingerprint: the record resets and
        // a single refusal is provisional again, not confirmed.
        let c = db
            .ledger_note(
                &key(),
                "<m>",
                &Outcome::Refused { server: "a".into() },
                &servers(&["a", "b", "c"]),
                t(3),
            )
            .unwrap();
        assert_eq!(c, ConfirmNeeded(true));
        let rec = &db.ledger_load_file("job-1", 0).unwrap()[0];
        assert_eq!(rec.state, LedgerState::ProvisionalMissing);
        assert_eq!(rec.refused_servers().len(), 1);
    }

    #[test]
    fn file_gone_ratio_counts_confirmed_only() {
        let db = Database::open_memory().unwrap();
        let srv = servers(&["a"]);

        // segment 0: confirmed missing
        let k0 = ArticleKey::new("job-2", 0, 0);
        db.ledger_note(
            &k0,
            "<m0>",
            &Outcome::Refused { server: "a".into() },
            &srv,
            t(0),
        )
        .unwrap();
        db.ledger_note(
            &k0,
            "<m0>",
            &Outcome::Refused { server: "a".into() },
            &srv,
            t(2),
        )
        .unwrap();
        // segment 1: present
        let k1 = ArticleKey::new("job-2", 0, 1);
        db.ledger_note(&k1, "<m1>", &Outcome::Present, &srv, t(0))
            .unwrap();

        assert_eq!(db.ledger_file_gone_ratio("job-2", 0).unwrap(), 0.5);
    }

    #[test]
    fn note_batch_applies_in_order_with_each_notes_time() {
        let db = Database::open_memory().unwrap();
        let srv = servers(&["a", "b"]);
        let key = ArticleKey::new("job-3", 0, 1);
        let refused = |server: &str| Outcome::Refused {
            server: server.into(),
        };
        // Two fast refusals, then a confirming one past the guard: one batch.
        let notes = vec![
            (key.clone(), "<m>".to_string(), refused("a"), t(0)),
            (key.clone(), "<m>".to_string(), refused("b"), t(0)),
            (key.clone(), "<m>".to_string(), refused("a"), t(2)),
        ];
        let confirms = db.ledger_note_batch(&notes, &srv).unwrap();
        assert_eq!(
            confirms,
            vec![
                ConfirmNeeded(true),
                ConfirmNeeded(true),
                ConfirmNeeded(false)
            ]
        );
        let records = db.ledger_load_file("job-3", 0).unwrap();
        assert_eq!(records[0].state, LedgerState::ConfirmedMissing);
    }
}
