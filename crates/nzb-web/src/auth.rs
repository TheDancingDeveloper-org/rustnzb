use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::fs::File;
#[cfg(unix)]
use std::io::Write;
#[cfg(unix)]
use std::os::fd::{AsRawFd, FromRawFd};

use parking_lot::RwLock;
use serde::{Deserialize, Serialize};

const ACCESS_TOKEN_TTL: Duration = Duration::from_secs(15 * 60); // 15 minutes
const REFRESH_TOKEN_TTL: Duration = Duration::from_secs(30 * 24 * 60 * 60); // 30 days

struct TokenEntry {
    expires_at: Instant,
    /// Login session the token belongs to. Every access and refresh token
    /// issued from one login (including refresh rotations) shares it, so
    /// logout can revoke the whole session.
    session: u64,
}

#[derive(Default)]
pub struct TokenStore {
    access_tokens: RwLock<HashMap<String, TokenEntry>>,
    refresh_tokens: RwLock<HashMap<String, TokenEntry>>,
    next_session: AtomicU64,
}

#[derive(Serialize)]
pub struct TokenResponse {
    pub access_token: String,
    pub refresh_token: String,
    pub token_type: &'static str,
    pub expires_in: u64,
}

#[derive(Deserialize)]
pub struct LoginRequest {
    pub username: String,
    pub password: String,
}

#[derive(Deserialize)]
pub struct RefreshRequest {
    pub refresh_token: String,
}

#[derive(Deserialize, Default)]
pub struct LogoutRequest {
    #[serde(default)]
    pub refresh_token: Option<String>,
}

fn generate_token() -> String {
    let bytes: [u8; 32] = rand::random();
    hex::encode(bytes)
}

impl TokenStore {
    pub fn new() -> Self {
        Self {
            access_tokens: RwLock::new(HashMap::new()),
            refresh_tokens: RwLock::new(HashMap::new()),
            next_session: AtomicU64::new(0),
        }
    }

    /// Issue tokens for a new login session.
    pub fn create_tokens(&self) -> TokenResponse {
        let session = self.next_session.fetch_add(1, Ordering::Relaxed);
        self.issue_tokens(session)
    }

    fn issue_tokens(&self, session: u64) -> TokenResponse {
        let access_token = generate_token();
        let refresh_token = generate_token();
        let now = Instant::now();

        self.access_tokens.write().insert(
            access_token.clone(),
            TokenEntry {
                expires_at: now + ACCESS_TOKEN_TTL,
                session,
            },
        );
        self.refresh_tokens.write().insert(
            refresh_token.clone(),
            TokenEntry {
                expires_at: now + REFRESH_TOKEN_TTL,
                session,
            },
        );

        TokenResponse {
            access_token,
            refresh_token,
            token_type: "Bearer",
            expires_in: ACCESS_TOKEN_TTL.as_secs(),
        }
    }

    pub fn validate_access_token(&self, token: &str) -> bool {
        let tokens = self.access_tokens.read();
        tokens
            .get(token)
            .is_some_and(|entry| entry.expires_at > Instant::now())
    }

    pub fn refresh(&self, refresh_token: &str) -> Option<TokenResponse> {
        // Remove the old refresh token (rotation) under the same lock that
        // checks it, so it can be redeemed only once.
        let session = {
            let mut tokens = self.refresh_tokens.write();
            let entry = tokens.remove(refresh_token)?;
            if entry.expires_at <= Instant::now() {
                return None;
            }
            entry.session
        };

        Some(self.issue_tokens(session))
    }

    pub fn revoke_refresh_token(&self, refresh_token: &str) {
        self.refresh_tokens.write().remove(refresh_token);
    }

