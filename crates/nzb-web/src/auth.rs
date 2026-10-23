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
use utoipa::ToSchema;

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

#[derive(Serialize, ToSchema)]
pub struct TokenResponse {
    pub access_token: String,
    pub refresh_token: String,
    pub token_type: &'static str,
    pub expires_in: u64,
}

#[derive(Deserialize, ToSchema)]
pub struct LoginRequest {
    pub username: String,
    pub password: String,
}

#[derive(Deserialize, ToSchema)]
pub struct RefreshRequest {
    pub refresh_token: String,
}

#[derive(Deserialize, Default, ToSchema)]
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

/// Credentials as the user supplies them. Only the username and an Argon2id
/// hash of the password are ever stored; the plaintext is not kept.
#[derive(Clone)]
pub struct StoredCredentials {
    pub username: String,
    pub password: String,
}

/// The stored account: the username and the Argon2id PHC string of the
/// password. Deliberately neither `Debug` nor returned from the store, so the
/// hash cannot reach a log line or a response.
struct Account {
    username: String,
    password_hash: String,
}

/// On-disk layout of `credentials.json`. Files written before WI-2278 hold a
/// plaintext `password`; loading migrates them to `password_hash`.
#[derive(Serialize, Deserialize)]
struct CredentialsFile {
    username: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    password_hash: Option<String>,
    #[serde(default, skip_serializing)]
    password: Option<String>,
}

/// Why a credential change was refused.
#[derive(Debug)]
pub enum ChangeCredentialsError {
    NotConfigured,
    WrongPassword,
    Empty,
    Io(std::io::Error),
}

fn hash_password(password: &str) -> Result<String, std::io::Error> {
    use argon2::password_hash::PasswordHasher;
    argon2::Argon2::default()
        .hash_password(password.as_bytes())
        .map(|hash| hash.to_string())
        .map_err(|error| std::io::Error::other(format!("failed to hash password: {error}")))
}

/// Verify `password` against an Argon2id PHC string. The comparison of the
/// derived hash is constant time (argon2 compares the output with `ctutils`).
fn verify_password(password: &str, password_hash: &str) -> bool {
    use argon2::password_hash::{PasswordVerifier, phc::PasswordHash};
    PasswordHash::new(password_hash).is_ok_and(|parsed| {
        argon2::Argon2::default()
            .verify_password(password.as_bytes(), &parsed)
            .is_ok()
    })
}

