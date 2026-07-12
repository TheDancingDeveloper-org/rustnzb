//! Error types for the NNTP subsystem.

use thiserror::Error;

/// NNTP-specific errors.
#[derive(Error, Debug)]
pub enum NntpError {
    /// TCP or connection-level failure.
    #[error("Connection error: {0}")]
    Connection(String),

    /// TLS handshake or configuration failure.
    #[error("TLS error: {0}")]
    Tls(String),

    /// Authentication failure (481, 482).
    #[error("Authentication failed: {0}")]
    Auth(String),

    /// Server requires authentication (480).
    #[error("Authentication required: {0}")]
    AuthRequired(String),

    /// The provider explicitly denied access to the requested operation or
    /// article (for example NNTP 403). This is an account/provider policy
    /// failure, not evidence that the article is absent.
    #[error("Permission denied: {0}")]
    PermissionDenied(String),

    /// Service permanently unavailable (502).
    #[error("Service unavailable: {0}")]
    ServiceUnavailable(String),

    /// Article not found (430).
    #[error("Article not found: {0}")]
    ArticleNotFound(String),

    /// No such newsgroup (411).
    #[error("No such group: {0}")]
    NoSuchGroup(String),

    /// No article selected / no article in group (412, 420).
    #[error("No article selected: {0}")]
    NoArticleSelected(String),

    /// NNTP protocol violation or unexpected response.
    #[error("Protocol error: {0}")]
    Protocol(String),

    /// Underlying I/O error.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// The connection pool has no available connections.
    #[error("No connections available for server {0}")]
    NoConnectionsAvailable(String),

    /// A timeout expired.
    #[error("Timeout: {0}")]
    Timeout(String),

    /// All servers have been tried for this article.
    #[error("All servers exhausted for article {0}")]
    AllServersExhausted(String),

    /// The downloader has been shut down.
    #[error("Downloader shut down")]
    Shutdown,
}

pub type NntpResult<T> = std::result::Result<T, NntpError>;

/// Classify an unexpected response to an article command.
///
/// A `400` response that says the session or idle timer expired is a dead
/// connection, not an article-level protocol failure.  Keeping this
/// classification in the NNTP layer lets every dispatcher invalidate the
/// socket and replay its in-flight commands without consuming provider
/// availability attempts.
pub(crate) fn unexpected_article_response(code: u16, message: String) -> NntpError {
    let normalized = message.to_ascii_lowercase();
    let expired_session = code == 400
        && ((normalized.contains("idle") && normalized.contains("timeout"))
            || (normalized.contains("session")
                && (normalized.contains("timeout")
                    || normalized.contains("expired")
                    || normalized.contains("closed"))));

    if expired_session {
        NntpError::Connection(format!("NNTP session expired ({code}): {message}"))
    } else {
        NntpError::Protocol(format!("Unexpected ARTICLE response {code}: {message}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_timeout_is_a_connection_failure() {
        assert!(matches!(
            unexpected_article_response(400, "Idle timeout.".into()),
            NntpError::Connection(_)
        ));
        assert!(matches!(
            unexpected_article_response(400, "Session expired".into()),
            NntpError::Connection(_)
        ));
    }

    #[test]
    fn unrelated_unexpected_response_remains_protocol_failure() {
        assert!(matches!(
            unexpected_article_response(400, "Bad command".into()),
            NntpError::Protocol(_)
        ));
        assert!(matches!(
            unexpected_article_response(499, "Idle timeout".into()),
            NntpError::Protocol(_)
        ));
    }
}
