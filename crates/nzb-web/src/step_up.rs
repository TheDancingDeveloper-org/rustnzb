//! Step-up re-authentication for sensitive settings.
//!
//! A sensitive write is allowed when the request presents the web password in
//! `X-Confirm-Password` (checked with the same Argon2 path as login) or when
//! this session confirmed that password within the validity window. An API
//! key is never enough: the header is ignored for that credential.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::http::HeaderMap;
use parking_lot::RwLock;

use crate::auth::{CredentialStore, LoginThrottle};
use crate::error::ApiError;

/// Header carrying the web password for one sensitive request.
pub const CONFIRM_HEADER: &str = "x-confirm-password";

/// Default step-up window: five minutes.
pub const DEFAULT_TTL: Duration = Duration::from_secs(300);

/// How the request authenticated, as decided by the auth middleware.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthMethod {
    /// `X-Api-Key`. Cannot satisfy step-up.
    ApiKey,
    /// Bearer access token belonging to `session`.
    Bearer { session: u64 },
    /// HTTP Basic with the web credentials. The password was just presented,
    /// so this request counts as step-up without a stored window.
    Basic,
}

/// Recent successful step-ups, keyed by login session.
pub struct StepUpStore {
    confirmed_at: RwLock<HashMap<u64, Instant>>,
    ttl: Duration,
    now: Arc<dyn Fn() -> Instant + Send + Sync>,
}

impl StepUpStore {
    pub fn new(ttl: Duration) -> Self {
        Self::with_clock(ttl, Arc::new(Instant::now))
    }

    pub fn with_clock(ttl: Duration, now: Arc<dyn Fn() -> Instant + Send + Sync>) -> Self {
        Self {
            confirmed_at: RwLock::new(HashMap::new()),
            ttl,
            now,
        }
    }

    pub fn ttl(&self) -> Duration {
        self.ttl
    }

    pub fn record(&self, session: u64) {
        self.confirmed_at.write().insert(session, (self.now)());
    }

    /// `Some(true)` while the session's confirmation is inside the window.
    pub fn fresh(&self, session: u64) -> bool {
        let now = (self.now)();
        self.confirmed_at
            .read()
            .get(&session)
            .is_some_and(|at| now.saturating_duration_since(*at) < self.ttl)
    }
}

impl Default for StepUpStore {
    fn default() -> Self {
        Self::new(DEFAULT_TTL)
    }
}

