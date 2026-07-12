//! Typed failure taxonomy for article downloads.
//!
//! Replaces the previous `String`-typed `ProgressUpdate::ArticleFailed { error }`.
//! Carrying a structured `ArticleFailureKind` lets the queue manager, hopeless
//! tracker, and circuit breaker each react to *what* failed without parsing
//! free-form messages — and lets future per-server retry policy be expressed
//! in code rather than in regex.
//!
//! Classification happens at the *emit site* (the worker that observed the
//! failure), where the original `NntpError` is still typed. By the time the
//! failure crosses the progress channel, it has been reduced to one of the
//! kinds below plus an opaque `message` for human-readable logs.

use nzb_nntp::error::NntpError;

/// Why an article failed. Drives retry decisions, hopeless tracking, and
/// circuit-breaker logic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ArticleFailureKind {
    /// `430` — article not present on this server. Could be retention drift
    /// or never posted; try other servers.
    NotFound,
    /// `50x` or service unavailable — transient server-side issue. Try
    /// another server; circuit-break this one if it persists.
    ServerDown,
    /// `481` / `482` — authentication failed for this account on this server.
    /// Don't keep retrying with the same credentials.
    AuthFailed,
    /// `403` / explicit forbidden — permanent rejection for this account on
    /// this server. Functionally identical to AuthFailed for retry purposes
    /// but distinguished for diagnostics.
    PermissionDenied,
    /// yEnc decode failure or file-assembly mismatch. Could be corruption on
    /// this server (try another) or genuine corruption on every server.
    DecodeError,
    /// Read/write timeout. Transient — retry on the same or another server.
    Timeout,
    /// TCP socket closed mid-transfer (RST, EOF, etc.).
    ConnectionClosed,
    /// NNTP protocol violation or unexpected response shape.
    Protocol,
    /// Catch-all when classification isn't possible at the emit site.
    Other,
}

impl ArticleFailureKind {
    /// True if this failure is specific to the server that produced it —
    /// the article may still be obtainable from another provider.
    pub fn is_per_server(self) -> bool {
        matches!(
            self,
            Self::NotFound
                | Self::ServerDown
                | Self::AuthFailed
                | Self::PermissionDenied
                | Self::Timeout
                | Self::ConnectionClosed
                | Self::Protocol
        )
    }

    /// True if the failure suggests the article is gone everywhere once
    /// every server has been tried (hopeless-tracker should count it).
    pub fn counts_toward_hopeless(self) -> bool {
        matches!(self, Self::NotFound | Self::DecodeError)
    }

    /// True if this server is unlikely to recover within the lifetime of
    /// the current download — circuit-breaker hint.
    pub fn should_break_server(self) -> bool {
        matches!(self, Self::AuthFailed | Self::PermissionDenied)
    }

    /// Short stable identifier suitable for logs/metrics labels.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NotFound => "not_found",
            Self::ServerDown => "server_down",
            Self::AuthFailed => "auth_failed",
            Self::PermissionDenied => "permission_denied",
            Self::DecodeError => "decode_error",
            Self::Timeout => "timeout",
            Self::ConnectionClosed => "connection_closed",
            Self::Protocol => "protocol",
            Self::Other => "other",
        }
    }
}

/// A classified article failure ready to flow through the progress channel.
#[derive(Debug, Clone)]
pub struct ArticleFailure {
    pub kind: ArticleFailureKind,
    pub server_id: String,
    pub message: String,
}