pub struct CredentialStore {
    account: RwLock<Option<Account>>,
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
        let file: Option<CredentialsFile> = std::fs::read_to_string(&file_path)
            .ok()
            .and_then(|contents| serde_json::from_str(&contents).ok());
        let store = Self {
            account: RwLock::new(None),
            #[cfg(not(unix))]
            file_path,
            #[cfg(unix)]
            directory,
        };
        if let Some(file) = file {
            store.load(file);
        }
        store
    }

    /// Install the loaded file, migrating a plaintext password to a hash and
    /// rewriting the file so the plaintext leaves the disk at startup.
    fn load(&self, file: CredentialsFile) {
        let password_hash = match (file.password_hash, file.password) {
            (Some(hash), _) => hash,
            (None, Some(plaintext)) => {
                let hash = match hash_password(&plaintext) {
                    Ok(hash) => hash,
                    Err(error) => {
                        tracing::error!("Could not migrate the stored web login password: {error}");
                        return;
                    }
                };
                match self.persist_account(&file.username, &hash) {
                    Ok(()) => {
                        tracing::info!("Migrated the stored web login password to an Argon2id hash")
                    }
                    Err(error) => tracing::warn!(
                        "Could not rewrite credentials.json with the hashed password \
                         (plaintext stays on disk until the next successful write): {error}"
                    ),
                }
                hash
            }
            (None, None) => return,
        };
        *self.account.write() = Some(Account {
            username: file.username,
            password_hash,
        });
    }

    pub fn has_credentials(&self) -> bool {
        self.account.read().is_some()
    }

    /// The configured username, if any. The password hash is never exposed.
    pub fn username(&self) -> Option<String> {
        self.account.read().as_ref().map(|a| a.username.clone())
    }

    fn persist_account(&self, username: &str, password_hash: &str) -> Result<(), std::io::Error> {
        let file = CredentialsFile {
            username: username.to_owned(),
            password_hash: Some(password_hash.to_owned()),
            password: None,
        };
        let json = serde_json::to_string_pretty(&file).map_err(std::io::Error::other)?;
        self.persist(json.as_bytes())
    }

    /// Write `json` to a temporary file in the data directory, fsync it,
    /// rename it over `credentials.json` and fsync the directory, so a crash
    /// leaves either the old file or the new one. Both are owner-only.
    fn persist(&self, json: &[u8]) -> Result<(), std::io::Error> {
        #[cfg(unix)]
        {
            // The directory handle is opened from the canonical data
            // directory at startup. The fixed filenames never come from a
            // request or configuration value, and O_NOFOLLOW | O_EXCL prevent
            // a planted symlink from redirecting the write.
            let dir = self.directory.as_raw_fd();
            let tmp = c"credentials.json.tmp";
            // A stale temp file from an interrupted write is removed first;
            // writes are serialized by the account lock.
            unsafe { libc::unlinkat(dir, tmp.as_ptr(), 0) };
            let flags =
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_CLOEXEC | libc::O_NOFOLLOW;
            let fd = unsafe { libc::openat(dir, tmp.as_ptr(), flags, 0o600) };
            if fd < 0 {
                return Err(std::io::Error::last_os_error());
            }
            let mut file = unsafe { File::from_raw_fd(fd) };
            let written = (|| {
                file.write_all(json)?;
                if unsafe { libc::fchmod(file.as_raw_fd(), 0o600) } != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                file.sync_all()
            })();
            drop(file);
            if let Err(error) = written {
                unsafe { libc::unlinkat(dir, tmp.as_ptr(), 0) };
                return Err(error);
            }
            if unsafe { libc::renameat(dir, tmp.as_ptr(), dir, c"credentials.json".as_ptr()) } != 0
            {
                let error = std::io::Error::last_os_error();
                unsafe { libc::unlinkat(dir, tmp.as_ptr(), 0) };
                return Err(error);
            }
            self.directory.sync_all()
        }

        #[cfg(not(unix))]
        {
            let tmp = self.file_path.with_extension("json.tmp");
            std::fs::write(&tmp, json)?;
            std::fs::rename(&tmp, &self.file_path)
        }
    }

    /// Replace the credentials, hashing the new password.
    pub fn set_credentials(&self, creds: StoredCredentials) -> Result<(), std::io::Error> {
        let password_hash = hash_password(&creds.password)?;
        let mut account = self.account.write();
        self.persist_account(&creds.username, &password_hash)?;
        *account = Some(Account {
            username: creds.username,
            password_hash,
        });
        Ok(())
    }

    /// Set credentials exactly once. The check and write are serialized so
    /// two first-boot setup requests cannot race into different accounts.
    pub fn initialize_credentials(&self, creds: StoredCredentials) -> Result<(), std::io::Error> {
        let password_hash = hash_password(&creds.password)?;
        let mut current = self.account.write();
        if current.is_some() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "credentials already configured",
            ));
        }
        self.persist_account(&creds.username, &password_hash)?;
        *current = Some(Account {
            username: creds.username,
            password_hash,
        });
        Ok(())
    }

    /// Verify `current_password`, then replace the username and/or password.
    /// A `None` field keeps its current value (the password keeps its hash).
    pub fn change_credentials(
        &self,
        current_password: &str,
        new_username: Option<String>,
        new_password: Option<String>,
    ) -> Result<(), ChangeCredentialsError> {
        let (username, current_hash) = match &*self.account.read() {
            Some(account) => (account.username.clone(), account.password_hash.clone()),
            None => return Err(ChangeCredentialsError::NotConfigured),
        };
        if !verify_password(current_password, &current_hash) {
            return Err(ChangeCredentialsError::WrongPassword);
        }
        let username = new_username.unwrap_or(username);
        if username.is_empty() || new_password.as_deref().is_some_and(str::is_empty) {
            return Err(ChangeCredentialsError::Empty);
        }
        let password_hash = match new_password {
            Some(password) => hash_password(&password).map_err(ChangeCredentialsError::Io)?,
            None => current_hash.clone(),
        };
        let mut account = self.account.write();
        // Refuse if another change landed while the password was verified.
        if account.as_ref().map(|a| a.password_hash.as_str()) != Some(current_hash.as_str()) {
            return Err(ChangeCredentialsError::WrongPassword);
        }
        self.persist_account(&username, &password_hash)
            .map_err(ChangeCredentialsError::Io)?;
        *account = Some(Account {
            username,
            password_hash,
        });
        Ok(())
    }

    /// Check a username and password. The username comparison is constant
    /// time, and the Argon2 verification runs even when the username is wrong
    /// so timing does not reveal which part failed. Argon2 is deliberately
    /// slow: async callers use [`CredentialStore::validate_async`].
    pub fn validate(&self, username: &str, password: &str) -> bool {
        let (username_ok, password_hash) = match &*self.account.read() {
            Some(account) => (
                constant_time_eq(username.as_bytes(), account.username.as_bytes()),
                account.password_hash.clone(),
            ),
            None => return false,
        };
        let password_ok = verify_password(password, &password_hash);
        username_ok & password_ok
    }

    /// [`CredentialStore::validate`] on the blocking thread pool, so the
    /// Argon2 work does not stall the async runtime.
    pub async fn validate_async(self: Arc<Self>, username: String, password: String) -> bool {
        tokio::task::spawn_blocking(move || self.validate(&username, &password))
            .await
            .unwrap_or(false)
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

#[derive(Serialize, ToSchema)]
pub struct AuthStatus {
    pub auth_enabled: bool,
    pub setup_required: bool,
}

#[utoipa::path(
    get,
    path = "/api/auth/status",
    tag = "auth",
    summary = "Authentication status",
    description = "Reports whether credentials are configured and whether the first-boot setup wizard is required.",
    responses(
        (status = 200, description = "Authentication state", body = AuthStatus),
    ),
)]
pub async fn h_auth_status(State(state): State<ApiState>) -> impl IntoResponse {
    let has_stored_creds = state.credential_store.has_credentials();
    Json(AuthStatus {
        auth_enabled: has_stored_creds,
        setup_required: !has_stored_creds,
    })
}