    /// End the login session that `token` (an access or refresh token)
    /// belongs to: every access and refresh token issued for that session,
    /// including earlier rotations, stops working.
    pub fn revoke_session_of(&self, token: &str) {
        let session = self
            .access_tokens
            .read()
            .get(token)
            .map(|entry| entry.session)
            .or_else(|| {
                self.refresh_tokens
                    .read()
                    .get(token)
                    .map(|entry| entry.session)
            });
        // Always drop the presented token itself, even if it is unknown.
        self.access_tokens.write().remove(token);
        self.refresh_tokens.write().remove(token);
        if let Some(session) = session {
            self.access_tokens
                .write()
                .retain(|_, entry| entry.session != session);
            self.refresh_tokens
                .write()
                .retain(|_, entry| entry.session != session);
        }
    }

    /// Revoke every session after credentials change.
    pub fn revoke_all(&self) {
        self.access_tokens.write().clear();
        self.refresh_tokens.write().clear();
    }

    pub fn cleanup_expired(&self) {
        let now = Instant::now();
        self.access_tokens
            .write()
            .retain(|_, entry| entry.expires_at > now);
        self.refresh_tokens
            .write()
            .retain(|_, entry| entry.expires_at > now);
    }
}

// --- Credential Store ---

#[derive(Serialize, Deserialize, Clone)]
pub struct StoredCredentials {
    pub username: String,
    pub password: String,
}

pub struct CredentialStore {
    credentials: RwLock<Option<StoredCredentials>>,
    #[cfg(not(unix))]
    file_path: PathBuf,
    #[cfg(unix)]
    directory: File,
}

impl CredentialStore {
    pub fn new(config_dir: PathBuf) -> Self {
        // Startup creates the configured data directory before constructing
        // this store. Canonicalizing it here confines the credential file to
        // that existing directory and removes traversal or symlinked-parent
        // ambiguity from the subsequent writes.
        let config_dir = config_dir.canonicalize().unwrap_or_else(|error| {
            panic!("credential store data directory must exist before startup: {error}")
        });
        let file_path = config_dir.join("credentials.json");
        #[cfg(unix)]
        let directory = File::open(&config_dir).unwrap_or_else(|error| {
            panic!("credential store data directory must be readable: {error}")
        });
        let credentials = if file_path.exists() {
            match std::fs::read_to_string(&file_path) {
                Ok(contents) => serde_json::from_str(&contents).ok(),
                Err(_) => None,
            }
        } else {
            None
        };
        Self {
            credentials: RwLock::new(credentials),
            #[cfg(not(unix))]
            file_path,
            #[cfg(unix)]
            directory,
        }
    }

    pub fn has_credentials(&self) -> bool {
        self.credentials.read().is_some()
    }

    pub fn get_credentials(&self) -> Option<StoredCredentials> {
        self.credentials.read().clone()
    }

    fn persist(&self, json: &[u8]) -> Result<(), std::io::Error> {
        #[cfg(unix)]
        {
            // The directory handle is opened from the canonical data
            // directory at startup. The fixed filename never comes from a
            // request or configuration value, and O_NOFOLLOW prevents a
            // pre-existing credentials symlink from redirecting the write.
            let flags =
                libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC | libc::O_CLOEXEC | libc::O_NOFOLLOW;
            let fd = unsafe {
                libc::openat(
                    self.directory.as_raw_fd(),
                    c"credentials.json".as_ptr(),
                    flags,
                    0o600,
                )
            };
            if fd < 0 {
                return Err(std::io::Error::last_os_error());
            }
            let mut file = unsafe { File::from_raw_fd(fd) };
            file.write_all(json)?;
            if unsafe { libc::fchmod(file.as_raw_fd(), 0o600) } != 0 {
                return Err(std::io::Error::last_os_error());
            }
            file.sync_all()
        }

        #[cfg(not(unix))]
        std::fs::write(&self.file_path, json)
    }

    pub fn set_credentials(&self, creds: StoredCredentials) -> Result<(), std::io::Error> {
        let json = serde_json::to_string_pretty(&creds).map_err(std::io::Error::other)?;
        self.persist(json.as_bytes())?;
        *self.credentials.write() = Some(creds);
        Ok(())
    }

