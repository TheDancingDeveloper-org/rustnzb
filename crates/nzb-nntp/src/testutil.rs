//! Test utilities: in-process mock NNTP server.
//!
//! Provides a configurable NNTP server that runs as a tokio task for unit testing.
//! Supports: AUTH, GROUP, XOVER, BODY, ARTICLE, STAT, QUIT with configurable
//! responses and error injection.
//!
//! ## Fault injection
//!
//! `MockConfig` exposes several primitives that simulate real-world provider
//! pathologies which the worker pool, stall detector, and connection tracker
//! must survive without producing zombie connections or stuck jobs:
//!
//! - `silent_close_after_bytes` — close the socket after writing N bytes (no
//!   QUIT, no error response). Simulates a provider that drops the session.
//! - `hang_after_command` — stop responding after seeing a specific verb.
//!   Reads incoming bytes and discards them. Simulates a provider that has
//!   stalled mid-session.
//! - `close_after_n_commands` — force a disconnect after N commands processed.
//!   Simulates a provider that recycles connections aggressively.
//! - `response_delay` — sleep before each write. Simulates a slow provider.
//! - `article_response_overrides` — return a custom NNTP response code (430,
//!   502, 403, etc.) for a specific message-id, regardless of whether the
//!   article exists. Simulates retention drift, throttling, and access denial.
//! - `auth_rate_limit` — reject auth attempts after N within a sliding window.
//!   State is shared across all connections to the same `MockNntpServer`.
//!   Simulates account-level rate limiting.

use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;

use crate::config::ServerConfig;

pub type ArticleResponseSequences = Arc<Mutex<HashMap<String, VecDeque<(u16, String)>>>>;

// ---------------------------------------------------------------------------
// Auth rate limiter (cross-connection state)
// ---------------------------------------------------------------------------

/// Sliding-window auth rate limiter. State is shared across all connections
/// to the same `MockNntpServer` so that a burst of reconnects can trip the
/// limiter the same way a real provider would.
pub struct AuthRateLimit {
    pub max_attempts: u32,
    pub window: Duration,
    state: Arc<Mutex<VecDeque<Instant>>>,
}

impl AuthRateLimit {
    pub fn new(max_attempts: u32, window: Duration) -> Self {
        Self {
            max_attempts,
            window,
            state: Arc::new(Mutex::new(VecDeque::new())),
        }
    }

    /// Record an auth attempt. Returns `true` if the attempt should be
    /// rejected (limit exceeded within the current window).
    fn check_and_record(&self) -> bool {
        let now = Instant::now();
        let mut s = self.state.lock();
        while let Some(&front) = s.front() {
            if now.duration_since(front) > self.window {
                s.pop_front();
            } else {
                break;
            }
        }
        s.push_back(now);
        s.len() as u32 > self.max_attempts
    }
}

impl Clone for AuthRateLimit {
    fn clone(&self) -> Self {
        Self {
            max_attempts: self.max_attempts,
            window: self.window,
            state: Arc::clone(&self.state),
        }
    }
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Configuration for the mock NNTP server.
#[derive(Clone)]
pub struct MockConfig {
    /// Welcome banner code (200 = posting allowed, 201 = read-only).
    pub welcome_code: u16,
    /// Welcome banner message.
    pub welcome_message: String,
    /// Whether auth is required before commands.
    pub auth_required: bool,
    /// Valid credentials. None = accept any credentials.
    pub valid_credentials: Option<(String, String)>,
    /// If true, authentication always fails (482 on USER).
    pub fail_auth: bool,
    /// If true, server sends 502 on connect.
    pub service_unavailable: bool,
    /// Groups: name -> (count, first, last).
    pub groups: HashMap<String, (u64, u64, u64)>,
    /// Articles: message-id (without angle brackets) -> body bytes.
    pub articles: HashMap<String, Vec<u8>>,
    /// XOVER entries as pre-formatted tab-delimited lines.
    pub xover_entries: Vec<String>,
    /// XHDR entries as pre-formatted `artnum value` lines.
    pub xhdr_entries: Vec<String>,
    /// XPAT entries as pre-formatted `artnum value` lines.
    pub xpat_entries: Vec<String>,
    /// LIST ACTIVE entries as pre-formatted `groupname last first status` lines.
    pub list_active_entries: Vec<String>,
    /// If true, `POST` returns 440 instead of accepting an article body.
    pub post_not_permitted: bool,
    /// Captured raw articles received via `POST`, after un-dot-stuffing and
    /// normalizing line endings to CRLF.
    pub posted_articles: Option<Arc<Mutex<Vec<Vec<u8>>>>>,

