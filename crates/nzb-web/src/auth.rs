use std::collections::{HashMap, VecDeque};
use std::net::{IpAddr, SocketAddr};
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
    /// A wrong current password while locked out.
    Throttled {
        retry_after: Duration,
    },
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

/// Placeholder hash for an account whose stored file could not be loaded or
/// migrated. It never parses as a PHC string, so every login is refused while
/// the account still counts as configured and first-boot setup stays closed.
const LOCKED_HASH: &str = "";

/// Argon2 work (verify or hash) allowed to run at once. Each verification
/// holds about 19 MiB for its duration, so this bounds the memory and the
/// blocking threads that unauthenticated requests can claim.
const HASHING_PERMITS: usize = 3;

/// How long a wrong password from a locked-out client holds the locked lane
/// (see [`CredentialStore::guarded_check`]). This bounds guessing while
/// locked out to about one attempt per hold across all locked clients.
const LOCKED_FAILURE_HOLD: Duration = Duration::from_secs(1);

/// Locked-out attempts that may wait for the locked lane at once. A further
/// attempt turns the oldest waiter away with a 429, unverified, so the queue,
/// and the connections and bodies it holds, stays this small.
const LOCKED_LANE_WAITERS: usize = 8;

/// One attempt at a time from locked-out clients, newest first.
///
/// When the lane frees, the most recent waiter goes next, and joining a full
/// queue turns the oldest waiter away. A fresh attempt, such as the operator
/// typing the correct password, therefore waits for at most the attempt in
/// progress, not behind a backlog of queued guesses. Nothing here can tell
/// the operator from a guesser before Argon2 runs, so a client that keeps
/// sending faster than the lane drains can still push a waiting attempt out;
/// that attempt gets a prompt 429 rather than an unbounded wait.
struct LockedLane {
    state: parking_lot::Mutex<LaneState>,
    max_waiters: usize,
}

#[derive(Default)]
struct LaneState {
    busy: bool,
    /// Oldest at the front, newest at the back.
    waiting: VecDeque<tokio::sync::oneshot::Sender<()>>,
}

/// Holding the locked lane; dropping it hands the lane to the newest waiter.
struct LanePass(Arc<LockedLane>);

impl Drop for LanePass {
    fn drop(&mut self) {
        self.0.release();
    }
}

/// A queued [`LockedLane::enter`]. If the caller is dropped after the lane
/// was handed to it but before it took the pass, the lane is passed on.
struct LaneWaiter {
    lane: Arc<LockedLane>,
    handed: tokio::sync::oneshot::Receiver<()>,
    settled: bool,
}

impl Drop for LaneWaiter {
    fn drop(&mut self) {
        if !self.settled {
            self.handed.close();
            if self.handed.try_recv().is_ok() {
                self.lane.release();
            }
        }
    }
}

impl LockedLane {
    fn new(max_waiters: usize) -> Self {
        Self {
            state: parking_lot::Mutex::new(LaneState::default()),
            max_waiters,
        }
    }

    /// Wait for the lane. `None` if a newer attempt turned this one away.
    async fn enter(self: &Arc<Self>) -> Option<LanePass> {
        let handed = {
            let mut state = self.state.lock();
            if !state.busy {
                state.busy = true;
                return Some(LanePass(self.clone()));
            }
            let (hand, handed) = tokio::sync::oneshot::channel();
            state.waiting.push_back(hand);
            if state.waiting.len() > self.max_waiters {
                // Dropping the sender answers the oldest waiter with `None`.
                state.waiting.pop_front();
            }
            handed
        };
        let mut waiter = LaneWaiter {
            lane: self.clone(),
            handed,
            settled: false,
        };
        let entered = (&mut waiter.handed).await.is_ok();
        waiter.settled = true;
        entered.then(|| LanePass(self.clone()))
    }

    fn release(&self) {
        let mut state = self.state.lock();
        while let Some(hand) = state.waiting.pop_back() {
            if hand.send(()).is_ok() {
                return;
            }
        }
        state.busy = false;
    }

    #[cfg(test)]
    fn waiting(&self) -> usize {
        self.state.lock().waiting.len()
    }
}

pub struct CredentialStore {
    account: RwLock<Option<Account>>,
    hashing: Arc<tokio::sync::Semaphore>,
    locked_lane: Arc<LockedLane>,
    locked_failure_hold: Duration,
    throttle: LoginThrottle,
    #[cfg(not(unix))]
    file_path: PathBuf,
    #[cfg(unix)]
    directory: File,
}

impl CredentialStore {
    pub fn new(config_dir: PathBuf) -> Self {
        Self::open(config_dir, hash_password)
    }