impl ArticleFailure {
    /// Classify an `NntpError` observed by a worker fetching an article.
    pub fn from_nntp(err: &NntpError, server_id: impl Into<String>) -> Self {
        let kind = match err {
            NntpError::ArticleNotFound(_) => ArticleFailureKind::NotFound,
            NntpError::ServiceUnavailable(_) => ArticleFailureKind::ServerDown,
            NntpError::Auth(_) | NntpError::AuthRequired(_) => ArticleFailureKind::AuthFailed,
            NntpError::PermissionDenied(_) => ArticleFailureKind::PermissionDenied,
            NntpError::Connection(_) => ArticleFailureKind::ConnectionClosed,
            NntpError::Io(_) => ArticleFailureKind::ConnectionClosed,
            NntpError::Timeout(_) => ArticleFailureKind::Timeout,
            NntpError::Protocol(_) => ArticleFailureKind::Protocol,
            NntpError::NoSuchGroup(_) | NntpError::NoArticleSelected(_) => {
                ArticleFailureKind::Protocol
            }
            NntpError::NoConnectionsAvailable(_)
            | NntpError::AllServersExhausted(_)
            | NntpError::Tls(_)
            | NntpError::Shutdown => ArticleFailureKind::Other,
        };
        Self {
            kind,
            server_id: server_id.into(),
            message: err.to_string(),
        }
    }

    /// Decode or yEnc-assembly failure raised above the NNTP layer.
    pub fn decode_error(server_id: impl Into<String>, msg: impl Into<String>) -> Self {
        Self {
            kind: ArticleFailureKind::DecodeError,
            server_id: server_id.into(),
            message: msg.into(),
        }
    }

    /// Article is present nowhere — emitted when every enabled server has
    /// already been tried for this article and the last attempt failed.
    pub fn not_found_anywhere(
        server_id: impl Into<String>,
        provider_outcomes: impl Into<String>,
    ) -> Self {
        Self {
            kind: ArticleFailureKind::NotFound,
            server_id: server_id.into(),
            message: format!(
                "Article explicitly not found on every eligible provider; outcomes: {}",
                provider_outcomes.into()
            ),
        }
    }

    /// Catch-all classifier for failures the emit site can't precisely type.
    pub fn other(server_id: impl Into<String>, msg: impl Into<String>) -> Self {
        Self {
            kind: ArticleFailureKind::Other,
            server_id: server_id.into(),
            message: msg.into(),
        }
    }
}

impl std::fmt::Display for ArticleFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "[{}] {} ({})",
            self.kind.as_str(),
            self.message,
            self.server_id
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_article_not_found() {
        let err = NntpError::ArticleNotFound("<msg-1>".into());
        let f = ArticleFailure::from_nntp(&err, "srv-a");
        assert_eq!(f.kind, ArticleFailureKind::NotFound);
        assert_eq!(f.server_id, "srv-a");
    }

    #[test]
    fn classify_service_unavailable_as_server_down() {
        let err = NntpError::ServiceUnavailable("502".into());
        assert_eq!(
            ArticleFailure::from_nntp(&err, "srv-a").kind,
            ArticleFailureKind::ServerDown
        );
    }

    #[test]
    fn classify_auth() {
        let err = NntpError::Auth("482".into());
        assert_eq!(
            ArticleFailure::from_nntp(&err, "srv-a").kind,
            ArticleFailureKind::AuthFailed
        );
    }

    #[test]
    fn classify_io_as_connection_closed() {
        let err = NntpError::Io(std::io::Error::other("eof"));
        assert_eq!(
            ArticleFailure::from_nntp(&err, "srv-a").kind,
            ArticleFailureKind::ConnectionClosed
        );
    }

    #[test]
    fn classify_timeout() {
        let err = NntpError::Timeout("read timeout".into());
        assert_eq!(
            ArticleFailure::from_nntp(&err, "srv-a").kind,
            ArticleFailureKind::Timeout
        );
    }

    #[test]
    fn per_server_classification_is_correct() {
        assert!(ArticleFailureKind::NotFound.is_per_server());
        assert!(ArticleFailureKind::ServerDown.is_per_server());
        assert!(!ArticleFailureKind::DecodeError.is_per_server());
        assert!(!ArticleFailureKind::Other.is_per_server());
    }

    #[test]
    fn counts_toward_hopeless() {
        assert!(ArticleFailureKind::NotFound.counts_toward_hopeless());
        assert!(ArticleFailureKind::DecodeError.counts_toward_hopeless());
        assert!(!ArticleFailureKind::ServerDown.counts_toward_hopeless());
        assert!(!ArticleFailureKind::AuthFailed.counts_toward_hopeless());
    }
}