    // ---- Fault injection (test-support feature) ----
    /// Close the socket silently after writing this many total bytes. The
    /// last write is truncated to the limit; no QUIT or error response is
    /// emitted. Simulates a provider that drops the session.
    pub silent_close_after_bytes: Option<usize>,
    /// Stop responding after seeing this command verb. The connection
    /// continues to read incoming bytes (and discards them) but emits no
    /// further responses. Simulates a stalled provider.
    pub hang_after_command: Option<String>,
    /// Force the connection closed after processing this many commands.
    /// Simulates a provider that recycles sessions.
    pub close_after_n_commands: Option<u32>,
    /// Sleep this long before each response write. Applied uniformly to
    /// every write, not just per-command. Simulates a slow provider.
    pub response_delay: Option<Duration>,
    /// Override response codes for specific message-ids on ARTICLE/BODY/STAT.
    /// The override always wins over the `articles` map and never returns a
    /// body. Use to inject 430 (not found), 502 (server down), 403
    /// (forbidden) etc. for individual articles.
    pub article_response_overrides: HashMap<String, u16>,
    /// Shared one-shot responses for ARTICLE requests. Each request pops the
    /// next `(code, message)` for its message-id; once exhausted, normal
    /// article lookup resumes. This models a stale session returning `400`
    /// followed by a successful fetch on a replacement connection.
    pub article_response_sequences: Option<ArticleResponseSequences>,
    /// Cross-connection auth rate limiter. After `max_attempts` AUTHINFO PASS
    /// attempts within the `window`, all subsequent auths return 481.
    pub auth_rate_limit: Option<AuthRateLimit>,
    /// If true, respond to `CAPABILITIES` with `500 Unknown command` to
    /// simulate a pre-RFC-3977 server. Forces the client onto its
    /// conservative-default code path.
    pub capabilities_unsupported: bool,
    /// If true, the `CAPABILITIES` response advertises `MODE-READER`
    /// instead of `READER`, requiring the client to issue `MODE READER`
    /// before reader commands work.
    pub capabilities_mode_reader: bool,
}

impl Default for MockConfig {
    fn default() -> Self {
        Self {
            welcome_code: 200,
            welcome_message: "Mock NNTP Ready".into(),
            auth_required: false,
            valid_credentials: None,
            fail_auth: false,
            service_unavailable: false,
            groups: HashMap::new(),
            articles: HashMap::new(),
            xover_entries: Vec::new(),
            xhdr_entries: Vec::new(),
            xpat_entries: Vec::new(),
            list_active_entries: Vec::new(),
            post_not_permitted: false,
            posted_articles: None,
            silent_close_after_bytes: None,
            hang_after_command: None,
            close_after_n_commands: None,
            response_delay: None,
            article_response_overrides: HashMap::new(),
            article_response_sequences: None,
            auth_rate_limit: None,
            capabilities_unsupported: false,
            capabilities_mode_reader: false,
        }
    }
}

// ---------------------------------------------------------------------------
// Mock server
// ---------------------------------------------------------------------------

/// An in-process mock NNTP server for testing.
pub struct MockNntpServer {
    pub addr: SocketAddr,
    _shutdown: tokio::sync::watch::Sender<bool>,
}

impl MockNntpServer {
    /// Start the mock server on a random port.
    pub async fn start(config: MockConfig) -> Self {
        Self::start_on("127.0.0.1:0", config).await
    }

    /// Start the mock server on the provided listen address.
    pub async fn start_on(bind_addr: &str, config: MockConfig) -> Self {
        let listener = TcpListener::bind(bind_addr).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let config = Arc::new(config);
        let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);

        tokio::spawn(async move {
            loop {
                tokio::select! {
                    result = listener.accept() => {
                        if let Ok((stream, _)) = result {
                            let cfg = config.clone();
                            tokio::spawn(handle_connection(stream, cfg));
                        }
                    }
                    _ = shutdown_rx.changed() => break,
                }
            }
        });