/// Refuse a sensitive write unless step-up is satisfied.
///
/// * API key → 403 `step_up_forbidden` (the confirm header is ignored).
/// * HTTP Basic → allowed (the password authenticated this request).
/// * `X-Confirm-Password` verifies against the stored Argon2 hash. Success
///   records the session and allows the write. A wrong password is counted
///   on `throttle` the same way a failed login is: the answer is held, and
///   a correct password later clears that client. Callers without a peer
///   pass `client = None`, which still shares one bucket.
/// * Otherwise a confirmation younger than the window allows the write.
/// * Anything else → 403 `step_up_required`.
pub async fn require_step_up(
    method: AuthMethod,
    headers: &HeaderMap,
    step_up: &StepUpStore,
    credentials: &Arc<CredentialStore>,
    throttle: &LoginThrottle,
    client: Option<std::net::IpAddr>,
) -> Result<(), ApiError> {
    match method {
        AuthMethod::ApiKey => Err(ApiError::step_up_forbidden()),
        AuthMethod::Basic => Ok(()),
        AuthMethod::Bearer { session } => {
            if let Some(password) = headers
                .get(CONFIRM_HEADER)
                .and_then(|value| value.to_str().ok())
            {
                let password = password.to_owned();
                let ok = credentials
                    .run_hashing(move |store| store.confirm_password(&password))
                    .await
                    .unwrap_or(false);
                if ok {
                    throttle.record_success(client);
                    step_up.record(session);
                    return Ok(());
                }
                let delay = throttle.record_failure(client, Instant::now());
                if !delay.is_zero() {
                    tokio::time::sleep(delay).await;
                }
                return Err(ApiError::step_up_required());
            }
            if step_up.fresh(session) {
                Ok(())
            } else {
                Err(ApiError::step_up_required())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn clock(secs: Arc<AtomicU64>) -> Arc<dyn Fn() -> Instant + Send + Sync> {
        let origin = Instant::now();
        Arc::new(move || origin + Duration::from_secs(secs.load(Ordering::Relaxed)))
    }

    #[test]
    fn window_is_required_then_satisfied_then_expired() {
        let secs = Arc::new(AtomicU64::new(1_000));
        let store = StepUpStore::with_clock(Duration::from_secs(300), clock(secs.clone()));
        assert!(!store.fresh(7), "required before any confirmation");
        store.record(7);
        assert!(store.fresh(7), "satisfied inside the window");
        secs.store(1_000 + 299, Ordering::Relaxed);
        assert!(store.fresh(7));
        secs.store(1_000 + 300, Ordering::Relaxed);
        assert!(!store.fresh(7), "expired at the window boundary");
        assert!(!store.fresh(8), "another session is not covered");
    }

    #[tokio::test]
    async fn header_satisfies_and_a_wrong_password_does_not() {
        let temp = tempfile::tempdir().unwrap();
        let store = Arc::new(CredentialStore::new(temp.path().to_path_buf()));
        store
            .set_credentials(crate::auth::StoredCredentials {
                username: "alice".into(),
                password: "correct horse".into(),
            })
            .unwrap();
        let step = StepUpStore::new(Duration::from_secs(300));
        let mut throttle = LoginThrottle::default();
        throttle.base_delay = Duration::ZERO;
        let mut headers = HeaderMap::new();
        headers.insert(CONFIRM_HEADER, "wrong".parse().unwrap());
        let err = require_step_up(
            AuthMethod::Bearer { session: 4 },
            &headers,
            &step,
            &store,
            &throttle,
            None,
        )
        .await
        .unwrap_err();
        assert_eq!(err.status(), http::StatusCode::FORBIDDEN);
        assert!(err.to_string().contains("step-up re-authentication"));
        assert!(!step.fresh(4));

        headers.insert(CONFIRM_HEADER, "correct horse".parse().unwrap());
        require_step_up(
            AuthMethod::Bearer { session: 4 },
            &headers,
            &step,
            &store,
            &throttle,
            None,
        )
        .await
        .unwrap();
        assert!(step.fresh(4));
    }

    #[tokio::test]
    async fn api_key_cannot_step_up_even_with_the_password() {
        let temp = tempfile::tempdir().unwrap();
        let store = Arc::new(CredentialStore::new(temp.path().to_path_buf()));
        store
            .set_credentials(crate::auth::StoredCredentials {
                username: "alice".into(),
                password: "correct horse".into(),
            })
            .unwrap();
        let step = StepUpStore::new(Duration::from_secs(300));
        let mut headers = HeaderMap::new();
        headers.insert(CONFIRM_HEADER, "correct horse".parse().unwrap());
        let throttle = LoginThrottle::default();
        let err = require_step_up(AuthMethod::ApiKey, &headers, &step, &store, &throttle, None)
            .await
            .unwrap_err();
        assert_eq!(err.status(), http::StatusCode::FORBIDDEN);
        assert!(err.to_string().contains("API-key"));
        require_step_up(AuthMethod::Basic, &headers, &step, &store, &throttle, None)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn repeated_wrong_confirm_passwords_are_throttled() {
        let temp = tempfile::tempdir().unwrap();
        let store = Arc::new(CredentialStore::new(temp.path().to_path_buf()));
        store
            .set_credentials(crate::auth::StoredCredentials {
                username: "alice".into(),
                password: "correct horse".into(),
            })
            .unwrap();
        let step = StepUpStore::new(Duration::from_secs(300));
        let mut throttle = LoginThrottle::default();
        throttle.base_delay = Duration::from_millis(40);
        let client = Some(std::net::IpAddr::from([203, 0, 113, 9]));
        let mut headers = HeaderMap::new();
        headers.insert(CONFIRM_HEADER, "wrong".parse().unwrap());
        let started = Instant::now();
        for _ in 0..2 {
            let err = require_step_up(
                AuthMethod::Bearer { session: 4 },
                &headers,
                &step,
                &store,
                &throttle,
                client,
            )
            .await
            .unwrap_err();
            assert_eq!(err.status(), http::StatusCode::FORBIDDEN);
        }
        // First failure holds 40ms, the second 80ms.
        assert!(started.elapsed() >= Duration::from_millis(110));
        assert_eq!(throttle.failures(client), Some(2));
        assert!(!step.fresh(4));

        headers.insert(CONFIRM_HEADER, "correct horse".parse().unwrap());
        let started = Instant::now();
        require_step_up(
            AuthMethod::Bearer { session: 4 },
            &headers,
            &step,
            &store,
            &throttle,
            client,
        )
        .await
        .unwrap();
        assert!(started.elapsed() < Duration::from_millis(40));
        assert_eq!(throttle.failures(client), None);
        assert!(step.fresh(4));
    }
}