// --- Auth Setup (first-boot) ---

#[derive(Deserialize, ToSchema)]
pub struct SetupRequest {
    pub username: String,
    pub password: String,
}

#[utoipa::path(
    post,
    path = "/api/auth/setup",
    tag = "auth",
    summary = "Create the first-boot credentials",
    description = "Stores the initial username and password and returns a token pair. Refused once credentials exist.",
    request_body = SetupRequest,
    responses(
        (status = 200, description = "Credentials created; tokens issued", body = TokenResponse),
        (status = 400, description = "Username or password missing", body = String, content_type = "text/plain"),
        (status = 403, description = "Credentials are already configured", body = String, content_type = "text/plain"),
        (status = 500, description = "Credentials could not be saved", body = String, content_type = "text/plain"),
    ),
)]
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

    let store = state.credential_store.clone();
    let result = tokio::task::spawn_blocking(move || {
        store.initialize_credentials(StoredCredentials {
            username: req.username,
            password: req.password,
        })
    })
    .await
    .unwrap_or_else(|error| Err(std::io::Error::other(error)));
    match result {
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

#[derive(Deserialize, ToSchema)]
pub struct ChangeCredentialsRequest {
    pub current_password: String,
    pub new_username: Option<String>,
    pub new_password: Option<String>,
}

#[utoipa::path(
    post,
    path = "/api/auth/change_credentials",
    tag = "auth",
    summary = "Change the credentials",
    description = "Verifies the current password, stores the new credentials and revokes every issued token.",
    request_body = ChangeCredentialsRequest,
    responses(
        (status = 200, description = "Credentials changed; all tokens revoked"),
        (status = 400, description = "A new username or password is empty", body = String, content_type = "text/plain"),
        (status = 401, description = "Current password is incorrect", body = String, content_type = "text/plain"),
        (status = 404, description = "No credentials configured", body = String, content_type = "text/plain"),
        (status = 500, description = "Credentials could not be saved", body = String, content_type = "text/plain"),
    ),
)]
pub async fn h_auth_change_credentials(
    State(state): State<ApiState>,
    Json(req): Json<ChangeCredentialsRequest>,
) -> impl IntoResponse {
    let store = state.credential_store.clone();
    let result = tokio::task::spawn_blocking(move || {
        store.change_credentials(&req.current_password, req.new_username, req.new_password)
    })
    .await
    .unwrap_or_else(|error| Err(ChangeCredentialsError::Io(std::io::Error::other(error))));

    match result {
        Ok(()) => {
            state.token_store.revoke_all();
            StatusCode::OK.into_response()
        }
        Err(ChangeCredentialsError::NotConfigured) => {
            (StatusCode::NOT_FOUND, "no credentials configured").into_response()
        }
        Err(ChangeCredentialsError::WrongPassword) => {
            (StatusCode::UNAUTHORIZED, "current password is incorrect").into_response()
        }
        Err(ChangeCredentialsError::Empty) => (
            StatusCode::BAD_REQUEST,
            "username and password cannot be empty",
        )
            .into_response(),
        Err(ChangeCredentialsError::Io(e)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("failed to save credentials: {e}"),
        )
            .into_response(),
    }
}