        Self {
            addr,
            _shutdown: shutdown_tx,
        }
    }

    /// The port the mock server is listening on.
    pub fn port(&self) -> u16 {
        self.addr.port()
    }
}

// ---------------------------------------------------------------------------
// Test config helpers
// ---------------------------------------------------------------------------

/// Create a plain-TCP ServerConfig pointing at localhost on the given port.
pub fn test_config(port: u16) -> ServerConfig {
    ServerConfig {
        id: "test-server".into(),
        name: "Test Server".into(),
        host: "127.0.0.1".into(),
        port,
        ssl: false,
        ssl_verify: false,
        username: None,
        password: None,
        connections: 4,
        priority: 0,
        enabled: true,
        retention: 0,
        pipelining: 1,
        optional: false,
        compress: false,
        ramp_up_delay_ms: 0, // no delay in tests
        recv_buffer_size: 0,
        proxy_url: None,
        trusted_fingerprint: None,
        connect_timeout_secs: 30,
    }
}

/// Create a plain-TCP ServerConfig with authentication credentials.
pub fn test_config_with_auth(port: u16, user: &str, pass: &str) -> ServerConfig {
    let mut config = test_config(port);
    config.username = Some(user.to_string());
    config.password = Some(pass.to_string());
    config
}

// ---------------------------------------------------------------------------
// Per-connection state + write helper
// ---------------------------------------------------------------------------

/// Per-connection state used to thread fault-injection through every write.
struct ConnState<'a> {
    stream: &'a mut BufReader<tokio::net::TcpStream>,
    config: &'a MockConfig,
    bytes_written: usize,
    hung: bool,
}

impl<'a> ConnState<'a> {
    fn new(stream: &'a mut BufReader<tokio::net::TcpStream>, config: &'a MockConfig) -> Self {
        Self {
            stream,
            config,
            bytes_written: 0,
            hung: false,
        }
    }

    /// Write `data` honouring fault injection. Returns `false` if the
    /// connection should close (silent_close_after_bytes hit, or we are in
    /// hang mode and the caller asked to write something).
    async fn write(&mut self, data: &[u8]) -> bool {
        if self.hung {
            return true;
        }

        if let Some(delay) = self.config.response_delay {
            tokio::time::sleep(delay).await;
        }

        if let Some(limit) = self.config.silent_close_after_bytes {
            if self.bytes_written >= limit {
                return false;
            }
            if self.bytes_written + data.len() > limit {
                let to_write = limit - self.bytes_written;
                let _ = self.stream.get_mut().write_all(&data[..to_write]).await;
                let _ = self.stream.get_mut().flush().await;
                self.bytes_written += to_write;
                return false;
            }
        }

        if self.stream.get_mut().write_all(data).await.is_err() {
            return false;
        }
        self.bytes_written += data.len();
        true
    }

    async fn flush(&mut self) {
        if !self.hung {
            let _ = self.stream.get_mut().flush().await;
        }
    }
}

/// Write `bytes`; if the helper signals close, return from the enclosing fn.
macro_rules! mwrite {
    ($conn:expr, $bytes:expr) => {
        if !$conn.write($bytes).await {
            return;
        }
    };
}

// ---------------------------------------------------------------------------
// Connection handler
// ---------------------------------------------------------------------------

