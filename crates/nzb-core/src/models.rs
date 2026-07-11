use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Job status lifecycle
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobStatus {
    Queued,
    Downloading,
    Paused,
    Verifying,
    Repairing,
    Extracting,
    PostProcessing,
    Completed,
    Failed,
}

impl std::fmt::Display for JobStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Queued => write!(f, "Queued"),
            Self::Downloading => write!(f, "Downloading"),
            Self::Paused => write!(f, "Paused"),
            Self::Verifying => write!(f, "Verifying"),
            Self::Repairing => write!(f, "Repairing"),
            Self::Extracting => write!(f, "Extracting"),
            Self::PostProcessing => write!(f, "PostProcessing"),
            Self::Completed => write!(f, "Completed"),
            Self::Failed => write!(f, "Failed"),
        }
    }
}

// ---------------------------------------------------------------------------
// Priority
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum Priority {
    Low = 0,
    #[default]
    Normal = 1,
    High = 2,
    Force = 3,
}

impl From<Priority> for u8 {
    fn from(p: Priority) -> u8 {
        p as u8
    }
}

impl TryFrom<u8> for Priority {
    type Error = u8;
    fn try_from(v: u8) -> Result<Self, u8> {
        match v {
            0 => Ok(Self::Low),
            1 => Ok(Self::Normal),
            2 => Ok(Self::High),
            3 => Ok(Self::Force),
            other => Err(other),
        }
    }
}

impl Serialize for Priority {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_u8(*self as u8)
    }
}

impl<'de> Deserialize<'de> for Priority {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let v = u8::deserialize(d)?;
        Self::try_from(v).map_err(|_| serde::de::Error::custom(format!("invalid priority {v}")))
    }
}

// ---------------------------------------------------------------------------
// NZB data model
// ---------------------------------------------------------------------------

/// Per-server article download statistics for a job.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ServerArticleStats {
    pub server_id: String,
    pub server_name: String,
    pub articles_downloaded: usize,
    pub articles_failed: usize,
    pub bytes_downloaded: u64,
}

/// A complete download job (parsed from one NZB file).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NzbJob {
    /// Unique job identifier
    pub id: String,
    /// Human-readable name (from NZB filename or metadata)
    pub name: String,
    /// Category for this download
    pub category: String,
    /// Current status
    pub status: JobStatus,
    /// Download priority
    pub priority: Priority,
    /// Total size in bytes (sum of all articles)
    pub total_bytes: u64,
    /// Bytes downloaded so far
    pub downloaded_bytes: u64,
    /// Number of files in this job
    pub file_count: usize,
    /// Number of files completed
    pub files_completed: usize,
    /// Number of articles total
    pub article_count: usize,
    /// Number of articles downloaded
    pub articles_downloaded: usize,
    /// Number of articles failed
    pub articles_failed: usize,
    /// When the job was added
    pub added_at: DateTime<Utc>,
    /// When the job completed (if applicable)
    pub completed_at: Option<DateTime<Utc>>,
    /// Working directory for this job (incomplete)
    pub work_dir: PathBuf,
    /// Final output directory
    pub output_dir: PathBuf,
    /// Optional password for extraction
    pub password: Option<String>,
    /// Error message if failed
    pub error_message: Option<String>,
    /// Current download speed for this job (bytes/sec)
    #[serde(default)]
    pub speed_bps: u64,
    /// Per-server download statistics
    #[serde(default)]
    pub server_stats: Vec<ServerArticleStats>,
    /// Files in this job
    #[serde(skip)]
    pub files: Vec<NzbFile>,
}

/// A single file within an NZB job (collection of NNTP articles).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NzbFile {
    /// Unique file identifier
    pub id: String,
    /// Filename (from yEnc header or NZB subject)
    pub filename: String,
    /// Total size in bytes
    pub bytes: u64,
    /// Bytes downloaded
    pub bytes_downloaded: u64,
    /// Is this a par2 file?
    pub is_par2: bool,
    /// Par2 set name (if par2)
    pub par2_setname: Option<String>,
    /// Par2 volume number (if par2)
    pub par2_vol: Option<u32>,
    /// Par2 block count (if par2)
    pub par2_blocks: Option<u32>,
    /// File assembly complete
    pub assembled: bool,
    /// Newsgroup(s) this file was posted to
    pub groups: Vec<String>,
    /// Article segments
    #[serde(skip)]
    pub articles: Vec<Article>,
}

/// A single NNTP article (segment of a file) — re-exported from the `nzb-nntp` crate.
pub use nzb_nntp::Article;

// ---------------------------------------------------------------------------
// History record (for completed/failed jobs)
// ---------------------------------------------------------------------------

