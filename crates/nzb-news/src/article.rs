//! Article / NzbFile / NzbObject — the three-level download hierarchy.
//!
//! The three levels exist because retry semantics differ at each:
//!
//! - [`Article`] — a single chunked segment (yEnc article). The unit of NNTP
//!   work. Owns the try-list consulted by the dispatcher: "which servers
//!   have we already asked for this message-id?"
//! - [`NzbFile`] — a set of articles that assemble into one final file. The
//!   file-level try-list tracks "which servers have we exhausted across all
//!   articles in this file?" — once every article in the file has failed on
//!   server S, new articles shouldn't waste a request on S.
//! - [`NzbObject`] — the whole NZB (the "job"). Aggregates file-level state
//!   plus hopeless detection, job-wide priorities, and scheduling metadata.
//!
//! The cascade-reset path uses all three levels: it resets the lowest level
//! first, and only cascades upward if that doesn't help.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};

use crate::trylist::TryList;

/// Per-article state: the message-id, which servers have been tried, how
/// many total attempts have been made.
///
/// `tries` vs `try_list.len()`:
/// - `try_list` is the set of **distinct** servers already tried.
/// - `tries` is the **total attempt count**, which can exceed `try_list.len()`
///   if a single server is retried after reconnect. `max_art_tries` (default 3)
///   bounds total attempts, not distinct servers.
#[derive(Debug)]
pub struct Article {
    /// Message-id (without angle brackets).
    pub message_id: String,
    /// Parent NzbFile ID (for back-pointer when reporting progress).
    pub file_id: String,
    /// Parent NzbObject (job) ID.
    pub job_id: String,
    /// Advertised article byte size from the NZB (may differ from actual).
    pub bytes: u64,
    /// Segment index within the file (for ordering).
    pub segment: u32,

    /// Servers already asked for this article.
    try_list: TryList,
    /// Total fetch attempts regardless of which server.
    tries: AtomicU8,
    /// The last server this article was dispatched to (for priority gating).
    /// `0` means "never dispatched". Stored as string via interning is
    /// overkill; we use an atomic slot populated with a `u64` hash of the
    /// server-id — not used for security, only for the priority-cascade
    /// `fetcher_priority` check, which compares against the server list.
    fetcher_priority: AtomicU8,
    /// Serial number (wraps at u64::MAX), for FIFO tie-breaking when
    /// multiple articles are candidates for dispatch.
    pub serial: u64,
    /// Tracks whether every failure observed on this article so far has
    /// been a confirmed-absent (`430 Article not found`) response. If so,
    /// cascade-retry is disabled: the article is given up on as soon as
    /// every active server has replied `430`. A single transient failure
    /// (timeout / network / protocol) flips this to `false`, restoring
    /// cascade-retry behaviour so a one-off network blip doesn't mark the
    /// article permanently missing.
    confirmed_absent_only: AtomicBool,
    /// Providers that explicitly returned NNTP 430 for this message-id.
    /// Unlike `try_list`, this is availability evidence: connection,
    /// authentication, timeout, and protocol failures never enter it.
    explicit_not_found: TryList,
}

impl Article {
    /// Construct a new article in the "never dispatched" state.
    pub fn new(
        message_id: impl Into<String>,
        file_id: impl Into<String>,
        job_id: impl Into<String>,
        bytes: u64,
        segment: u32,
        serial: u64,
    ) -> Self {
        Self {
            message_id: message_id.into(),
            file_id: file_id.into(),
            job_id: job_id.into(),
            bytes,
            segment,
            try_list: TryList::new(),
            tries: AtomicU8::new(0),
            // 255 is our "no fetcher priority yet" sentinel. Real server
            // priorities are u8 in the range 0..=50 (the provider config
            // schema caps at 50).
            fetcher_priority: AtomicU8::new(u8::MAX),
            serial,
            confirmed_absent_only: AtomicBool::new(true),
            explicit_not_found: TryList::new(),
        }
    }

    /// Returns `true` if every failure observed on this article so far has
    /// been `ArticleNotFound`. Callers use this to skip cascade-retry on
    /// articles that appear to be genuinely missing across all servers.
    pub fn confirmed_absent_only(&self) -> bool {
        self.confirmed_absent_only.load(Ordering::Relaxed)
    }

    /// Record that this article failed with a non-absent error (timeout,
    /// network, protocol). Flipping this flag re-enables cascade-retry so
    /// a transient failure isn't treated as permanent absence.
    pub fn mark_transient_failure(&self) {
        self.confirmed_absent_only.store(false, Ordering::Relaxed);
    }

    /// Record an explicit NNTP 430 from `server_id`.
    pub fn mark_server_not_found(&self, server_id: &str) {
        self.try_list.add(server_id);
        self.explicit_not_found.add(server_id);
    }

    /// Whether `server_id` explicitly established article absence.
    pub fn server_explicitly_not_found(&self, server_id: &str) -> bool {
        self.explicit_not_found.contains(server_id)
    }

