//! Feeds per-server article outcomes from the download engine into the
//! damage ledger (WI-1225).
//!
//! The engine emits one `ProgressUpdate::ArticleOutcome` per server attempt.
//! A job's progress listener buffers them in a [`LedgerRecorder`] and writes
//! them to the ledger in batches, so the database is touched once per batch
//! rather than once per article.
//!
//! A single `430` is only provisional: the ledger confirms a hole once every
//! enabled server has refused the article and the busy-provider guard
//! ([`HOLE_CONFIRM_DELAY_SECS`]) has elapsed. During a download every server
//! usually refuses within milliseconds, so an article that fails everywhere
//! is left provisional. [`probe_server`] re-asks after the guard, and
//! that confirms it.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use chrono::Utc;
use tracing::warn;

use crate::nzb_core::damage_ledger::HOLE_CONFIRM_DELAY_SECS;
use crate::nzb_core::db::Database;
use crate::nzb_core::{ArticleKey, Outcome};
use nzb_nntp::NntpError;
use nzb_nntp::{NntpConnection, ServerConfig};

/// Outcomes buffered before a batch is written.
const BATCH_SIZE: usize = 64;

/// Upper bound on one confirm re-ask: connect, authenticate and fetch.
const PROBE_TIMEOUT: Duration = Duration::from_secs(20);

/// How long to wait after a terminal failure before the confirm re-ask. It
/// must exceed the ledger's busy-provider guard.
pub(crate) const CONFIRM_BACKOFF: Duration =
    Duration::from_millis(HOLE_CONFIRM_DELAY_SECS as u64 * 1000 + 250);

/// Confirm re-asks allowed to run at once across all jobs.
pub(crate) const MAX_CONCURRENT_PROBES: usize = 2;

struct Pending {
    file_id: String,
    segment_number: u32,
    message_id: String,
    outcome: Outcome,
}

/// Per-job buffer of article outcomes waiting to be written to the ledger.
#[derive(Default)]
pub(crate) struct LedgerRecorder {
    pending: Vec<Pending>,
    /// `file_id` → position of the file in `job.files`.
    file_index: HashMap<String, u32>,
    /// Articles whose last write asked for a confirming re-ask, with their
    /// message-ids.
    needs_confirm: HashMap<(u32, u32), String>,
    /// Articles already re-asked once; a re-ask is never repeated.
    probed: HashSet<(u32, u32)>,
}

impl LedgerRecorder {
    pub(crate) fn push(
        &mut self,
        file_id: String,
        segment_number: u32,
        message_id: String,
        outcome: Outcome,
    ) {
        self.pending.push(Pending {
            file_id,
            segment_number,
            message_id,
            outcome,
        });
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    pub(crate) fn is_full(&self) -> bool {
        self.pending.len() >= BATCH_SIZE
    }

    /// Whether any buffered outcome names a file whose index is not known yet.
    pub(crate) fn needs_file_index(&self) -> bool {
        self.pending
            .iter()
            .any(|p| !self.file_index.contains_key(&p.file_id))
    }

    /// Learn the file order of the job, i.e. the ids of `job.files`.
    pub(crate) fn set_file_order<'a>(&mut self, file_ids: impl Iterator<Item = &'a str>) {
        self.file_index = file_ids
            .enumerate()
            .map(|(idx, id)| (id.to_string(), idx as u32))
            .collect();
    }

    pub(crate) fn file_index(&self, file_id: &str) -> Option<u32> {
        self.file_index.get(file_id).copied()
    }

    /// Write every buffered outcome to the ledger.
    pub(crate) fn flush(&mut self, job_id: &str, db: &Database, enabled_servers: &[String]) {
        let now = Utc::now();
        for p in std::mem::take(&mut self.pending) {
            let Some(file_index) = self.file_index(&p.file_id) else {
                warn!(job_id, file_id = %p.file_id, "Damage ledger: outcome for an unknown file");
                continue;
            };
            let key = ArticleKey::new(job_id, file_index, p.segment_number);
            match db.ledger_note(&key, &p.message_id, &p.outcome, enabled_servers, now) {
                Ok(confirm) if confirm.0 => {
                    self.needs_confirm
                        .insert((file_index, p.segment_number), p.message_id);
                }
                Ok(_) => {
                    self.needs_confirm.remove(&(file_index, p.segment_number));
                }
                Err(e) => warn!(job_id, "Damage ledger write failed: {e}"),
            }
        }
    }

    /// Claim the single confirm re-ask for an article that failed on every
    /// server. Returns its message-id when the last write asked for a confirm
    /// and the article has not been re-asked yet.
    pub(crate) fn take_confirm(&mut self, file_index: u32, segment_number: u32) -> Option<String> {
        let key = (file_index, segment_number);
        let message_id = self.needs_confirm.remove(&key)?;
        self.probed.insert(key).then_some(message_id)
    }
}

/// Re-asks per server during a confirm. A provider is known to answer `430`
/// for an article it holds while busy, so a refusal is asked once more
/// before it counts.
const PROBE_ATTEMPTS: usize = 2;

/// The confirm re-ask to one server: up to [`PROBE_ATTEMPTS`] asks, stopping
/// at the first answer that is not a refusal. Only this final answer is
/// written to the ledger, so a provider that serves on the second ask clears
/// the hole instead of confirming it.
pub(crate) async fn probe_server(server: &ServerConfig, message_id: &str) -> Outcome {
    let mut outcome = ask_server(server, message_id).await;
    for _ in 1..PROBE_ATTEMPTS {
        if !matches!(outcome, Outcome::Refused { .. }) {
            break;
        }
        outcome = ask_server(server, message_id).await;
    }
    outcome
}

/// Ask one server for an article on a fresh connection and translate the
/// answer into a ledger outcome. A body is `Present`, a "not here" answer is
/// `Refused`, and anything else, including a timeout, is `Transport`.
async fn ask_server(server: &ServerConfig, message_id: &str) -> Outcome {
    let attempt = async {
        let mut conn = NntpConnection::new(server.id.clone());
        conn.connect(server).await?;
        let result = conn.fetch_article(message_id).await;
        let _ = conn.quit().await;
        result
    };
    match tokio::time::timeout(PROBE_TIMEOUT, attempt).await {
        Ok(Ok(_)) => Outcome::Present,
        Ok(Err(
            NntpError::ArticleNotFound(_)
            | NntpError::NoSuchGroup(_)
            | NntpError::NoArticleSelected(_),
        )) => Outcome::Refused {
            server: server.id.clone(),
        },
        Ok(Err(_)) | Err(_) => Outcome::Transport {
            server: server.id.clone(),
        },
    }
}