async fn handle_connection(stream: tokio::net::TcpStream, config: Arc<MockConfig>) {
    let mut stream = BufReader::new(stream);

    // Send welcome banner (or 502 + bail).
    if config.service_unavailable {
        let _ = stream
            .get_mut()
            .write_all(b"502 Service unavailable\r\n")
            .await;
        let _ = stream.get_mut().flush().await;
        return;
    }

    let mut conn = ConnState::new(&mut stream, &config);

    let welcome = format!("{} {}\r\n", config.welcome_code, config.welcome_message);
    mwrite!(conn, welcome.as_bytes());
    conn.flush().await;

    let mut authenticated = !config.auth_required;
    let mut selected_group: Option<String> = None;
    let mut commands_processed: u32 = 0;
    let mut line = String::new();

    loop {
        line.clear();
        let read_result = conn.stream.read_line(&mut line).await;
        match read_result {
            Ok(0) => break,
            Ok(_) => {}
            Err(_) => break,
        }

        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let parts: Vec<&str> = trimmed.splitn(3, ' ').collect();
        let cmd = parts[0].to_uppercase();

        // hang_after_command: once we see this verb, mark the connection as
        // hung. We continue reading bytes (so the client doesn't see EOF) but
        // emit nothing. The client should hit its own stall detection.
        if let Some(ref hang_cmd) = config.hang_after_command
            && !conn.hung
            && cmd == hang_cmd.to_uppercase()
        {
            conn.hung = true;
        }

        match cmd.as_str() {
            "QUIT" => {
                mwrite!(conn, b"205 Goodbye\r\n");
                conn.flush().await;
                break;
            }

            "CAPABILITIES" => {
                if config.capabilities_unsupported {
                    mwrite!(conn, b"500 Unknown command\r\n");
                } else {
                    mwrite!(conn, b"101 Capability list:\r\n");
                    mwrite!(conn, b"VERSION 2\r\n");
                    if config.capabilities_mode_reader {
                        mwrite!(conn, b"MODE-READER\r\n");
                        mwrite!(conn, b"IHAVE\r\n");
                    } else {
                        mwrite!(conn, b"READER\r\n");
                        mwrite!(conn, b"POST\r\n");
                        mwrite!(conn, b"HDR\r\n");
                        mwrite!(conn, b"OVER MSGID\r\n");
                        mwrite!(conn, b"LIST ACTIVE NEWSGROUPS OVERVIEW.FMT\r\n");
                    }
                    mwrite!(conn, b"IMPLEMENTATION nzb-nntp-testutil 1.0\r\n");
                    mwrite!(conn, b".\r\n");
                }
            }

            "MODE" => {
                let sub = parts.get(1).map(|s| s.to_uppercase()).unwrap_or_default();
                if sub == "READER" {
                    mwrite!(conn, b"200 Reader mode, posting allowed\r\n");
                } else {
                    let resp = format!("501 Unknown MODE subcommand: {sub}\r\n");
                    mwrite!(conn, resp.as_bytes());
                }
            }

            "AUTHINFO" => {
                let sub = parts.get(1).map(|s| s.to_uppercase()).unwrap_or_default();
                match sub.as_str() {
                    "USER" => {
                        if config.fail_auth {
                            mwrite!(conn, b"482 Authentication rejected\r\n");
                        } else {
                            mwrite!(conn, b"381 Password required\r\n");
                        }
                    }
                    "PASS" => {
                        // Cross-connection rate limit takes precedence.
                        let rate_limited = config
                            .auth_rate_limit
                            .as_ref()
                            .map(|r| r.check_and_record())
                            .unwrap_or(false);

                        if rate_limited {
                            mwrite!(conn, b"481 Authentication rate-limited\r\n");
                        } else if config.fail_auth {
                            mwrite!(conn, b"481 Authentication failed\r\n");
                        } else if let Some((_, ref valid_pass)) = config.valid_credentials {
                            let given = parts.get(2).unwrap_or(&"");
                            if *given == valid_pass.as_str() {
                                authenticated = true;
                                mwrite!(conn, b"281 Authentication accepted\r\n");
                            } else {
                                mwrite!(conn, b"481 Authentication failed\r\n");
                            }
                        } else {
                            // No specific credentials — accept anything
                            authenticated = true;
                            mwrite!(conn, b"281 Authentication accepted\r\n");
                        }
                    }
                    _ => {
                        mwrite!(conn, b"500 Unknown AUTHINFO subcommand\r\n");
                    }
                }
            }

            "GROUP" => {
                if !authenticated {
                    mwrite!(conn, b"480 Authentication required\r\n");
                } else {
                    let name = parts.get(1).unwrap_or(&"");
                    if let Some(&(count, first, last)) = config.groups.get(*name) {
                        selected_group = Some(name.to_string());
                        let resp = format!("211 {count} {first} {last} {name}\r\n");
                        mwrite!(conn, resp.as_bytes());
                    } else {
                        mwrite!(conn, b"411 No such group\r\n");
                    }
                }
            }

            "XOVER" => {
                if !authenticated {
                    mwrite!(conn, b"480 Authentication required\r\n");
                } else if selected_group.is_none() {
                    mwrite!(conn, b"412 No newsgroup selected\r\n");
                } else if config.xover_entries.is_empty() {
                    mwrite!(conn, b"420 No articles in range\r\n");
                } else {
                    mwrite!(conn, b"224 Overview data follows\r\n");
                    for entry in &config.xover_entries {
                        mwrite!(conn, entry.as_bytes());
                        mwrite!(conn, b"\r\n");
                    }
                    mwrite!(conn, b".\r\n");
                }
            }

            "XHDR" => {
                if !authenticated {
                    mwrite!(conn, b"480 Authentication required\r\n");
                } else if config.xhdr_entries.is_empty() {
                    mwrite!(conn, b"420 No articles in range\r\n");
                } else {
                    mwrite!(conn, b"221 Header data follows\r\n");
                    for entry in &config.xhdr_entries {
                        mwrite!(conn, entry.as_bytes());
                        mwrite!(conn, b"\r\n");
                    }
                    mwrite!(conn, b".\r\n");
                }
            }

            "XPAT" => {
                if !authenticated {
                    mwrite!(conn, b"480 Authentication required\r\n");
                } else if config.xpat_entries.is_empty() {
                    mwrite!(conn, b"420 No articles matched\r\n");
                } else {
                    mwrite!(conn, b"221 Header data follows\r\n");
                    for entry in &config.xpat_entries {
                        mwrite!(conn, entry.as_bytes());
                        mwrite!(conn, b"\r\n");
                    }
                    mwrite!(conn, b".\r\n");
                }
            }

            "LIST" => {
                if !authenticated {
                    mwrite!(conn, b"480 Authentication required\r\n");
                } else if config.list_active_entries.is_empty() {
                    mwrite!(conn, b"215 List of newsgroups follows\r\n");
                    mwrite!(conn, b".\r\n");
                } else {
                    mwrite!(conn, b"215 List of newsgroups follows\r\n");
                    for entry in &config.list_active_entries {
                        mwrite!(conn, entry.as_bytes());
                        mwrite!(conn, b"\r\n");
                    }
                    mwrite!(conn, b".\r\n");
                }
            }

            "ARTICLE" => {
                if !authenticated {
                    mwrite!(conn, b"480 Authentication required\r\n");
                } else {
                    let mid = parts
                        .get(1)
                        .unwrap_or(&"")
                        .trim_matches(|c| c == '<' || c == '>');
                    let sequenced = config
                        .article_response_sequences
                        .as_ref()
                        .and_then(|state| state.lock().get_mut(mid).and_then(VecDeque::pop_front));
                    if let Some((code, message)) = sequenced {
                        let resp = format!("{code} {message}\r\n");
                        mwrite!(conn, resp.as_bytes());
                    } else if let Some(&code) = config.article_response_overrides.get(mid) {
                        let resp = format!("{code} <{mid}>\r\n");
                        mwrite!(conn, resp.as_bytes());
                    } else if let Some(data) = config.articles.get(mid) {
                        let header = format!("220 0 <{mid}>\r\n");
                        mwrite!(conn, header.as_bytes());
                        if !write_multiline_body(&mut conn, data).await {
                            return;
                        }
                    } else {
                        let resp = format!("430 No article: <{mid}>\r\n");
                        mwrite!(conn, resp.as_bytes());
                    }
                }
            }

            "BODY" => {
                if !authenticated {
                    mwrite!(conn, b"480 Authentication required\r\n");
                } else {
                    let mid = parts
                        .get(1)
                        .unwrap_or(&"")
                        .trim_matches(|c| c == '<' || c == '>');
                    if let Some(&code) = config.article_response_overrides.get(mid) {
                        let resp = format!("{code} <{mid}>\r\n");
                        mwrite!(conn, resp.as_bytes());
                    } else if let Some(data) = config.articles.get(mid) {
                        let header = format!("222 0 <{mid}>\r\n");
                        mwrite!(conn, header.as_bytes());
                        if !write_multiline_body(&mut conn, data).await {
                            return;
                        }
                    } else {
                        let resp = format!("430 No article: <{mid}>\r\n");
                        mwrite!(conn, resp.as_bytes());
                    }
                }
            }

            "STAT" => {
                if !authenticated {
                    mwrite!(conn, b"480 Authentication required\r\n");
                } else {
                    let mid = parts
                        .get(1)
                        .unwrap_or(&"")
                        .trim_matches(|c| c == '<' || c == '>');
                    if let Some(&code) = config.article_response_overrides.get(mid) {
                        let resp = format!("{code} <{mid}>\r\n");
                        mwrite!(conn, resp.as_bytes());
                    } else if config.articles.contains_key(mid) {
                        let resp = format!("223 0 <{mid}>\r\n");
                        mwrite!(conn, resp.as_bytes());
                    } else {
                        let resp = format!("430 No article: <{mid}>\r\n");
                        mwrite!(conn, resp.as_bytes());
                    }
                }
            }

            "POST" => {
                if !authenticated {
                    mwrite!(conn, b"480 Authentication required\r\n");
                } else if config.post_not_permitted {
                    mwrite!(conn, b"440 Posting not permitted\r\n");
                } else {
                    mwrite!(conn, b"340 Send article to be posted\r\n");
                    conn.flush().await;

                    let Some(article) = read_posted_article(conn.stream).await else {
                        return;
                    };
                    if let Some(captured) = &config.posted_articles {
                        captured.lock().push(article);
                    }
                    mwrite!(conn, b"240 Article received OK\r\n");
                }
            }

            _ => {
                let resp = format!("500 Unknown command: {cmd}\r\n");
                mwrite!(conn, resp.as_bytes());
            }
        }

        conn.flush().await;

        commands_processed += 1;
        if let Some(limit) = config.close_after_n_commands
            && commands_processed >= limit
        {
            return;
        }
    }
}