    /// Set credentials exactly once. The check and write are serialized so
    /// two first-boot setup requests cannot race into different accounts.
    pub fn initialize_credentials(&self, creds: StoredCredentials) -> Result<(), std::io::Error> {
        let mut current = self.credentials.write();
        if current.is_some() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "credentials already configured",
            ));
        }
        let json = serde_json::to_string_pretty(&creds).map_err(std::io::Error::other)?;
        self.persist(json.as_bytes())?;
        *current = Some(creds);
        Ok(())
    }

    pub fn validate(&self, username: &str, password: &str) -> bool {
        match &*self.credentials.read() {
            Some(creds) => {
                constant_time_eq(username.as_bytes(), creds.username.as_bytes())
                    && constant_time_eq(password.as_bytes(), creds.password.as_bytes())
            }
            None => false,
        }
    }
}

/// Constant-time byte comparison to prevent timing attacks on auth credentials.
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter()
        .zip(b.iter())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}

// --- HTTP Handlers ---

use axum::Json;
use axum::extract::State;
use axum::response::IntoResponse;
use http::StatusCode;

use crate::state::AppState;

type ApiState = Arc<AppState>;

// --- Auth Status ---

#[derive(Serialize)]
pub struct AuthStatus {
    pub auth_enabled: bool,
    pub setup_required: bool,
}

pub async fn h_auth_status(State(state): State<ApiState>) -> impl IntoResponse {
    let has_stored_creds = state.credential_store.has_credentials();
    Json(AuthStatus {
        auth_enabled: has_stored_creds,
        setup_required: !has_stored_creds,
    })
}

// --- Auth Setup (first-boot) ---

#[derive(Deserialize)]
pub struct SetupRequest {
    pub username: String,
    pub password: String,
}

pub async fn h_auth_setup(
    State(state): State<ApiState>,
    Json(req): Json<SetupRequest>,
) -> impl IntoResponse {
    if req.username.is_empty() || req.password.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            "username and password are required",
        )
            .into_response();
    }

    match state
        .credential_store
        .initialize_credentials(StoredCredentials {
            username: req.username,
            password: req.password,
        }) {
        Ok(_) => {
            // Create tokens for the new user so they're immediately logged in
            let tokens = state.token_store.create_tokens();
            (StatusCode::OK, Json(tokens)).into_response()
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            (StatusCode::FORBIDDEN, "credentials are already configured").into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("failed to save credentials: {e}"),
        )
            .into_response(),
    }
}

// --- Change Credentials ---

#[derive(Deserialize)]
pub struct ChangeCredentialsRequest {
    pub current_password: String,
    pub new_username: Option<String>,
    pub new_password: Option<String>,
}

pub async fn h_auth_change_credentials(
    State(state): State<ApiState>,
    Json(req): Json<ChangeCredentialsRequest>,
) -> impl IntoResponse {
    let current_creds = match state.credential_store.get_credentials() {
        Some(c) => c,
        None => {
            return (StatusCode::NOT_FOUND, "no credentials configured").into_response();
        }
    };

    // Verify current password
    if !constant_time_eq(
        req.current_password.as_bytes(),
        current_creds.password.as_bytes(),
    ) {
        return (StatusCode::UNAUTHORIZED, "current password is incorrect").into_response();
    }

    let new_creds = StoredCredentials {
        username: req.new_username.unwrap_or(current_creds.username),
        password: req.new_password.unwrap_or(current_creds.password),
    };
    if new_creds.username.is_empty() || new_creds.password.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            "username and password cannot be empty",
        )
            .into_response();
    }

    match state.credential_store.set_credentials(new_creds) {
        Ok(_) => {
            state.token_store.revoke_all();
            StatusCode::OK.into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("failed to save credentials: {e}"),
        )
            .into_response(),
    }
}

// --- Login ---

pub async fn h_auth_login(
    State(state): State<ApiState>,
    Json(req): Json<LoginRequest>,
) -> impl IntoResponse {
    if !state.credential_store.has_credentials() {
        return (StatusCode::NOT_FOUND, "authentication not configured").into_response();
    }

    if !state
        .credential_store
        .validate(&req.username, &req.password)
    {
        return (StatusCode::UNAUTHORIZED, "invalid credentials").into_response();
    }

    state.token_store.cleanup_expired();
    let tokens = state.token_store.create_tokens();
    (StatusCode::OK, Json(tokens)).into_response()
}