// --- Login ---

#[utoipa::path(
    post,
    path = "/api/auth/login",
    tag = "auth",
    summary = "Log in",
    description = "Exchanges a username and password for an access token and a refresh token.",
    request_body = LoginRequest,
    responses(
        (status = 200, description = "Credentials accepted; tokens issued", body = TokenResponse),
        (status = 401, description = "Invalid credentials", body = String, content_type = "text/plain"),
        (status = 404, description = "Authentication is not configured", body = String, content_type = "text/plain"),
    ),
)]
pub async fn h_auth_login(
    State(state): State<ApiState>,
    Json(req): Json<LoginRequest>,
) -> impl IntoResponse {
    if !state.credential_store.has_credentials() {
        return (StatusCode::NOT_FOUND, "authentication not configured").into_response();
    }

    if !state
        .credential_store
        .clone()
        .validate_async(req.username, req.password)
        .await
    {
        return (StatusCode::UNAUTHORIZED, "invalid credentials").into_response();
    }

    state.token_store.cleanup_expired();
    let tokens = state.token_store.create_tokens();
    (StatusCode::OK, Json(tokens)).into_response()
}

// --- Refresh ---

#[utoipa::path(
    post,
    path = "/api/auth/refresh",
    tag = "auth",
    summary = "Refresh tokens",
    description = "Redeems a refresh token once and returns a new token pair.",
    request_body = RefreshRequest,
    responses(
        (status = 200, description = "New token pair", body = TokenResponse),
        (status = 401, description = "Refresh token invalid or expired", body = String, content_type = "text/plain"),
    ),
)]
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
#[utoipa::path(
    post,
    path = "/api/auth/logout",
    tag = "auth",
    summary = "Log out",
    description = "Revokes the login session of the refresh token in the body and of the `Authorization: Bearer` access token, if either is supplied.",
    request_body = LogoutRequest,
    responses(
        (status = 204, description = "Session revoked"),
    ),
)]
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

    #[cfg(unix)]
    fn mode(path: &std::path::Path) -> u32 {
        std::os::unix::fs::PermissionsExt::mode(&std::fs::metadata(path).unwrap().permissions())
            & 0o777
    }

    fn alice() -> StoredCredentials {
        StoredCredentials {
            username: "alice".into(),
            password: "correct horse".into(),
        }
    }

    #[test]
    fn credential_store_persists_an_owner_only_argon2id_hash() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("credentials.json");
        let store = CredentialStore::new(temp.path().to_path_buf());
        assert!(!store.has_credentials());
        store.set_credentials(alice()).unwrap();
        assert!(store.validate("alice", "correct horse"));

        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(!contents.contains("correct horse"), "{contents}");
        let file: serde_json::Value = serde_json::from_str(&contents).unwrap();
        assert!(file.get("password").is_none());
        assert!(
            file["password_hash"]
                .as_str()
                .unwrap()
                .starts_with("$argon2id$v=19$")
        );
        assert!(!temp.path().join("credentials.json.tmp").exists());
        #[cfg(unix)]
        assert_eq!(mode(&path), 0o600);

        let reloaded = CredentialStore::new(temp.path().to_path_buf());
        assert!(reloaded.validate("alice", "correct horse"));
        assert_eq!(reloaded.username().as_deref(), Some("alice"));
    }

    #[test]
    fn hashing_salts_each_password() {
        let first = hash_password("same").unwrap();
        let second = hash_password("same").unwrap();
        assert_ne!(first, second);
        assert!(verify_password("same", &first));
        assert!(verify_password("same", &second));
        assert!(!verify_password("same", "not a phc string"));
    }

    #[test]
    fn validate_rejects_a_wrong_password_or_username() {
        let temp = tempfile::tempdir().unwrap();
        let store = CredentialStore::new(temp.path().to_path_buf());
        assert!(!store.validate("alice", "correct horse"));
        store.set_credentials(alice()).unwrap();
        assert!(!store.validate("alice", "wrong"));
        assert!(!store.validate("alice", ""));
        assert!(!store.validate("alice", "correct horse "));
        assert!(!store.validate("bob", "correct horse"));
        assert!(!store.validate("", ""));
        assert!(!constant_time_eq(b"same", b"different"));
        assert!(!constant_time_eq(b"same", b"sane"));
        assert!(constant_time_eq(b"same", b"same"));
    }

    #[test]
    fn plaintext_credentials_are_migrated_at_startup() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("credentials.json");
        std::fs::write(&path, r#"{"username":"alice","password":"correct horse"}"#).unwrap();
        #[cfg(unix)]
        std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o644))
            .unwrap();

        let store = CredentialStore::new(temp.path().to_path_buf());
        assert!(store.validate("alice", "correct horse"));
        assert!(!store.validate("alice", "wrong"));

        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(!contents.contains("correct horse"), "{contents}");
        assert!(contents.contains("$argon2id$"));
        #[cfg(unix)]
        assert_eq!(mode(&path), 0o600);

        // The migrated file loads as a hash and is not rewritten again.
        let reloaded = CredentialStore::new(temp.path().to_path_buf());
        assert!(reloaded.validate("alice", "correct horse"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), contents);
    }

    #[test]
    fn change_credentials_verifies_the_current_password() {
        let temp = tempfile::tempdir().unwrap();
        let store = CredentialStore::new(temp.path().to_path_buf());
        assert!(matches!(
            store.change_credentials("x", None, None),
            Err(ChangeCredentialsError::NotConfigured)
        ));
        store.set_credentials(alice()).unwrap();

        assert!(matches!(
            store.change_credentials("wrong", None, Some("new".into())),
            Err(ChangeCredentialsError::WrongPassword)
        ));
        assert!(matches!(
            store.change_credentials("correct horse", None, Some(String::new())),
            Err(ChangeCredentialsError::Empty)
        ));
        assert!(store.validate("alice", "correct horse"));

        // Renaming keeps the existing password.
        store
            .change_credentials("correct horse", Some("bob".into()), None)
            .unwrap();
        assert!(store.validate("bob", "correct horse"));
        assert!(!store.validate("alice", "correct horse"));

        store
            .change_credentials("correct horse", None, Some("battery staple".into()))
            .unwrap();
        assert!(store.validate("bob", "battery staple"));
        assert!(!store.validate("bob", "correct horse"));

        let reloaded = CredentialStore::new(temp.path().to_path_buf());
        assert!(reloaded.validate("bob", "battery staple"));
        #[cfg(unix)]
        assert_eq!(mode(&temp.path().join("credentials.json")), 0o600);
    }

    #[cfg(unix)]
    #[test]
    fn persist_does_not_follow_a_planted_temp_symlink() {
        let temp = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let target = outside.path().join("stolen");
        std::os::unix::fs::symlink(&target, temp.path().join("credentials.json.tmp")).unwrap();
        let store = CredentialStore::new(temp.path().to_path_buf());
        store.set_credentials(alice()).unwrap();
        assert!(!target.exists());
        assert!(store.validate("alice", "correct horse"));
    }
}