/// A history entry for a completed or failed job.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub id: String,
    pub name: String,
    pub category: String,
    pub status: JobStatus,
    pub total_bytes: u64,
    pub downloaded_bytes: u64,
    pub added_at: DateTime<Utc>,
    pub completed_at: DateTime<Utc>,
    pub output_dir: PathBuf,
    /// Post-processing stages with results
    pub stages: Vec<StageResult>,
    pub error_message: Option<String>,
    /// Per-server download statistics
    #[serde(default)]
    pub server_stats: Vec<ServerArticleStats>,
    /// Raw NZB XML data (for retry)
    #[serde(skip_serializing)]
    pub nzb_data: Option<Vec<u8>>,
}

/// Immutable statistics ledger row recorded when a job leaves the queue.
/// Unlike history, these compact rows are retained when users clear or prune
/// individual history entries so lifetime counters remain meaningful.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DownloadStatistic {
    pub job_id: String,
    pub completed_at: DateTime<Utc>,
    pub status: JobStatus,
    pub total_bytes: u64,
    pub downloaded_bytes: u64,
    pub duration_secs: f64,
    pub average_speed_bps: u64,
    pub server_stats: Vec<ServerArticleStats>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StageResult {
    pub name: String,
    pub status: StageStatus,
    pub message: Option<String>,
    pub duration_secs: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StageStatus {
    Success,
    Failed,
    Skipped,
}

// ---------------------------------------------------------------------------
// RSS feed items and download rules
// ---------------------------------------------------------------------------

/// A discovered item from an RSS feed, persisted in the database.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RssItem {
    /// Feed entry ID (from the RSS feed)
    pub id: String,
    /// Name of the feed this came from
    pub feed_name: String,
    /// Title of the entry
    pub title: String,
    /// NZB download URL
    pub url: Option<String>,
    /// When the entry was published (from feed)
    pub published_at: Option<DateTime<Utc>>,
    /// When we first saw this item
    pub first_seen_at: DateTime<Utc>,
    /// Whether this item has been downloaded
    pub downloaded: bool,
    /// When it was downloaded (if applicable)
    pub downloaded_at: Option<DateTime<Utc>>,
    /// Category used when downloaded
    pub category: Option<String>,
    /// Size in bytes (if available from feed)
    pub size_bytes: u64,
}

/// A download rule that automatically enqueues matching RSS feed items.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RssRule {
    /// Unique rule identifier
    pub id: String,
    /// Human-readable name for the rule
    pub name: String,
    /// Which feed(s) this rule applies to (one or more feed names)
    pub feed_names: Vec<String>,
    /// Category to assign to downloaded NZBs
    pub category: Option<String>,
    /// Download priority (0=low, 1=normal, 2=high, 3=force)
    pub priority: i32,
    /// Regex to match against feed item titles (applied to pre-filtered items)
    pub match_regex: String,
    /// Whether this rule is active
    pub enabled: bool,
}

// ---------------------------------------------------------------------------
// Newsgroup browsing
// ---------------------------------------------------------------------------

#[cfg(feature = "groups-db")]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GroupRow {
    pub id: i64,
    pub name: String,
    pub description: Option<String>,
    pub subscribed: bool,
    pub article_count: i64,
    pub first_article: i64,
    pub last_article: i64,
    pub last_scanned: i64,
    pub last_updated: Option<String>,
    pub created_at: String,
    #[serde(default)]
    pub unread_count: i64,
}

#[cfg(feature = "groups-db")]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HeaderRow {
    pub id: i64,
    pub group_id: i64,
    pub article_num: i64,
    pub subject: String,
    pub author: String,
    pub date: String,
    pub message_id: String,
    pub references_: String,
    pub bytes: i64,
    pub lines: i64,
    pub read: bool,
    pub downloaded_at: String,
}

#[cfg(feature = "groups-db")]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThreadSummary {
    pub root_message_id: String,
    pub subject: String,
    pub author: String,
    pub date: String,
    pub last_reply_date: String,
    pub reply_count: i64,
    pub unread_count: i64,
}

#[cfg(feature = "groups-db")]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThreadArticle {
    #[serde(flatten)]
    pub header: HeaderRow,
    pub depth: i32,
}

#[cfg(feature = "groups-db")]
#[derive(Debug, Clone, Deserialize)]
pub struct MarkReadInput {
    pub header_ids: Vec<i64>,
}