/// Write a multiline body with dot-stuffing and `.\r\n` terminator. Operates
/// on raw bytes — must NOT lossy-UTF-8-convert because article bodies can be
/// binary (yEnc-encoded payloads contain non-UTF-8 byte sequences). Accepts
/// either `\n`- or `\r\n`-delimited input lines and emits canonical `\r\n`.
/// Returns `false` if the connection should close (silent_close_after_bytes hit).
async fn write_multiline_body(conn: &mut ConnState<'_>, data: &[u8]) -> bool {
    let mut start = 0usize;
    let mut wrote_anything = false;
    while start < data.len() {
        // Find the next \n; if there isn't one, the rest is a final line
        // without a trailing newline (we add one ourselves).
        let nl_pos = data[start..]
            .iter()
            .position(|&b| b == b'\n')
            .map(|p| start + p);
        let line_end = nl_pos.unwrap_or(data.len());

        // Strip a trailing \r so callers can supply either \n- or \r\n-
        // delimited input.
        let mut line = &data[start..line_end];
        if line.last() == Some(&b'\r') {
            line = &line[..line.len() - 1];
        }

        wrote_anything = true;
        // Dot-stuff: if a line begins with '.', prepend another '.'.
        if line.first() == Some(&b'.') && !conn.write(b".").await {
            return false;
        }
        if !conn.write(line).await {
            return false;
        }
        if !conn.write(b"\r\n").await {
            return false;
        }

        start = match nl_pos {
            Some(p) => p + 1,
            None => data.len(),
        };
    }
    if !wrote_anything && !conn.write(b"\r\n").await {
        return false;
    }
    if !conn.write(b".\r\n").await {
        return false;
    }
    conn.flush().await;
    true
}

async fn read_posted_article(stream: &mut BufReader<tokio::net::TcpStream>) -> Option<Vec<u8>> {
    let mut article = Vec::new();

    loop {
        let mut line = Vec::new();
        match stream.read_until(b'\n', &mut line).await {
            Ok(0) => return None,
            Ok(_) => {}
            Err(_) => return None,
        }

        if line.ends_with(b"\n") {
            line.pop();
        }
        if line.ends_with(b"\r") {
            line.pop();
        }

        if line == b"." {
            break;
        }

        if line.starts_with(b"..") {
            article.extend_from_slice(&line[1..]);
        } else {
            article.extend_from_slice(&line);
        }
        article.extend_from_slice(b"\r\n");
    }

    Some(article)
}
