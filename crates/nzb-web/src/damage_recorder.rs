//! Feeds per-server article outcomes from the download engine into the
//! damage ledger (WI-1225).
//!
//! The engine emits one `ProgressUpdate::ArticleOutcome` per server attempt.
//! A job's progress listener buffers them in a [`LedgerRecorder`] and writes
//! each batch in one transaction, so the database is touched once per batch
//! rather than once per article.
//!
//! The confirm the ledger asks for (a refusal from every server spread past
//! the busy-provider guard) is the engine's job: an article every server
//! refuses gets `nzb_dispatch::download_engine::CONFIRM_ASKS_PER_SERVER`
//! further rounds after the guard, through the pool's own connections, before
//! it fails. Their outcomes arrive here like any other.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use tracing::warn;

use crate::nzb_core::db::Database;
use crate::nzb_core::{ArticleKey, Outcome};

/// Outcomes buffered before a batch is written.
const BATCH_SIZE: usize = 256;

struct Pending {
    file_id: String,
    segment_number: u32,
    message_id: String,
    outcome: Outcome,
    at: DateTime<Utc>,
}

/// Per-job buffer of article outcomes waiting to be written to the ledger.
#[derive(Default)]
pub(crate) struct LedgerRecorder {
    pending: Vec<Pending>,
    /// `file_id` → position of the file in `job.files`.
    file_index: HashMap<String, u32>,
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
            at: Utc::now(),
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

    /// Write every buffered outcome to the ledger in one transaction.
    pub(crate) fn flush(&mut self, job_id: &str, db: &Database, enabled_servers: &[String]) {
        let mut notes = Vec::with_capacity(self.pending.len());
        for p in std::mem::take(&mut self.pending) {
            let Some(&file_index) = self.file_index.get(&p.file_id) else {
                warn!(job_id, file_id = %p.file_id, "Damage ledger: outcome for an unknown file");
                continue;
            };
            let key = ArticleKey::new(job_id, file_index, p.segment_number);
            notes.push((key, p.message_id, p.outcome, p.at));
        }
        if let Err(e) = db.ledger_note_batch(&notes, enabled_servers) {
            warn!(
                job_id,
                rows = notes.len(),
                "Damage ledger batch write failed: {e}"
            );
        }
    }
}