    /// Sorted provider IDs that explicitly returned NNTP 430.
    pub fn explicit_not_found_servers(&self) -> Vec<String> {
        let mut servers = self
            .explicit_not_found
            .snapshot()
            .into_iter()
            .collect::<Vec<_>>();
        servers.sort_unstable();
        servers
    }

    /// Total attempts made so far.
    pub fn tries(&self) -> u8 {
        self.tries.load(Ordering::Relaxed)
    }

    /// Record a fetch attempt. Returns the new attempt count.
    pub fn increment_tries(&self) -> u8 {
        self.tries.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// Clear the attempt counter (used when all servers are to be re-tried).
    pub fn reset_tries(&self) {
        self.tries.store(0, Ordering::Relaxed);
    }

    /// Priority value of the last server this article was dispatched to,
    /// or `u8::MAX` if it has never been dispatched. Used by the priority
    /// gate to prefer a higher-priority server if one is available.
    pub fn fetcher_priority(&self) -> u8 {
        self.fetcher_priority.load(Ordering::Relaxed)
    }

    /// Record which priority tier this article was dispatched to.
    pub fn set_fetcher_priority(&self, p: u8) {
        self.fetcher_priority.store(p, Ordering::Relaxed);
    }

    /// Returns `true` if `server_id` is already in the try-list.
    pub fn server_tried(&self, server_id: &str) -> bool {
        self.try_list.contains(server_id)
    }

    /// Record that `server_id` has been tried.
    pub fn mark_server_tried(&self, server_id: &str) {
        self.try_list.add(server_id);
    }

    /// Reset the try-list (called by a cascade reset from NzbFile or NzbObject).
    pub fn reset_try_list(&self) {
        self.try_list.reset();
    }

    /// Access to the underlying try-list (for snapshotting / observability).
    pub fn try_list(&self) -> &TryList {
        &self.try_list
    }
}

// ---------------------------------------------------------------------------
// NzbFile — a set of articles that assemble into one on-disk file.
// ---------------------------------------------------------------------------

/// A set of articles that together make up one output file. Owns a
/// file-level try-list (servers exhausted *across all articles* — not the
/// same as any single article's try-list).
#[derive(Debug)]
pub struct NzbFile {
    /// File identifier (opaque; matches the nzb-core NzbFile::id).
    pub id: String,
    /// Parent job ID.
    pub job_id: String,
    /// Display name (for logs).
    pub display_name: String,
    /// Total articles this file is composed of.
    pub article_count: u32,
    /// Articles that have completed (success or discard).
    completed_articles: AtomicU64,
    /// File-level try-list: servers known to be useless for this file.
    try_list: TryList,
}

impl NzbFile {
    pub fn new(
        id: impl Into<String>,
        job_id: impl Into<String>,
        display_name: impl Into<String>,
        article_count: u32,
    ) -> Self {
        Self {
            id: id.into(),
            job_id: job_id.into(),
            display_name: display_name.into(),
            article_count,
            completed_articles: AtomicU64::new(0),
            try_list: TryList::new(),
        }
    }

    /// Record one article as finished (success or permanent discard).
    /// Returns the new completion count.
    pub fn mark_article_completed(&self) -> u64 {
        self.completed_articles.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// True when every article in the file has reached a terminal state.
    pub fn is_complete(&self) -> bool {
        self.completed_articles.load(Ordering::Relaxed) >= self.article_count as u64
    }

    /// Is this server known to be useless for *every* article in this file?
    pub fn server_tried(&self, server_id: &str) -> bool {
        self.try_list.contains(server_id)
    }

    /// Mark `server_id` as exhausted for this file.
    pub fn mark_server_tried(&self, server_id: &str) {
        self.try_list.add(server_id);
    }

    /// Reset (used by the cascade reset from NzbObject).
    pub fn reset_try_list(&self) {
        self.try_list.reset();
    }

    pub fn try_list(&self) -> &TryList {
        &self.try_list
    }
}

// ---------------------------------------------------------------------------
// NzbObject — one job (a whole NZB).
// ---------------------------------------------------------------------------

/// A whole NZB job. Holds the job-wide try-list and hopeless-state counters.
#[derive(Debug)]
pub struct NzbObject {
    pub id: String,
    pub display_name: String,
    pub article_count: u64,
    /// Sum across all files (computed once at load).
    pub total_bytes: u64,
    /// Articles that completed with data.
    articles_downloaded: AtomicU64,
    /// Articles that failed on every server and were discarded.
    articles_failed: AtomicU64,
    /// Bytes downloaded (decoded).
    bytes_downloaded: AtomicU64,
    /// Job-level try-list: servers exhausted across the entire job.
    try_list: TryList,
    /// Files in this job — stored for reset cascade and debugging. The
    /// dispatcher keeps Articles in a separate work queue, referencing
    /// NzbFile via file_id for O(1) lookup.
    files: Vec<Arc<NzbFile>>,
}

impl NzbObject {
    pub fn new(
        id: impl Into<String>,
        display_name: impl Into<String>,
        article_count: u64,
        total_bytes: u64,
        files: Vec<Arc<NzbFile>>,
    ) -> Self {
        Self {
            id: id.into(),
            display_name: display_name.into(),
            article_count,
            total_bytes,
            articles_downloaded: AtomicU64::new(0),
            articles_failed: AtomicU64::new(0),
            bytes_downloaded: AtomicU64::new(0),
            try_list: TryList::new(),
            files,
        }
    }

