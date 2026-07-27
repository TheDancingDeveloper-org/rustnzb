use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::RwLock;
use serde::{Deserialize, Serialize};

const ACCESS_TOKEN_TTL: Duration = Duration::from_secs(15 * 60); // 15 minutes
const REFRESH_TOKEN_TTL: Duration = Duration::from_secs(30 * 24 * 60 * 60); // 30 days

struct TokenEntry {
    expires_at: Instant,
}

#[derive(Default)]
pub struct TokenStore {
    access_tokens: RwLock<HashMap<String, TokenEntry>>,
    refresh_tokens: RwLock<HashMap<String, TokenEntry>>,
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

#[derive(Deserialize)]
pub struct LogoutRequest {
    pub refresh_token: String,
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
        }
    }

    pub fn create_tokens(&self) -> TokenResponse {
        let access_token = generate_token();
        let refresh_token = generate_token();
        let now = Instant::now();

        self.access_tokens.write().insert(
            access_token.clone(),
            TokenEntry {
                expires_at: now + ACCESS_TOKEN_TTL,
            },
        );
        self.refresh_tokens.write().insert(
            refresh_token.clone(),
            TokenEntry {
                expires_at: now + REFRESH_TOKEN_TTL,
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
        let valid = {
            let tokens = self.refresh_tokens.read();
            tokens
                .get(refresh_token)
                .is_some_and(|entry| entry.expires_at > Instant::now())
        };

        if !valid {
            return None;
        }

        // Revoke the old refresh token (rotation)
        self.refresh_tokens.write().remove(refresh_token);

        Some(self.create_tokens())
    }

    pub fn revoke_refresh_token(&self, refresh_token: &str) {
        self.refresh_tokens.write().remove(refresh_token);
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
    file_path: PathBuf,
}

impl CredentialStore {
    pub fn new(config_dir: PathBuf) -> Self {
        let file_path = config_dir.join("credentials.json");
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
            file_path,
        }
    }

    pub fn has_credentials(&self) -> bool {
        self.credentials.read().is_some()
    }

    pub fn get_credentials(&self) -> Option<StoredCredentials> {
        self.credentials.read().clone()
    }

    pub fn set_credentials(&self, creds: StoredCredentials) -> Result<(), std::io::Error> {
        let json = serde_json::to_string_pretty(&creds).map_err(std::io::Error::other)?;
        // Create parent directory if needed
        if let Some(parent) = self.file_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&self.file_path, &json)?;
        // Set file permissions to owner-only on unix
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&self.file_path, std::fs::Permissions::from_mode(0o600))?;
        }
        *self.credentials.write() = Some(creds);
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
    // Only allow if no credentials exist yet
    if state.credential_store.has_credentials() {
        return (StatusCode::FORBIDDEN, "credentials already configured").into_response();
    }

    if req.username.is_empty() || req.password.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            "username and password are required",
        )
            .into_response();
    }

    match state.credential_store.set_credentials(StoredCredentials {
        username: req.username,
        password: req.password,
    }) {
        Ok(_) => {
            // Create tokens for the new user so they're immediately logged in
            let tokens = state.token_store.create_tokens();
            (StatusCode::OK, Json(tokens)).into_response()
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

    match state.credential_store.set_credentials(new_creds) {
        Ok(_) => StatusCode::OK.into_response(),
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

pub async fn h_auth_logout(
    State(state): State<ApiState>,
    Json(req): Json<LogoutRequest>,
) -> impl IntoResponse {
    state.token_store.revoke_refresh_token(&req.refresh_token);
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