    fn open(config_dir: PathBuf, hasher: fn(&str) -> Result<String, std::io::Error>) -> Self {
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
        let contents = std::fs::read_to_string(&file_path);
        let store = Self {
            account: RwLock::new(None),
            hashing: Arc::new(tokio::sync::Semaphore::new(HASHING_PERMITS)),
            locked_lane: Arc::new(LockedLane::new(LOCKED_LANE_WAITERS)),
            locked_failure_hold: LOCKED_FAILURE_HOLD,
            throttle: LoginThrottle::default(),
            #[cfg(not(unix))]
            file_path: file_path.clone(),
            #[cfg(unix)]
            directory,
        };
        match contents {
            Ok(contents) => match serde_json::from_str::<CredentialsFile>(&contents) {
                Ok(file) => store.load(file, hasher),
                Err(error) => store.lock(
                    String::new(),
                    &format!("{} is not valid: {error}", file_path.display()),
                ),
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => store.lock(
                String::new(),
                &format!("{} cannot be read: {error}", file_path.display()),
            ),
        }
        store
    }

    /// Keep the account configured but refuse every login. Used when the
    /// stored file exists but cannot be used, so a damaged or unmigratable
    /// file never reopens first-boot setup to whoever reaches it first.
    fn lock(&self, username: String, reason: &str) {
        tracing::error!(
            "Web login is disabled: {reason}. Fix or remove credentials.json in the data \
             directory and restart; removing it reopens first-boot setup."
        );
        *self.account.write() = Some(Account {
            username,
            password_hash: LOCKED_HASH.to_owned(),
        });
    }

    /// Install the loaded file, migrating a plaintext password to a hash and
    /// rewriting the file so the plaintext leaves the disk at startup.
    fn load(&self, file: CredentialsFile, hasher: fn(&str) -> Result<String, std::io::Error>) {
        let password_hash = match (file.password_hash, file.password) {
            (Some(hash), plaintext) => {
                // A file holding both keys (hand-edited or half-migrated)
                // still carries the plaintext: rewrite it with the hash only.
                if plaintext.is_some() {
                    match self.persist_account(&file.username, &hash) {
                        Ok(()) => tracing::info!(
                            "Removed a plaintext password left beside the hash in credentials.json"
                        ),
                        Err(error) => tracing::warn!(
                            "Could not remove the plaintext password from credentials.json: {error}"
                        ),
                    }
                }
                hash
            }
            (None, Some(plaintext)) => {
                let hash = match hasher(&plaintext) {
                    Ok(hash) => hash,
                    Err(error) => {
                        // The plaintext file stays as it is, so the next
                        // start can retry the migration.
                        self.lock(
                            file.username,
                            &format!("the stored password could not be migrated ({error})"),
                        );
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
            (None, None) => {
                self.lock(file.username, "credentials.json holds no password");
                return;
            }
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
    /// slow: async callers use [`CredentialStore::check_login`].
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

    /// Run Argon2 work (`f`) on the blocking thread pool, at most
    /// [`HASHING_PERMITS`] at a time. The permit moves into the blocking task,
    /// so a client that disconnects does not free its slot early. Returns
    /// `None` if the task panicked. This applies no failure throttling: a
    /// password check goes through [`CredentialStore::guarded_check`].
    pub async fn run_hashing<T, F>(self: &Arc<Self>, f: F) -> Option<T>
    where
        T: Send + 'static,
        F: FnOnce(&CredentialStore) -> T + Send + 'static,
    {
        let permit = self.hashing.clone().acquire_owned().await.ok()?;
        let store = self.clone();
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            f(&store)
        })
        .await
        .ok()
    }

    /// The one path every password guess takes (the login endpoint, HTTP
    /// Basic auth and the current password of a credential change), so they
    /// share the failed-attempt throttle and cannot diverge.
    ///
    /// `check` verifies the password on the blocking pool, under the
    /// [`HASHING_PERMITS`] cap, and says how the throttle should count it.
    /// A client that is locked out (per address or globally) is still
    /// verified, so the correct password gets in, but its attempts go one at
    /// a time through the [`LockedLane`], and each wrong one holds that lane
    /// for [`LOCKED_FAILURE_HOLD`]. While locked out, guessing is limited to
    /// about one attempt per hold across all locked clients. The lane serves
    /// the newest attempt first and keeps at most [`LOCKED_LANE_WAITERS`]
    /// waiting, so a fresh correct attempt waits for about one hold, not for
    /// a queue of guesses. An attempt turned away from a full lane, and a
    /// wrong guess made in the lane, are answered as [`Guarded::Throttled`].
    /// Wrong guesses made in the lane extend the client's own lockout but do
    /// not count toward the global cap, so one locked client cannot keep
    /// every other client in the lane.
    ///
    /// The lockout is checked again once a hashing slot is held, so a burst
    /// that queued before the lockout began is moved to the locked lane
    /// instead of each attempt getting a free verification. Returns `None` if
    /// the check panicked.
    pub async fn guarded_check<T, F>(
        self: &Arc<Self>,
        client: Option<IpAddr>,
        check: F,
    ) -> Option<Guarded<T>>
    where
        T: Send + 'static,
        F: FnOnce(&CredentialStore) -> (T, Verdict) + Send + 'static,
    {
        let mut lane = None;
        let hashing = loop {
            if lane.is_none()
                && let Err(retry_after) = self.throttle.check(client, Instant::now())
            {
                // The lane is taken before a hashing slot, never while one is
                // held, so the two cannot deadlock.
                match self.locked_lane.enter().await {
                    Some(pass) => lane = Some(pass),
                    None => return Some(Guarded::Throttled { retry_after }),
                }
            }
            let permit = self.hashing.clone().acquire_owned().await.ok()?;
            if lane.is_some() || self.throttle.check(client, Instant::now()).is_ok() {
                break permit;
            }
            drop(permit);
        };

        // The work runs in its own task holding both permits, so a client
        // that disconnects frees neither the hashing slot nor the lane early.
        let (reply, response) = tokio::sync::oneshot::channel();
        let store = self.clone();
        let hold = self.locked_failure_hold;
        tokio::spawn(async move {
            let locked = lane.is_some();
            let checked = tokio::task::spawn_blocking(move || {
                // The result is recorded before the hashing slot is freed, so
                // the next attempt's re-check already sees this failure.
                let _hashing = hashing;
                let (value, verdict) = check(&store);
                let now = Instant::now();
                let outcome = match verdict {
                    Verdict::Accepted => {
                        store.throttle.record_success(client);
                        Guarded::Done(value)
                    }
                    Verdict::Neutral => Guarded::Done(value),
                    Verdict::Rejected => {
                        store.throttle.record_failure(client, now, !locked);
                        match store.throttle.check(client, now) {
                            Err(retry_after) if locked => Guarded::Throttled { retry_after },
                            _ => Guarded::Done(value),
                        }
                    }
                };
                (outcome, verdict)
            })
            .await;
            let Ok((outcome, verdict)) = checked else {
                return;
            };
            let _ = reply.send(outcome);
            if lane.is_some() && verdict == Verdict::Rejected {
                tokio::time::sleep(hold).await;
            }
            drop(lane);
        });
        response.await.ok()
    }

    /// Check a password login (the login endpoint or HTTP Basic auth) from
    /// `client` through [`CredentialStore::guarded_check`]. A success clears
    /// the client's failure count and is never counted.
    pub async fn check_login(
        self: Arc<Self>,
        client: Option<IpAddr>,
        username: String,
        password: String,
    ) -> LoginOutcome {
        let checked = self
            .guarded_check(client, move |store| {
                if store.validate(&username, &password) {
                    (LoginOutcome::Ok, Verdict::Accepted)
                } else {
                    (LoginOutcome::Invalid, Verdict::Rejected)
                }
            })
            .await;
        match checked {
            Some(Guarded::Done(outcome)) => outcome,
            Some(Guarded::Throttled { retry_after }) => LoginOutcome::Throttled { retry_after },
            None => LoginOutcome::Invalid,
        }
    }

    /// [`CredentialStore::change_credentials`] through
    /// [`CredentialStore::guarded_check`]: the current password is a guess
    /// like a login and is throttled the same way.
    pub async fn check_change_credentials(
        self: &Arc<Self>,
        client: Option<IpAddr>,
        current_password: String,
        new_username: Option<String>,
        new_password: Option<String>,
    ) -> Result<(), ChangeCredentialsError> {
        let checked = self
            .guarded_check(client, move |store| {
                let result =
                    store.change_credentials(&current_password, new_username, new_password);
                let verdict = match &result {
                    Err(ChangeCredentialsError::WrongPassword) => Verdict::Rejected,
                    Err(ChangeCredentialsError::NotConfigured) => Verdict::Neutral,
                    // The current password was right; the change failed later.
                    _ => Verdict::Accepted,
                };
                (result, verdict)
            })
            .await;
        match checked {
            Some(Guarded::Done(result)) => result,
            Some(Guarded::Throttled { retry_after }) => {
                Err(ChangeCredentialsError::Throttled { retry_after })
            }
            None => Err(ChangeCredentialsError::Io(std::io::Error::other(
                "credential change task failed",
            ))),
        }
    }
}

/// How [`CredentialStore::guarded_check`] counts one password check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// The password was correct: the client's failures are cleared.
    Accepted,
    /// The password was wrong: a failure is recorded.
    Rejected,
    /// No password was checked (for example, nothing is configured).
    Neutral,
}

/// Result of [`CredentialStore::guarded_check`].
#[derive(Debug)]
pub enum Guarded<T> {
    Done(T),
    /// A wrong guess while locked out; retry after the given delay.
    Throttled {
        retry_after: Duration,
    },
}

/// Result of [`CredentialStore::check_login`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoginOutcome {
    Ok,
    Invalid,
    /// A wrong password while locked out; retry after the given delay.
    Throttled {
        retry_after: Duration,
    },
}

impl LoginOutcome {
    /// A 429 response with a `Retry-After` header (whole seconds, rounded up).
    pub fn too_many_requests(retry_after: Duration) -> axum::response::Response {
        let secs = retry_after.as_secs() + u64::from(retry_after.subsec_nanos() > 0);
        (
            StatusCode::TOO_MANY_REQUESTS,
            [(http::header::RETRY_AFTER, secs.max(1).to_string())],
            "too many failed login attempts; try again later",
        )
            .into_response()
    }
}

/// The client address failed logins are counted against: the peer address
/// axum records when the server is run with
/// `into_make_service_with_connect_info::<SocketAddr>()`, or, when that peer
/// is listed in `general.trusted_proxies`, the client it forwarded for (see
/// [`forwarded_client`]). Without a trusted proxy, every client behind a
/// reverse proxy shares the proxy's bucket.
pub fn client_ip(
    state: &AppState,
    extensions: &http::Extensions,
    headers: &http::HeaderMap,
) -> Option<IpAddr> {
    let peer = extensions
        .get::<axum::extract::ConnectInfo<SocketAddr>>()?
        .0
        .ip();
    Some(forwarded_client(
        peer,
        headers,
        &state.config().general.trusted_proxies,
    ))
}

/// The client behind `peer`. `X-Forwarded-For` is believed only when `peer`
/// is a trusted proxy. It is read right to left, since each proxy appends
/// the address it received the request from: the first address that is not
/// itself a trusted proxy is the client. An entry that does not parse stops
/// the walk at the last trusted hop, so a forged or garbled header can only
/// ever name an address no more specific than a trusted proxy.
pub fn forwarded_client(
    peer: IpAddr,
    headers: &http::HeaderMap,
    trusted_proxies: &[String],
) -> IpAddr {
    let networks: Vec<(IpAddr, u8)> = trusted_proxies
        .iter()
        .filter_map(|entry| crate::nzb_core::config::parse_ip_network(entry.trim()))
        .collect();
    let trusted = |ip: IpAddr| {
        networks
            .iter()
            .any(|&(net, prefix)| crate::fetch_guard::network_contains(net, prefix, ip))
    };
    let peer = peer.to_canonical();
    if !trusted(peer) {
        return peer;
    }
    let mut client = peer;
    for value in headers.get_all("x-forwarded-for").iter().rev() {
        let Ok(value) = value.to_str() else {
            return client;
        };
        for hop in value.rsplit(',') {
            let hop = hop.trim();
            let Some(ip) = hop
                .parse::<IpAddr>()
                .ok()
                .or_else(|| hop.parse::<SocketAddr>().ok().map(|addr| addr.ip()))
            else {
                return client;
            };
            client = ip.to_canonical();
            if !trusted(client) {
                return client;
            }
        }
    }
    client
}

// --- Failed-login throttle ---

/// Failures a client may make within [`CLIENT_FAILURE_WINDOW`] before it is
/// locked out.
const CLIENT_FREE_FAILURES: u32 = 5;
/// A client's failure count resets once it has made no failed attempt for
/// this long.
const CLIENT_FAILURE_WINDOW: Duration = Duration::from_secs(15 * 60);
/// First per-client lockout; each further failure doubles it.
const CLIENT_BASE_LOCKOUT: Duration = Duration::from_secs(30);
/// Upper bound of the exponential per-client lockout.
const CLIENT_MAX_LOCKOUT: Duration = Duration::from_secs(15 * 60);
/// Failures from all clients together allowed per [`GLOBAL_WINDOW`]. This
/// bounds guessing from many addresses (or from behind one proxy).
const GLOBAL_MAX_FAILURES: u32 = 30;
const GLOBAL_WINDOW: Duration = Duration::from_secs(60);
/// Clients tracked at once; beyond this, stale entries are pruned and new
/// clients are counted by the global budget only.
const MAX_TRACKED_CLIENTS: usize = 4096;

struct ClientFailures {
    count: u32,
    last: Instant,
}

impl ClientFailures {
    /// How long the client stays locked out after `count` failures: none for
    /// the first [`CLIENT_FREE_FAILURES`] - 1, then [`CLIENT_BASE_LOCKOUT`]
    /// doubling up to [`CLIENT_MAX_LOCKOUT`].
    fn lockout(&self) -> Duration {
        if self.count < CLIENT_FREE_FAILURES {
            return Duration::ZERO;
        }
        let exponent = (self.count - CLIENT_FREE_FAILURES).min(16);
        (CLIENT_BASE_LOCKOUT * (1u32 << exponent)).min(CLIENT_MAX_LOCKOUT)
    }
}

#[derive(Default)]
struct ThrottleState {
    clients: HashMap<Option<IpAddr>, ClientFailures>,
    global_window_start: Option<Instant>,
    global_failures: u32,
}

/// Failed-login backoff: an exponential lockout per client address plus a
/// global cap on failures per minute. Only failures are counted. A lockout
/// moves the client's attempts to the paced locked lane; it never refuses a
/// correct password (see [`CredentialStore::guarded_check`]).
#[derive(Default)]
pub struct LoginThrottle {
    state: parking_lot::Mutex<ThrottleState>,
}

impl LoginThrottle {
    /// `Err(retry_after)` when `client` (or everyone) must wait.
    fn check(&self, client: Option<IpAddr>, now: Instant) -> Result<(), Duration> {
        let state = self.state.lock();
        if let Some(start) = state.global_window_start {
            let end = start + GLOBAL_WINDOW;
            if now < end && state.global_failures >= GLOBAL_MAX_FAILURES {
                return Err(end - now);
            }
        }
        if let Some(entry) = state.clients.get(&client) {
            let until = entry.last + entry.lockout();
            if now < until {
                return Err(until - now);
            }
        }
        Ok(())
    }

    /// Count a failure against `client`, and against the global cap when
    /// `global` (false for a guess made while already locked out).
    fn record_failure(&self, client: Option<IpAddr>, now: Instant, global: bool) {
        let mut state = self.state.lock();
        if global {
            match state.global_window_start {
                Some(start) if now < start + GLOBAL_WINDOW => state.global_failures += 1,
                _ => {
                    state.global_window_start = Some(now);
                    state.global_failures = 1;
                }
            }
        }
        if !state.clients.contains_key(&client) && state.clients.len() >= MAX_TRACKED_CLIENTS {
            state
                .clients
                .retain(|_, entry| now.duration_since(entry.last) < CLIENT_FAILURE_WINDOW);
            if state.clients.len() >= MAX_TRACKED_CLIENTS {
                return;
            }
        }
        let entry = state.clients.entry(client).or_insert(ClientFailures {
            count: 0,
            last: now,
        });
        if now.duration_since(entry.last) >= CLIENT_FAILURE_WINDOW {
            entry.count = 0;
        }
        entry.count = entry.count.saturating_add(1);
        entry.last = now;
    }

    fn record_success(&self, client: Option<IpAddr>) {
        self.state.lock().clients.remove(&client);
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

    let result = state
        .credential_store
        .run_hashing(move |store| {
            store.initialize_credentials(StoredCredentials {
                username: req.username,
                password: req.password,
            })
        })
        .await
        .unwrap_or_else(|| Err(std::io::Error::other("credential setup task failed")));
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
        (status = 429, description = "Wrong password while this client (or every client) is locked out after repeated failures; see Retry-After. The correct password is still accepted", body = String, content_type = "text/plain"),
        (status = 500, description = "Credentials could not be saved", body = String, content_type = "text/plain"),
    ),
)]
pub async fn h_auth_change_credentials(
    State(state): State<ApiState>,
    extensions: http::Extensions,
    headers: http::HeaderMap,
    Json(req): Json<ChangeCredentialsRequest>,
) -> impl IntoResponse {
    let result = state
        .credential_store
        .check_change_credentials(
            client_ip(&state, &extensions, &headers),
            req.current_password,
            req.new_username,
            req.new_password,
        )
        .await;
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
        Err(ChangeCredentialsError::Throttled { retry_after }) => {
            LoginOutcome::too_many_requests(retry_after)
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
        (status = 429, description = "Wrong password while this client (or every client) is locked out after repeated failures; see Retry-After. The correct password is still accepted", body = String, content_type = "text/plain"),
    ),
)]
pub async fn h_auth_login(
    State(state): State<ApiState>,
    extensions: http::Extensions,
    headers: http::HeaderMap,
    Json(req): Json<LoginRequest>,
) -> impl IntoResponse {
    if !state.credential_store.has_credentials() {
        return (StatusCode::NOT_FOUND, "authentication not configured").into_response();
    }

    match state
        .credential_store
        .clone()
        .check_login(
            client_ip(&state, &extensions, &headers),
            req.username,
            req.password,
        )
        .await
    {
        LoginOutcome::Ok => {}
        LoginOutcome::Invalid => {
            return (StatusCode::UNAUTHORIZED, "invalid credentials").into_response();
        }
        LoginOutcome::Throttled { retry_after } => {
            return LoginOutcome::too_many_requests(retry_after);
        }
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

    fn failing_hasher(_: &str) -> Result<String, std::io::Error> {
        Err(std::io::Error::other("no entropy"))
    }

    #[test]
    fn failed_migration_keeps_the_file_and_refuses_login_without_reopening_setup() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("credentials.json");
        let plaintext = r#"{"username":"alice","password":"correct horse"}"#;
        std::fs::write(&path, plaintext).unwrap();

        let store = CredentialStore::open(temp.path().to_path_buf(), failing_hasher);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), plaintext);
        assert!(store.has_credentials(), "setup must stay closed");
        assert!(!store.validate("alice", "correct horse"));
        assert!(matches!(
            store.change_credentials("correct horse", None, Some("new".into())),
            Err(ChangeCredentialsError::WrongPassword)
        ));
        let error = store.initialize_credentials(alice()).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), plaintext);

        // The next start with a working hasher migrates the kept file.
        let retried = CredentialStore::new(temp.path().to_path_buf());
        assert!(retried.validate("alice", "correct horse"));
        assert!(
            !std::fs::read_to_string(&path)
                .unwrap()
                .contains("correct horse")
        );
    }

    #[test]
    fn unusable_credentials_files_fail_closed() {
        for contents in ["{not json", r#"{"username":"alice"}"#] {
            let temp = tempfile::tempdir().unwrap();
            let path = temp.path().join("credentials.json");
            std::fs::write(&path, contents).unwrap();
            let store = CredentialStore::new(temp.path().to_path_buf());
            assert!(
                store.has_credentials(),
                "{contents}: setup must stay closed"
            );
            assert!(!store.validate("alice", ""));
            assert!(store.initialize_credentials(alice()).is_err());
            assert_eq!(std::fs::read_to_string(&path).unwrap(), contents);
        }
    }

    #[test]
    fn a_plaintext_password_beside_the_hash_is_removed() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("credentials.json");
        let hash = hash_password("correct horse").unwrap();
        let both = serde_json::json!({
            "username": "alice",
            "password_hash": hash,
            "password": "stale plaintext",
        });
        std::fs::write(&path, both.to_string()).unwrap();

        let store = CredentialStore::new(temp.path().to_path_buf());
        assert!(store.validate("alice", "correct horse"));
        assert!(!store.validate("alice", "stale plaintext"));
        let file: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(file.get("password").is_none(), "{file}");
        assert_eq!(file["password_hash"], hash.as_str());
        #[cfg(unix)]
        assert_eq!(mode(&path), 0o600);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn hashing_work_is_capped_at_the_permit_count() {
        use std::sync::atomic::AtomicUsize;
        let temp = tempfile::tempdir().unwrap();
        let store = Arc::new(CredentialStore::new(temp.path().to_path_buf()));
        let running = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let tasks: Vec<_> = (0..12)
            .map(|_| {
                let store = store.clone();
                let running = running.clone();
                let peak = peak.clone();
                tokio::spawn(async move {
                    store
                        .run_hashing(move |_| {
                            let now = running.fetch_add(1, Ordering::SeqCst) + 1;
                            peak.fetch_max(now, Ordering::SeqCst);
                            std::thread::sleep(Duration::from_millis(30));
                            running.fetch_sub(1, Ordering::SeqCst);
                        })
                        .await
                        .unwrap();
                })
            })
            .collect();
        for task in tasks {
            task.await.unwrap();
        }
        assert_eq!(peak.load(Ordering::SeqCst), HASHING_PERMITS);
    }

    fn ip(last: u8) -> Option<IpAddr> {
        Some(IpAddr::from([192, 0, 2, last]))
    }

    #[test]
    fn throttle_locks_a_client_out_exponentially_after_repeated_failures() {
        let throttle = LoginThrottle::default();
        let start = Instant::now();
        for _ in 0..CLIENT_FREE_FAILURES - 1 {
            throttle.record_failure(ip(1), start, true);
            assert!(throttle.check(ip(1), start).is_ok());
        }
        throttle.record_failure(ip(1), start, true);
        assert_eq!(throttle.check(ip(1), start), Err(CLIENT_BASE_LOCKOUT));
        assert!(throttle.check(ip(1), start + CLIENT_BASE_LOCKOUT).is_ok());
        // Another client is unaffected.
        assert!(throttle.check(ip(2), start).is_ok());

        let later = start + CLIENT_BASE_LOCKOUT;
        throttle.record_failure(ip(1), later, true);
        assert_eq!(throttle.check(ip(1), later), Err(CLIENT_BASE_LOCKOUT * 2));
        throttle.record_failure(ip(1), later, true);
        assert_eq!(throttle.check(ip(1), later), Err(CLIENT_BASE_LOCKOUT * 4));
        for _ in 0..20 {
            throttle.record_failure(ip(1), later, true);
        }
        assert_eq!(throttle.check(ip(1), later), Err(CLIENT_MAX_LOCKOUT));

        // A success clears the client's record.
        throttle.record_success(ip(1));
        assert!(throttle.check(ip(1), later).is_ok());

        // Failures older than the window are forgotten.
        for _ in 0..CLIENT_FREE_FAILURES {
            throttle.record_failure(ip(3), start, true);
        }
        let quiet = start + CLIENT_FAILURE_WINDOW;
        throttle.record_failure(ip(3), quiet, true);
        assert!(throttle.check(ip(3), quiet).is_ok());
    }

    #[test]
    fn throttle_caps_failures_across_all_clients() {
        let throttle = LoginThrottle::default();
        let start = Instant::now();
        for n in 0..GLOBAL_MAX_FAILURES {
            throttle.record_failure(ip(n as u8), start, true);
        }
        let retry = throttle.check(ip(250), start).unwrap_err();
        assert_eq!(retry, GLOBAL_WINDOW);
        assert!(throttle.check(ip(250), start + GLOBAL_WINDOW).is_ok());
    }

    /// A store with a short locked-lane hold so tests run quickly.
    fn fast_store(temp: &tempfile::TempDir) -> Arc<CredentialStore> {
        let mut store = CredentialStore::new(temp.path().to_path_buf());
        store.locked_failure_hold = Duration::from_millis(50);
        store.set_credentials(alice()).unwrap();
        Arc::new(store)
    }

    #[tokio::test]
    async fn check_login_throttles_wrong_guesses_but_never_the_correct_password() {
        let temp = tempfile::tempdir().unwrap();
        let store = fast_store(&temp);
        let login = |client, password: &str| {
            store
                .clone()
                .check_login(client, "alice".into(), password.into())
        };

        // Successes are never counted: a correct login after a few failures
        // succeeds and resets the client's count.
        for _ in 0..CLIENT_FREE_FAILURES - 1 {
            assert_eq!(login(ip(1), "wrong").await, LoginOutcome::Invalid);
        }
        assert_eq!(login(ip(1), "correct horse").await, LoginOutcome::Ok);
        for _ in 0..CLIENT_FREE_FAILURES {
            assert_eq!(login(ip(1), "wrong").await, LoginOutcome::Invalid);
        }

        // Locked out: wrong guesses are refused with a retry delay, other
        // clients are unaffected, and the correct password still gets in.
        assert!(matches!(
            login(ip(1), "wrong").await,
            LoginOutcome::Throttled { .. }
        ));
        assert_eq!(login(ip(2), "wrong").await, LoginOutcome::Invalid);
        assert_eq!(login(ip(1), "correct horse").await, LoginOutcome::Ok);
        // The success cleared the lockout.
        assert!(store.throttle.check(ip(1), Instant::now()).is_ok());
    }

    #[tokio::test]
    async fn the_global_cap_does_not_block_the_correct_password() {
        let temp = tempfile::tempdir().unwrap();
        let store = fast_store(&temp);
        let now = Instant::now();
        for n in 0..GLOBAL_MAX_FAILURES {
            store.throttle.record_failure(ip(n as u8), now, true);
        }
        assert!(store.throttle.check(ip(200), now).is_err());

        assert!(matches!(
            store
                .clone()
                .check_login(ip(200), "alice".into(), "wrong".into())
                .await,
            LoginOutcome::Throttled { .. }
        ));
        assert_eq!(
            store
                .clone()
                .check_login(ip(201), "alice".into(), "correct horse".into())
                .await,
            LoginOutcome::Ok
        );
        assert!(
            store
                .check_change_credentials(ip(202), "correct horse".into(), Some("bob".into()), None)
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn change_credentials_shares_the_throttle_and_accepts_the_correct_password() {
        let temp = tempfile::tempdir().unwrap();
        let store = fast_store(&temp);
        let change = |password: &str| {
            store.check_change_credentials(ip(1), password.into(), None, Some("new".into()))
        };
        for _ in 0..CLIENT_FREE_FAILURES {
            assert!(matches!(
                change("wrong").await,
                Err(ChangeCredentialsError::WrongPassword)
            ));
        }
        assert!(matches!(
            change("wrong").await,
            Err(ChangeCredentialsError::Throttled { .. })
        ));
        // Login from the same client is locked out by the same count.
        assert!(matches!(
            store
                .clone()
                .check_login(ip(1), "alice".into(), "wrong".into())
                .await,
            LoginOutcome::Throttled { .. }
        ));
        change("correct horse").await.unwrap();
        assert!(store.validate("alice", "new"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_concurrent_burst_of_wrong_guesses_is_moved_to_the_locked_lane() {
        let temp = tempfile::tempdir().unwrap();
        let store = fast_store(&temp);
        const BURST: usize = 12;
        let started = Instant::now();
        let tasks: Vec<_> = (0..BURST)
            .map(|_| {
                let store = store.clone();
                tokio::spawn(async move {
                    store
                        .check_change_credentials(ip(1), "wrong".into(), None, Some("x".into()))
                        .await
                })
            })
            .collect();
        let (mut wrong, mut throttled) = (0, 0);
        for task in tasks {
            match task.await.unwrap() {
                Err(ChangeCredentialsError::WrongPassword) => wrong += 1,
                Err(ChangeCredentialsError::Throttled { .. }) => throttled += 1,
                other => panic!("unexpected {other:?}"),
            }
        }
        // Only the free failures, plus attempts already holding a hashing
        // slot when the lockout began, are verified outside the locked lane.
        let free = CLIENT_FREE_FAILURES as usize + HASHING_PERMITS - 1;
        assert!(wrong <= free, "{wrong} unpaced guesses");
        assert_eq!(wrong + throttled, BURST);
        // Locked-lane guesses run one at a time, each holding the lane. The
        // last guess's hold runs after its reply, so it is not counted.
        let paced = throttled.saturating_sub(1) as u32 * store.locked_failure_hold;
        assert!(
            started.elapsed() >= paced,
            "{:?} < {paced:?}",
            started.elapsed()
        );
    }

    /// Wait until `lane` has `count` waiters, letting spawned tasks run.
    async fn until_waiting(lane: &LockedLane, count: usize) {
        while lane.waiting() != count {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    }

    #[tokio::test]
    async fn the_locked_lane_serves_the_newest_waiter_and_turns_the_oldest_away() {
        let lane = Arc::new(LockedLane::new(2));
        let first = lane.enter().await.unwrap();
        let (order_tx, mut order) = tokio::sync::mpsc::unbounded_channel();
        for (name, waiting) in [("a", 1), ("b", 2), ("c", 2)] {
            let joining = lane.clone();
            let order_tx = order_tx.clone();
            tokio::spawn(async move {
                let pass = joining.enter().await;
                order_tx.send((name, pass.is_some())).unwrap();
            });
            if name != "c" {
                until_waiting(&lane, waiting).await;
            }
        }
        // The third waiter pushed the oldest out of the full queue.
        assert_eq!(order.recv().await, Some(("a", false)));
        // Freed, the lane goes to the newest waiter first.
        drop(first);
        assert_eq!(order.recv().await, Some(("c", true)));
        assert_eq!(order.recv().await, Some(("b", true)));
        assert_eq!(lane.waiting(), 0);
        assert!(lane.enter().await.is_some());
    }

    #[tokio::test]
    async fn a_waiter_dropped_after_being_handed_the_lane_passes_it_on() {
        let lane = Arc::new(LockedLane::new(2));
        let first = lane.enter().await.unwrap();
        let mut entering = Box::pin(lane.enter());
        assert!(
            tokio::time::timeout(Duration::from_millis(10), &mut entering)
                .await
                .is_err()
        );
        // The lane is handed to the waiter, which is dropped unpolled.
        drop(first);
        drop(entering);
        tokio::time::timeout(Duration::from_secs(1), lane.enter())
            .await
            .expect("the lane was lost")
            .unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_correct_login_gets_through_a_queued_flood_quickly() {
        let temp = tempfile::tempdir().unwrap();
        // The real one-second hold, so the bound below is the shipped one.
        let store = CredentialStore::new(temp.path().to_path_buf());
        store.set_credentials(alice()).unwrap();
        let store = Arc::new(store);
        let now = Instant::now();
        for _ in 0..CLIENT_FREE_FAILURES {
            store.throttle.record_failure(ip(1), now, true);
        }
        let global_before = store.throttle.state.lock().global_failures;

        const FLOOD: usize = 40;
        let flood: Vec<_> = (0..FLOOD)
            .map(|_| {
                let store = store.clone();
                tokio::spawn(async move {
                    store
                        .check_login(ip(1), "alice".into(), "wrong".into())
                        .await
                })
            })
            .collect();
        until_waiting(&store.locked_lane, LOCKED_LANE_WAITERS).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        // Beyond the queue, the flood was answered at once, unverified.
        let answered = flood.iter().filter(|task| task.is_finished()).count();
        assert!(answered >= FLOOD - 1 - LOCKED_LANE_WAITERS, "{answered}");

        // Another client is not locked: the flood did not trip the global
        // cap, so it logs in without waiting for the lane.
        let started = Instant::now();
        assert_eq!(
            store
                .clone()
                .check_login(ip(2), "alice".into(), "correct horse".into())
                .await,
            LoginOutcome::Ok
        );
        assert!(started.elapsed() < Duration::from_secs(3));

        // The flooded client's own correct attempt waits for about one
        // hold, not for the queued guesses.
        let started = Instant::now();
        assert_eq!(
            store
                .clone()
                .check_login(ip(1), "alice".into(), "correct horse".into())
                .await,
            LoginOutcome::Ok
        );
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "{:?}",
            started.elapsed()
        );
        // Guesses made in the lane never counted globally.
        assert_eq!(store.throttle.state.lock().global_failures, global_before);
        for task in flood {
            task.abort();
        }
    }

    #[test]
    fn x_forwarded_for_is_believed_only_from_a_trusted_proxy() {
        let headers = |values: &[&str]| {
            let mut headers = http::HeaderMap::new();
            for value in values {
                headers.append("x-forwarded-for", value.parse().unwrap());
            }
            headers
        };
        let addr = |s: &str| s.parse::<IpAddr>().unwrap();
        let proxy = addr("172.18.0.5");
        let trusted = vec!["172.18.0.0/16".to_owned(), "10.0.0.1".to_owned()];
        let client = |peer, values: &[&str], trusted: &[String]| {
            forwarded_client(peer, &headers(values), trusted)
        };

        // No trusted proxies: the header is ignored.
        assert_eq!(client(proxy, &["203.0.113.7"], &[]), proxy);
        // An untrusted peer cannot claim another address.
        assert_eq!(
            client(addr("198.51.100.9"), &["203.0.113.7"], &trusted),
            addr("198.51.100.9")
        );
        // A trusted peer: the rightmost untrusted address is the client, so
        // an address the client prepended itself is not believed.
        assert_eq!(
            client(proxy, &["203.0.113.7"], &trusted),
            addr("203.0.113.7")
        );
        assert_eq!(
            client(proxy, &["198.51.100.1, 203.0.113.7, 10.0.0.1"], &trusted),
            addr("203.0.113.7")
        );
        assert_eq!(
            client(proxy, &["198.51.100.1", "203.0.113.7"], &trusted),
            addr("203.0.113.7")
        );
        // An IPv4-mapped peer matches an IPv4 network; ports are dropped.
        assert_eq!(
            client(addr("::ffff:172.18.0.5"), &["203.0.113.7:4711"], &trusted),
            addr("203.0.113.7")
        );
        // A garbled entry stops at the last trusted hop.
        assert_eq!(client(proxy, &["203.0.113.7, nonsense"], &trusted), proxy);
        assert_eq!(
            client(proxy, &["203.0.113.7, nonsense, 10.0.0.1"], &trusted),
            addr("10.0.0.1")
        );
        // No header, or only trusted hops: the last trusted address.
        assert_eq!(client(proxy, &[], &trusted), proxy);
        assert_eq!(client(proxy, &["10.0.0.1"], &trusted), addr("10.0.0.1"));
    }
}