    pub fn articles_downloaded(&self) -> u64 {
        self.articles_downloaded.load(Ordering::Relaxed)
    }

    pub fn articles_failed(&self) -> u64 {
        self.articles_failed.load(Ordering::Relaxed)
    }

    pub fn bytes_downloaded(&self) -> u64 {
        self.bytes_downloaded.load(Ordering::Relaxed)
    }

    /// Record a successful article fetch.
    pub fn record_article_downloaded(&self, bytes: u64) {
        self.articles_downloaded.fetch_add(1, Ordering::Relaxed);
        self.bytes_downloaded.fetch_add(bytes, Ordering::Relaxed);
    }

    /// Record an article as permanently failed.
    pub fn record_article_failed(&self) {
        self.articles_failed.fetch_add(1, Ordering::Relaxed);
    }

    /// Is every article in this job terminal?
    pub fn is_complete(&self) -> bool {
        let done = self.articles_downloaded.load(Ordering::Relaxed)
            + self.articles_failed.load(Ordering::Relaxed);
        done >= self.article_count
    }

    pub fn server_tried(&self, server_id: &str) -> bool {
        self.try_list.contains(server_id)
    }

    pub fn mark_server_tried(&self, server_id: &str) {
        self.try_list.add(server_id);
    }

    /// Cascade reset: clear the job-level try-list, every file's try-list,
    /// and every article's try-list. Use when a new server is added mid-job
    /// and we want every article to be re-dispatchable on it.
    pub fn reset_all_try_lists(&self) {
        self.try_list.reset();
        for file in &self.files {
            file.reset_try_list();
        }
        // Articles are owned by the dispatcher's work queue, not by
        // NzbObject — so the reset of article try-lists is done by the
        // caller iterating the work queue. We only reset our own level here.
    }

    pub fn try_list(&self) -> &TryList {
        &self.try_list
    }

    pub fn files(&self) -> &[Arc<NzbFile>] {
        &self.files
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn article_tries_and_try_list() {
        let a = Article::new("mid1", "file1", "job1", 750_000, 0, 0);
        assert_eq!(a.tries(), 0);
        assert_eq!(a.fetcher_priority(), u8::MAX);
        assert!(!a.server_tried("s1"));
        a.mark_server_tried("s1");
        assert!(a.server_tried("s1"));
        assert_eq!(a.increment_tries(), 1);
        assert_eq!(a.increment_tries(), 2);
        a.reset_try_list();
        assert!(!a.server_tried("s1"));
    }

    #[test]
    fn file_completion_tracking() {
        let f = NzbFile::new("f1", "j1", "demo.rar", 3);
        assert!(!f.is_complete());
        f.mark_article_completed();
        f.mark_article_completed();
        assert!(!f.is_complete());
        assert_eq!(f.mark_article_completed(), 3);
        assert!(f.is_complete());
    }

    #[test]
    fn nzb_object_cascade_reset() {
        let f1 = Arc::new(NzbFile::new("f1", "j1", "a.rar", 2));
        let f2 = Arc::new(NzbFile::new("f2", "j1", "b.rar", 2));
        f1.mark_server_tried("s1");
        f2.mark_server_tried("s1");
        let job = NzbObject::new("j1", "demo", 4, 3_000_000, vec![f1.clone(), f2.clone()]);
        job.mark_server_tried("s1");
        assert!(f1.server_tried("s1"));
        assert!(f2.server_tried("s1"));
        assert!(job.server_tried("s1"));
        job.reset_all_try_lists();
        assert!(!job.server_tried("s1"));
        assert!(!f1.server_tried("s1"));
        assert!(!f2.server_tried("s1"));
    }

    #[test]
    fn job_progress_bookkeeping() {
        let job = NzbObject::new("j1", "demo", 10, 1_000_000, vec![]);
        job.record_article_downloaded(100_000);
        job.record_article_downloaded(100_000);
        job.record_article_failed();
        assert_eq!(job.articles_downloaded(), 2);
        assert_eq!(job.articles_failed(), 1);
        assert_eq!(job.bytes_downloaded(), 200_000);
        assert!(!job.is_complete());
        for _ in 0..7 {
            job.record_article_downloaded(100_000);
        }
        assert!(job.is_complete());
    }
}