#[cfg(feature = "groups-db")]
#[derive(Debug, Clone, Deserialize)]
pub struct DownloadSelectedInput {
    pub message_ids: Vec<String>,
    pub name: Option<String>,
    pub category: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_job_status_display() {
        assert_eq!(JobStatus::Queued.to_string(), "Queued");
        assert_eq!(JobStatus::Downloading.to_string(), "Downloading");
        assert_eq!(JobStatus::Paused.to_string(), "Paused");
        assert_eq!(JobStatus::Verifying.to_string(), "Verifying");
        assert_eq!(JobStatus::Repairing.to_string(), "Repairing");
        assert_eq!(JobStatus::Extracting.to_string(), "Extracting");
        assert_eq!(JobStatus::PostProcessing.to_string(), "PostProcessing");
        assert_eq!(JobStatus::Completed.to_string(), "Completed");
        assert_eq!(JobStatus::Failed.to_string(), "Failed");
    }

    #[test]
    fn test_job_status_serde_roundtrip() {
        let statuses = [
            JobStatus::Queued,
            JobStatus::Downloading,
            JobStatus::Paused,
            JobStatus::Completed,
            JobStatus::Failed,
        ];

        for status in &statuses {
            let json = serde_json::to_string(status).unwrap();
            let restored: JobStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(*status, restored);
        }
    }

    #[test]
    fn test_job_status_serde_snake_case() {
        let json = serde_json::to_string(&JobStatus::PostProcessing).unwrap();
        assert_eq!(json, "\"post_processing\"");

        let restored: JobStatus = serde_json::from_str("\"post_processing\"").unwrap();
        assert_eq!(restored, JobStatus::PostProcessing);
    }

    #[test]
    fn test_priority_default() {
        let p = Priority::default();
        assert_eq!(p, Priority::Normal);
    }

    #[test]
    fn test_priority_ordering() {
        assert!(Priority::Low < Priority::Normal);
        assert!(Priority::Normal < Priority::High);
        assert!(Priority::High < Priority::Force);
    }

    #[test]
    fn test_priority_values() {
        assert_eq!(Priority::Low as i32, 0);
        assert_eq!(Priority::Normal as i32, 1);
        assert_eq!(Priority::High as i32, 2);
        assert_eq!(Priority::Force as i32, 3);
    }

    #[test]
    fn test_priority_serde_roundtrip() {
        for p in [
            Priority::Low,
            Priority::Normal,
            Priority::High,
            Priority::Force,
        ] {
            let json = serde_json::to_string(&p).unwrap();
            let restored: Priority = serde_json::from_str(&json).unwrap();
            assert_eq!(p, restored);
        }
    }

    #[test]
    fn test_stage_status_serde() {
        let statuses = [
            StageStatus::Success,
            StageStatus::Failed,
            StageStatus::Skipped,
        ];
        for s in &statuses {
            let json = serde_json::to_string(s).unwrap();
            let restored: StageStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(*s, restored);
        }
    }

    #[test]
    fn test_stage_status_snake_case() {
        assert_eq!(
            serde_json::to_string(&StageStatus::Success).unwrap(),
            "\"success\""
        );
        assert_eq!(
            serde_json::to_string(&StageStatus::Failed).unwrap(),
            "\"failed\""
        );
        assert_eq!(
            serde_json::to_string(&StageStatus::Skipped).unwrap(),
            "\"skipped\""
        );
    }

    #[test]
    fn test_stage_result_serde() {
        let sr = StageResult {
            name: "Verify".into(),
            status: StageStatus::Success,
            message: Some("OK".into()),
            duration_secs: 2.5,
        };
        let json = serde_json::to_string(&sr).unwrap();
        let restored: StageResult = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.name, "Verify");
        assert_eq!(restored.status, StageStatus::Success);
        assert_eq!(restored.message.as_deref(), Some("OK"));
        assert!((restored.duration_secs - 2.5).abs() < 0.001);
    }

    #[test]
    fn test_server_article_stats_default() {
        let stats = ServerArticleStats::default();
        assert!(stats.server_id.is_empty());
        assert_eq!(stats.articles_downloaded, 0);
        assert_eq!(stats.articles_failed, 0);
        assert_eq!(stats.bytes_downloaded, 0);
    }

    #[test]
    fn test_server_article_stats_serde() {
        let stats = ServerArticleStats {
            server_id: "srv-1".into(),
            server_name: "Provider".into(),
            articles_downloaded: 100,
            articles_failed: 5,
            bytes_downloaded: 75_000_000,
        };
        let json = serde_json::to_string(&stats).unwrap();
        let restored: ServerArticleStats = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.server_id, "srv-1");
        assert_eq!(restored.articles_downloaded, 100);
        assert_eq!(restored.articles_failed, 5);
        assert_eq!(restored.bytes_downloaded, 75_000_000);
    }
}