// --- Refresh ---

pub async fn h_auth_refresh(
    State(state): State<ApiState>,
    Json(req): Json<RefreshRequest>,
) -> impl IntoResponse {
    match state.token_store.refresh(&req.refresh_token) {
        Some(tokens) => (StatusCode::OK, Json(tokens)).into_response(),
        None => (StatusCode::UNAUTHORIZED, "invalid or expired refresh token").into_response(),
    }
}

// --- Logout ---

/// Revoke the caller's session: the refresh token in the body and the
/// access token in the `Authorization: Bearer` header (if any), together
/// with every other token issued for the same login.
pub async fn h_auth_logout(
    State(state): State<ApiState>,
    headers: http::HeaderMap,
    req: Option<Json<LogoutRequest>>,
) -> impl IntoResponse {
    let req = req.map(|Json(req)| req).unwrap_or_default();
    if let Some(refresh_token) = req.refresh_token.as_deref() {
        state.token_store.revoke_session_of(refresh_token);
    }
    if let Some(access_token) = headers
        .get(http::header::AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "))
    {
        state.token_store.revoke_session_of(access_token);
    }
    StatusCode::NO_CONTENT
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_rotation_invalidates_the_previous_refresh_token() {
        let store = TokenStore::new();
        let first = store.create_tokens();
        assert!(store.validate_access_token(&first.access_token));

        let second = store
            .refresh(&first.refresh_token)
            .expect("refresh token is valid");
        assert!(store.validate_access_token(&first.access_token));
        assert!(store.validate_access_token(&second.access_token));
        assert!(store.refresh(&first.refresh_token).is_none());

        store.revoke_refresh_token(&second.refresh_token);
        assert!(store.refresh(&second.refresh_token).is_none());
    }

    #[test]
    fn revoking_a_session_revokes_its_access_tokens_and_rotations() {
        let store = TokenStore::new();
        let first = store.create_tokens();
        let rotated = store.refresh(&first.refresh_token).unwrap();
        let other = store.create_tokens();

        store.revoke_session_of(&rotated.refresh_token);

        assert!(!store.validate_access_token(&first.access_token));
        assert!(!store.validate_access_token(&rotated.access_token));
        assert!(store.refresh(&rotated.refresh_token).is_none());
        // Another login is unaffected.
        assert!(store.validate_access_token(&other.access_token));
        assert!(store.refresh(&other.refresh_token).is_some());
    }

    #[test]
    fn revoking_by_access_token_ends_the_session() {
        let store = TokenStore::new();
        let tokens = store.create_tokens();
        store.revoke_session_of(&tokens.access_token);
        assert!(!store.validate_access_token(&tokens.access_token));
        assert!(store.refresh(&tokens.refresh_token).is_none());
        // Unknown tokens are a no-op.
        store.revoke_session_of("not-a-token");
    }

    #[test]
    fn credential_store_persists_owner_only_credentials_and_validates_in_constant_time() {
        let temp = tempfile::tempdir().unwrap();
        let store = CredentialStore::new(temp.path().to_path_buf());
        assert!(!store.has_credentials());
        store
            .set_credentials(StoredCredentials {
                username: "alice".into(),
                password: "correct horse".into(),
            })
            .unwrap();
        assert!(store.validate("alice", "correct horse"));
        assert!(!store.validate("alice", "wrong"));
        assert!(!constant_time_eq(b"same", b"different"));

        #[cfg(unix)]
        assert_eq!(
            std::os::unix::fs::PermissionsExt::mode(
                &std::fs::metadata(temp.path().join("credentials.json"))
                    .unwrap()
                    .permissions(),
            ) & 0o777,
            0o600,
        );

        let reloaded = CredentialStore::new(temp.path().to_path_buf());
        assert!(reloaded.validate("alice", "correct horse"));
    }
}
