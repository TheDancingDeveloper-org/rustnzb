//! Maps [`NntpError`] variants to the penalty window we apply against the
//! originating [`Server`].
//!
//! Splitting this from `server.rs` keeps the protocol-specific knowledge
//! (which NNTP errors warrant which cooldowns) in one place, testable
//! without having to stand up a real `Server`. The caller invokes
//! [`penalty_for_error`] after a failed fetch and passes the result to
//! [`super::server::Server::apply_penalty`] (or
//! [`super::server::Server::register_failure`] for bad_cons-driven
//! gating).
//!
//! Categories:
//!
//! - **Connection errors** (TCP refused, TLS handshake, socket dropped):
//!   short penalty so we back off during a transient network hiccup but
//!   recover quickly once the provider is reachable again.
//! - **Protocol / service unavailable** (`502`, unexpected codes): medium
//!   penalty; this typically means the server is load-shedding.
//! - **Authentication** (480/481/482, `AuthRequired`): longer penalty so we
//!   don't lock out the account with repeated bad-auth attempts, and so
//!   operator-side fixes (password rotation) have time to propagate.
//! - **Timeouts** (read-line, multi-line body): medium penalty — usually
//!   a sign that the specific connection or the specific article-server
//!   pairing is stuck, not the whole server.
//! - **Article-level errors** (`ArticleNotFound`, `NoArticleSelected`):
//!   NOT penalised — the article is simply unavailable on this server,
//!   which is handled by the try-list (not by a server-wide cooldown).

use std::time::Duration;

use nzb_nntp::error::NntpError;

use crate::server::{DEFAULT_PENALTY, PENALTY_502, PENALTY_AUTH};

/// Short cooldown after a TCP/TLS-level failure. We don't want to hammer
/// a network-level problem at sub-second intervals; a few seconds gives
/// the upstream routing / DNS / TLS layer time to recover.
pub const PENALTY_CONNECTION: Duration = Duration::from_secs(5);

/// Timeout cooldown. Long enough that the specific slow connection is
/// dropped and not retried immediately; short enough that we don't lose
/// the whole server for extended periods of mild slowness.
pub const PENALTY_TIMEOUT: Duration = Duration::from_secs(10);

/// Classification of how a fetch error should be handled at the server
/// level, after the article-level (try-list) bookkeeping runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PenaltyAction {
    /// No server-level penalty. Continue using this server — the failure
    /// was article-specific (not-found, rejected message-id, etc.).
    None,
    /// Apply a fixed penalty window of the given duration.
    Cooldown(Duration),
    /// Bump bad_cons and, if the threshold is crossed, apply the given
    /// duration. Used for flaky-but-not-dead failure modes where a single
    /// event shouldn't penalise the server but repeated events should.
    BadCons(Duration),
}

/// Decide what penalty (if any) to apply to the server responsible for
/// `err`.
pub fn penalty_for_error(err: &NntpError) -> PenaltyAction {
    use NntpError::*;
    match err {
        // Article-specific: never penalise the server.
        ArticleNotFound(_) | NoSuchGroup(_) | NoArticleSelected(_) => PenaltyAction::None,

        // Auth failure: apply the long cooldown immediately.
        AuthRequired(_) | Auth(_) | PermissionDenied(_) => PenaltyAction::Cooldown(PENALTY_AUTH),

        // Service permanently unavailable (welcome 502, etc.): medium.
        ServiceUnavailable(_) => PenaltyAction::Cooldown(PENALTY_502),

        // Transport-level hiccup: short cooldown (network), but only after
        // bad_cons crosses the threshold so single blips are tolerated.
        Io(_) | Connection(_) | Tls(_) => PenaltyAction::BadCons(PENALTY_CONNECTION),

        // Timeouts and unexpected protocol responses: medium cooldown
        // after bad_cons threshold. A single slow article shouldn't
        // penalise the whole server, but a run of timeouts should.
        Timeout(_) | Protocol(_) => PenaltyAction::BadCons(PENALTY_TIMEOUT),

        // Pool/orchestrator-side errors — shouldn't flow through this
        // path, but if they do, take the conservative default penalty.
        NoConnectionsAvailable(_) | AllServersExhausted(_) | Shutdown => {
            PenaltyAction::Cooldown(DEFAULT_PENALTY)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn article_level_errors_are_never_penalised() {
        assert_eq!(
            penalty_for_error(&NntpError::ArticleNotFound("m".into())),
            PenaltyAction::None
        );
        assert_eq!(
            penalty_for_error(&NntpError::NoSuchGroup("g".into())),
            PenaltyAction::None
        );
        assert_eq!(
            penalty_for_error(&NntpError::NoArticleSelected("".into())),
            PenaltyAction::None
        );
    }

    #[test]
    fn auth_is_instant_long_cooldown() {
        assert_eq!(
            penalty_for_error(&NntpError::AuthRequired("".into())),
            PenaltyAction::Cooldown(PENALTY_AUTH)
        );
        assert_eq!(
            penalty_for_error(&NntpError::Auth("bad pw".into())),
            PenaltyAction::Cooldown(PENALTY_AUTH)
        );
    }

    #[test]
    fn service_unavailable_is_502_cooldown() {
        assert_eq!(
            penalty_for_error(&NntpError::ServiceUnavailable("".into())),
            PenaltyAction::Cooldown(PENALTY_502)
        );
    }

    #[test]
    fn connection_errors_bump_bad_cons() {
        assert_eq!(
            penalty_for_error(&NntpError::Connection("refused".into())),
            PenaltyAction::BadCons(PENALTY_CONNECTION)
        );
        assert_eq!(
            penalty_for_error(&NntpError::Tls("handshake".into())),
            PenaltyAction::BadCons(PENALTY_CONNECTION)
        );
    }

    #[test]
    fn timeouts_bump_bad_cons() {
        assert_eq!(
            penalty_for_error(&NntpError::Timeout("read".into())),
            PenaltyAction::BadCons(PENALTY_TIMEOUT)
        );
        assert_eq!(
            penalty_for_error(&NntpError::Protocol("weird response".into())),
            PenaltyAction::BadCons(PENALTY_TIMEOUT)
        );
    }
}
