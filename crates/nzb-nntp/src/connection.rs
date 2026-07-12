//! NNTP connection state machine.
//!
//! Implements RFC 3977 (Network News Transfer Protocol) over async TCP/TLS.
//!
//! Connection lifecycle:
//! 1. TCP connect -> receive welcome (200/201)
//! 2. AUTH: USER/PASS if credentials provided
//! 3. ARTICLE <message-id> -> receive article data
//! 4. STAT <message-id> -> check article existence
//! 5. QUIT -> close

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_socks::tcp::Socks5Stream;
use tracing::{debug, info, trace, warn};

/// Timeout for reading a single response line from the server.
/// If the server doesn't respond within this window, the connection is
/// considered dead and an I/O error is returned so workers can reconnect.
///
/// Set to 60s: XOVER on large (25K+ article) ranges can legitimately take
/// 30-45s for the server to return the first response line, especially
/// under high concurrency where the overview DB is contended. 20s was
/// too aggressive and killed healthy connections, causing reconnection
/// churn and losing in-flight pipelined commands.
const READ_LINE_TIMEOUT: Duration = Duration::from_secs(60);

/// Timeout for reading each line of a multi-line body (article data).
/// This is per-line, not per-article — large articles get many lines but
/// each individual line read must complete within this window.
const READ_BODY_LINE_TIMEOUT: Duration = Duration::from_secs(20);

use crate::capabilities::NntpCapabilities;
use crate::config::{ListActiveEntry, ServerConfig};

use crate::error::{NntpError, NntpResult};

// ---------------------------------------------------------------------------
// Response
// ---------------------------------------------------------------------------

/// NNTP response: status code + message, optionally with multi-line body.
#[derive(Debug, Clone)]
pub struct NntpResponse {
    /// Three-digit numeric status code (e.g. 200, 220, 430).
    pub code: u16,
    /// Human-readable message from the first response line.
    pub message: String,
    /// Multi-line body data, if any. Dot-stuffing has been undone.
    pub data: Option<Vec<u8>>,
}

impl NntpResponse {
    /// Returns `true` if the response indicates success (2xx).
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.code)
    }

    /// Returns `true` if the response indicates the server wants auth (480).
    pub fn needs_auth(&self) -> bool {
        self.code == 480
    }
}

// ---------------------------------------------------------------------------
// GROUP response
// ---------------------------------------------------------------------------

/// Response from the GROUP command (RFC 3977 Section 6.1.1).
#[derive(Debug, Clone)]
pub struct GroupResponse {
    /// Estimated number of articles in the group.
    pub count: u64,
    /// Lowest article number.
    pub first: u64,
    /// Highest article number.
    pub last: u64,
    /// Group name (echoed back by server).
    pub name: String,
}

// ---------------------------------------------------------------------------
// XOVER entry
// ---------------------------------------------------------------------------

/// A single entry from an XOVER/OVER response.
/// Fields correspond to the overview.fmt standard (RFC 2980 Section 3.1.1):
/// article_num \t subject \t from \t date \t message-id \t references \t bytes \t lines
#[derive(Debug, Clone)]
pub struct XoverEntry {
    pub article_num: u64,
    pub subject: String,
    pub from: String,
    pub date: String,
    pub message_id: String,
    pub references: String,
    pub bytes: u64,
    pub lines: u64,
}

// ---------------------------------------------------------------------------
// XHDR / XPAT entry
// ---------------------------------------------------------------------------

/// A single entry from an XHDR or XPAT response.
/// Each line of the multi-line response is: `article_num value`
/// where article_num and value are separated by a space (or tab).
#[derive(Debug, Clone)]
pub struct HeaderEntry {
    /// Article number.
    pub article_num: u64,
    /// The header field value for this article.
    pub value: String,
}

// ---------------------------------------------------------------------------
// Article range for XHDR / XPAT queries
// ---------------------------------------------------------------------------

/// Specifies which articles to query with XHDR/XPAT.
#[derive(Debug, Clone)]
pub enum ArticleRange {
    /// A numeric range of article numbers (inclusive).
    Range(u64, u64),
    /// A single message-id.
    MessageId(String),
}

impl ArticleRange {
    /// Format this range for use in an NNTP command string.
    fn to_command_arg(&self) -> String {
        match self {
            ArticleRange::Range(start, end) => format!("{start}-{end}"),
            ArticleRange::MessageId(mid) => normalize_message_id(mid),
        }
    }
}

// ---------------------------------------------------------------------------
// Connection state enum
// ---------------------------------------------------------------------------

/// Current state of an NNTP connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionState {
    /// Not connected to any server.
    Disconnected,
    /// TCP/TLS handshake in progress.
    Connecting,
    /// Performing USER/PASS authentication.
    Authenticating,
    /// Authenticated and idle, ready for commands.
    Ready,
    /// Currently sending/receiving article data.
    Busy,
    /// An unrecoverable error occurred; reconnection required.
    Error,
}

// ---------------------------------------------------------------------------
// Transport abstraction
// ---------------------------------------------------------------------------

/// A transport is either a plain TCP stream or a TLS-wrapped stream.
/// We box-erase so `NntpConnection` has a single concrete type.
enum Transport {
    Plain(BufReader<TcpStream>),
    Tls(Box<BufReader<tokio_rustls::client::TlsStream<TcpStream>>>),
}

impl Transport {
    /// Read a single `\r\n`-terminated line into `buf`, returning the number
    /// of bytes read (including the delimiter).
    async fn read_line(&mut self, buf: &mut String) -> std::io::Result<usize> {
        match self {
            Transport::Plain(r) => r.read_line(buf).await,
            Transport::Tls(r) => r.read_line(buf).await,
        }
    }

    /// Read bytes until `\r\n` into a `Vec<u8>`. Returns number of bytes.
    async fn read_line_bytes(&mut self, buf: &mut Vec<u8>) -> std::io::Result<usize> {
        match self {
            Transport::Plain(r) => r.read_until(b'\n', buf).await,
            Transport::Tls(r) => r.read_until(b'\n', buf).await,
        }
    }

    /// Write all bytes and flush.
    async fn write_all(&mut self, data: &[u8]) -> std::io::Result<()> {
        match self {
            Transport::Plain(r) => {
                r.get_mut().write_all(data).await?;
                r.get_mut().flush().await
            }
            Transport::Tls(r) => {
                r.get_mut().write_all(data).await?;
                r.get_mut().flush().await
            }
        }
    }

    /// Shut down the write half.
    async fn shutdown(&mut self) -> std::io::Result<()> {
        match self {
            Transport::Plain(r) => r.get_mut().shutdown().await,
            Transport::Tls(r) => r.get_mut().shutdown().await,
        }
    }
}

// ---------------------------------------------------------------------------
// NntpConnection
// ---------------------------------------------------------------------------

/// A single NNTP connection to one server.
/// Socket-level liveness heartbeat.
///
/// Supplied by the caller so a higher-level watchdog (e.g. nzb-web's idle
/// worker supervisor) can distinguish a **slow-but-alive** connection from a
/// **silent/zombied** one. Every successful line read from the NNTP socket
/// ticks the heartbeat — matches SABnzbd's `nw.timeout` model, which resets
/// on any data reception (`newswrapper.py:315`), not on article completion.
///
/// The caller owns the `Arc<AtomicU64>` and compares stored values against
/// its own clock. The stored value is millis since `epoch`.
#[derive(Clone)]
struct IoHeartbeat {
    timestamp_ms: Arc<AtomicU64>,
    epoch: Instant,
}

impl IoHeartbeat {
    fn tick(&self) {
        self.timestamp_ms
            .store(self.epoch.elapsed().as_millis() as u64, Ordering::Relaxed);
    }
}

pub struct NntpConnection {
    /// Server identifier (matches `ServerConfig::id`).
    pub server_id: String,
    /// Current connection state.
    pub state: ConnectionState,
    /// Underlying transport (set after connect).
    transport: Option<Transport>,
    /// Whether XFEATURE COMPRESS GZIP is active on this connection.
    compress_enabled: bool,
    /// Optional socket-liveness heartbeat. When set, every successful line
    /// read from the socket updates the stored timestamp (millis since
    /// `epoch`). Higher-level watchdogs use this to avoid false-positive
    /// evictions of workers that are legitimately mid-article with slow
    /// provider response times.
    io_heartbeat: Option<IoHeartbeat>,
    /// Capabilities advertised by the server (populated on connect via the
    /// `CAPABILITIES` command; falls back to conservative defaults if the
    /// server is pre-RFC-3977 and rejects the command).
    capabilities: NntpCapabilities,
}

impl NntpConnection {
    /// Create a new, disconnected connection for the given server.
    pub fn new(server_id: String) -> Self {
        Self {
            server_id,
            state: ConnectionState::Disconnected,
            transport: None,
            compress_enabled: false,
            io_heartbeat: None,
            capabilities: NntpCapabilities::default_assumed(),
        }
    }

    /// Capabilities advertised by this server (populated during `connect()`).
    ///
    /// If the server does not implement `CAPABILITIES` (pre-RFC 3977), this
    /// returns the conservative defaults from [`NntpCapabilities::default_assumed`] —
    /// which assume BODY/ARTICLE/HEAD/STAT are all available. Check
    /// [`NntpCapabilities::probed`] to distinguish "server told us" from
    /// "we assumed".
    pub fn capabilities(&self) -> &NntpCapabilities {
        &self.capabilities
    }

    /// Test-only: override the cached capabilities. Used by the test suite to
    /// exercise capability-gated branches (e.g. the BODY → ARTICLE fallback)
    /// without inventing unrealistic mock-server responses.
    #[cfg(any(test, feature = "test-support"))]
    pub fn set_capabilities_for_test(&mut self, caps: NntpCapabilities) {
        self.capabilities = caps;
    }

    /// Attach a socket-liveness heartbeat. Every successful line read from
    /// the socket will store `epoch.elapsed().as_millis() as u64` in
    /// `timestamp_ms`. Callers (e.g. nzb-web's idle-worker watchdog) compare
    /// against their own clock to detect truly silent/zombied connections
    /// without false-positively evicting slow-but-working workers.
    ///
    /// Mirrors SABnzbd's `nw.timeout` update on any data reception
    /// (`sabnzbd/newswrapper.py:315`).
    ///
    /// Optional: connections without a heartbeat behave exactly as before
    /// (no-op), so existing callers / tests are unaffected.
    pub fn set_io_heartbeat(&mut self, timestamp_ms: Arc<AtomicU64>, epoch: Instant) {
        self.io_heartbeat = Some(IoHeartbeat {
            timestamp_ms,
            epoch,
        });
    }

    /// Internal: tick the heartbeat if one is attached. Called after every
    /// successful byte read from the underlying socket.
    #[inline]
    fn tick_io_heartbeat(&self) {
        if let Some(hb) = &self.io_heartbeat {
            hb.tick();
        }
    }

    /// Returns `true` if gzip compression was negotiated on this connection.
    pub fn is_compress_enabled(&self) -> bool {
        self.compress_enabled
    }

    /// Disable compression on this connection.
    ///
    /// Used as a fallback when the server sends corrupted compressed data.
    /// The server may still send compressed responses for the rest of the session,
    /// but we stop attempting to decompress them (treating raw bytes as-is).
    pub fn disable_compression(&mut self) {
        self.compress_enabled = false;
    }

    // ------------------------------------------------------------------
    // Connect
    // ------------------------------------------------------------------

    /// Connect to the NNTP server described by `config`.
    ///
    /// This performs TCP connection, optional TLS upgrade, reads the welcome
    /// banner, and authenticates if credentials are configured. The whole
    /// sequence is bounded by `config.connect_timeout_secs` — a server that's
    /// unreachable (firewalled, dead host) would otherwise hang on the TCP
    /// handshake for the OS-level default (minutes), which stalls failover
    /// to the next-priority server.
    pub async fn connect(&mut self, config: &ServerConfig) -> NntpResult<()> {
        let budget = Duration::from_secs(config.connect_timeout_secs.max(1) as u64);
        match tokio::time::timeout(budget, self.connect_inner(config)).await {
            Ok(result) => result,
            Err(_) => {
                self.state = ConnectionState::Error;
                Err(NntpError::Timeout(format!(
                    "connect to {}:{} did not complete within {}s",
                    config.host, config.port, config.connect_timeout_secs
                )))
            }
        }
    }

    async fn connect_inner(&mut self, config: &ServerConfig) -> NntpResult<()> {
        self.state = ConnectionState::Connecting;
        let t_connect = Instant::now();

        let addr = format!("{}:{}", config.host, config.port);
        info!(
            server = %self.server_id,
            %addr,
            ssl = config.ssl,
            ssl_verify = config.ssl_verify,
            // Avoid logging the username — it may be PII for email-shaped
            // logins, an account identifier worth protecting in shared logs.
            // Just record whether credentials were configured.
            authenticated = config.username.is_some(),
            connections = config.connections,
            compress = config.compress,
            "NNTP connecting"
        );

        // Rate-limit connection attempts per host to prevent thundering herd
        let _gate_permit = crate::connect_gate::acquire(&config.host).await;

        // 1. TCP connect (optionally through SOCKS5 proxy)
        let tcp = if let Some(proxy_url) = config
            .proxy_url
            .as_deref()
            .map(str::trim)
            .filter(|u| !u.is_empty())
        {
            let proxy = parse_socks5_url(proxy_url).map_err(|e| {
                self.state = ConnectionState::Error;
                NntpError::Connection(format!("Invalid proxy URL: {e}"))
            })?;
            debug!(server = %self.server_id, proxy = %proxy.addr, "Connecting via SOCKS5 proxy");
            let stream = if let Some((user, pass)) = &proxy.auth {
                Socks5Stream::connect_with_password(proxy.addr.as_str(), addr.as_str(), user, pass)
                    .await
            } else {
                Socks5Stream::connect(proxy.addr.as_str(), addr.as_str()).await
            };
            stream
                .map_err(|e| {
                    self.state = ConnectionState::Error;
                    NntpError::Connection(format!("SOCKS5 connect to {addr} via proxy: {e}"))
                })?
                .into_inner()
        } else {
            TcpStream::connect(&addr).await.map_err(|e| {
                self.state = ConnectionState::Error;
                NntpError::Connection(format!("TCP connect to {addr}: {e}"))
            })?
        };
        tcp.set_nodelay(true).ok();
        if config.recv_buffer_size > 0 {
            let sock = socket2::SockRef::from(&tcp);
            if let Err(e) = sock.set_recv_buffer_size(config.recv_buffer_size as usize) {
                warn!(server = %self.server_id, size = config.recv_buffer_size, "failed to set SO_RCVBUF: {e}");
            }
        }
        info!(server = %self.server_id, %addr, "TCP connected");

        // 2. Optional TLS
        if config.ssl {
            info!(server = %self.server_id, %addr, ssl_verify = config.ssl_verify, "TLS handshake starting");
            let tls_config =
                build_tls_config(config.ssl_verify, config.trusted_fingerprint.as_deref())?;
            let connector = TlsConnector::from(Arc::new(tls_config));

            let server_name =
                rustls_pki_types::ServerName::try_from(config.host.clone()).map_err(|e| {
                    NntpError::Tls(format!("Invalid server name '{}': {e}", config.host))
                })?;

            let tls_stream = connector.connect(server_name, tcp).await.map_err(|e| {
                self.state = ConnectionState::Error;
                NntpError::Tls(format!("TLS handshake with {addr}: {e}"))
            })?;

            info!(server = %self.server_id, %addr, "TLS handshake complete");
            self.transport = Some(Transport::Tls(Box::new(BufReader::with_capacity(
                256 * 1024,
                tls_stream,
            ))));
        } else {
            self.transport = Some(Transport::Plain(BufReader::with_capacity(256 * 1024, tcp)));
        }

        // 3. Read welcome banner
        let welcome = self.read_response_line().await?;
        info!(server = %self.server_id, code = welcome.code, msg = %welcome.message, "NNTP welcome banner");

        match welcome.code {
            200 | 201 => {} // posting allowed / posting not allowed — both fine
            502 => {
                warn!(
                    server = %self.server_id,
                    %addr,
                    code = 502,
                    msg = %welcome.message,
                    "NNTP server rejected connection at welcome (502 Service Unavailable)"
                );
                self.state = ConnectionState::Error;
                return Err(NntpError::ServiceUnavailable(welcome.message));
            }
            _ => {
                warn!(
                    server = %self.server_id,
                    %addr,
                    code = welcome.code,
                    msg = %welcome.message,
                    "NNTP unexpected welcome code"
                );
                self.state = ConnectionState::Error;
                return Err(NntpError::Protocol(format!(
                    "Unexpected welcome code {}: {}",
                    welcome.code, welcome.message
                )));
            }
        }

        // 4. Authenticate if credentials are provided
        let t_auth_start = std::time::Instant::now();
        if config.username.is_some() {
            self.authenticate(config).await?;
        } else {
            self.state = ConnectionState::Ready;
        }
        let auth_ms = t_auth_start.elapsed().as_millis() as u64;

        // 5. Query server capabilities (RFC 3977 §5.2). If this fails we fall
        //    back to conservative defaults assuming ARTICLE/BODY/HEAD/STAT are
        //    all available, matching legacy pre-3977 client behaviour.
        let t_caps_start = std::time::Instant::now();
        if let Err(e) = self.query_capabilities().await {
            debug!(
                server = %self.server_id,
                error = %e,
                "CAPABILITIES query failed — falling back to default feature assumptions"
            );
            self.capabilities = NntpCapabilities::default_assumed();
        }
        let caps_ms = t_caps_start.elapsed().as_millis() as u64;

        // 5b. If the server advertises MODE-READER but not READER, transition
        //     to reader mode. Post-transition commands (BODY/STAT) require it.
        let t_mode_start = std::time::Instant::now();
        if self.capabilities.mode_reader_required
            && !self.capabilities.reader
            && let Err(e) = self.enter_reader_mode().await
        {
            warn!(
                server = %self.server_id,
                error = %e,
                "MODE READER failed — server may still accept reader commands"
            );
        }
        let mode_ms = t_mode_start.elapsed().as_millis() as u64;

        // 6. Negotiate compression if configured
        let t_compress_start = std::time::Instant::now();
        if config.compress
            && let Err(e) = self.negotiate_compression().await
        {
            debug!(server = %self.server_id, error = %e, "Compression negotiation failed, continuing without");
        }
        let compress_ms = t_compress_start.elapsed().as_millis() as u64;

        info!(
            server = %self.server_id,
            compress = self.compress_enabled,
            connect_ms = t_connect.elapsed().as_millis() as u64,
            auth_ms,
            caps_ms,
            mode_ms,
            compress_ms,
            probed = self.capabilities.probed,
            "nntp_connect"
        );
        Ok(())
    }

    // ------------------------------------------------------------------
    // Authentication
    // ------------------------------------------------------------------

    /// Perform USER/PASS authentication.
    async fn authenticate(&mut self, config: &ServerConfig) -> NntpResult<()> {
        self.state = ConnectionState::Authenticating;

        let username = config
            .username
            .as_deref()
            .ok_or_else(|| NntpError::Auth("No username configured".into()))?;

        // NB: do not log the username field at info — it can be PII for
        // email-shaped logins. The fact that authentication is starting is
        // already implied by the connection-attempt log; downgrade detail
        // to debug.
        info!(
            server = %self.server_id,
            host = %config.host,
            "NNTP authenticating (AUTHINFO USER)"
        );
        debug!(server = %self.server_id, username = %username, "AUTHINFO USER detail");

        // Try AUTHINFO USER first (RFC 4643), fall back to USER (RFC 2980)
        self.send_command(&format!("AUTHINFO USER {username}"))
            .await?;
        let resp = self.read_response_line().await?;

        info!(
            server = %self.server_id,
            code = resp.code,
            msg = %resp.message,
            "NNTP AUTHINFO USER response"
        );

        match resp.code {
            281 => {
                // Authenticated with just username (unusual but valid)
                info!(server = %self.server_id, "NNTP auth complete (username only)");
                self.state = ConnectionState::Ready;
                return Ok(());
            }
            381 | 480 => {
                // 381 = password required (standard)
                // 480 = authentication required (some servers send this to mean "continue")
                debug!(server = %self.server_id, code = resp.code, "NNTP server wants password");
            }
            481 | 482 => {
                // 481 = credentials rejected (RFC 4643)
                // 482 = non-standard but used by providers for block/account exhausted
                warn!(
                    server = %self.server_id,
                    host = %config.host,
                    code = resp.code,
                    msg = %resp.message,
                    "NNTP AUTHINFO USER rejected — credentials invalid or account blocked"
                );
                self.state = ConnectionState::Error;
                return Err(NntpError::Auth(format!(
                    "USER rejected ({}): {}",
                    resp.code, resp.message
                )));
            }
            502 => {
                warn!(
                    server = %self.server_id,
                    host = %config.host,
                    code = 502,
                    msg = %resp.message,
                    "NNTP service unavailable during AUTH USER"
                );
                self.state = ConnectionState::Error;
                return Err(NntpError::ServiceUnavailable(resp.message));
            }
            _ => {
                warn!(
                    server = %self.server_id,
                    host = %config.host,
                    code = resp.code,
                    msg = %resp.message,
                    "NNTP unexpected AUTH USER response"
                );
                self.state = ConnectionState::Error;
                return Err(NntpError::Protocol(format!(
                    "Unexpected USER response {}: {}",
                    resp.code, resp.message
                )));
            }
        }

        // Send PASS
        let password = config.password.as_deref().ok_or_else(|| {
            NntpError::Auth("Server requires password but none configured".into())
        })?;

        debug!(server = %self.server_id, "NNTP sending AUTHINFO PASS");
        self.send_command(&format!("AUTHINFO PASS {password}"))
            .await?;
        let resp = self.read_response_line().await?;

        info!(
            server = %self.server_id,
            code = resp.code,
            msg = %resp.message,
            "NNTP AUTHINFO PASS response"
        );

        match resp.code {
            281 => {
                info!(server = %self.server_id, host = %config.host, "NNTP auth successful");
                self.state = ConnectionState::Ready;
                Ok(())
            }
            481 | 482 => {
                // 481 = credentials rejected (RFC 4643)
                // 482 = non-standard but used by providers for block/account exhausted
                warn!(
                    server = %self.server_id,
                    host = %config.host,
                    code = resp.code,
                    msg = %resp.message,
                    "NNTP AUTHINFO PASS rejected — credentials invalid or account blocked"
                );
                self.state = ConnectionState::Error;
                Err(NntpError::Auth(format!(
                    "PASS rejected ({}): {}",
                    resp.code, resp.message
                )))
            }
            502 => {
                warn!(
                    server = %self.server_id,
                    host = %config.host,
                    code = 502,
                    msg = %resp.message,
                    "NNTP service unavailable during AUTH PASS"
                );
                self.state = ConnectionState::Error;
                Err(NntpError::ServiceUnavailable(resp.message))
            }
            _ => {
                warn!(
                    server = %self.server_id,
                    host = %config.host,
                    code = resp.code,
                    msg = %resp.message,
                    "NNTP unexpected AUTH PASS response"
                );
                self.state = ConnectionState::Error;
                Err(NntpError::Protocol(format!(
                    "Unexpected PASS response {}: {}",
                    resp.code, resp.message
                )))
            }
        }
    }

    // ------------------------------------------------------------------
    // CAPABILITIES (RFC 3977 §5.2)
    // ------------------------------------------------------------------

    /// Query the server's CAPABILITIES and store them on the connection.
    ///
    /// A compliant RFC 3977 server responds with `101 Capability list:`
    /// followed by a multi-line body listing each capability. Older servers
    /// (INN 1.x, Diablo pre-2010) don't implement the command at all and
    /// respond with `500 Unknown command` — in that case the caller falls
    /// back to conservative defaults.
    async fn query_capabilities(&mut self) -> NntpResult<()> {
        if self.state != ConnectionState::Ready {
            return Err(NntpError::Protocol(format!(
                "Cannot query CAPABILITIES in state {:?}",
                self.state
            )));
        }
        debug!(server = %self.server_id, "CAPABILITIES query starting");
        self.send_command("CAPABILITIES").await?;
        let resp = self.read_response_line().await?;

        match resp.code {
            101 => {
                let body = self.read_multiline_body().await?;
                let mut caps = NntpCapabilities::parse(&body);
                // Defensive: a probed response that yields zero usable
                // content-command flags would silently break BODY/STAT for
                // every caller. Real RFC 3977 servers always advertise either
                // READER or one of OVER/HDR/POST/IHAVE, so an empty result
                // here almost certainly means a non-compliant or stripped
                // capability list rather than a true transit-only server.
                // Treat as "unknown" and fall back to permissive defaults
                // (preserves pre-3977 client behaviour) while keeping the
                // parsed metadata (version/implementation/list_keywords) so
                // ops still see what the server reported.
                if !caps.have_body && !caps.have_stat && !caps.have_article && !caps.have_head {
                    debug!(
                        server = %self.server_id,
                        "CAPABILITIES probed but no reader-mode flags advertised — assuming defaults"
                    );
                    caps.have_article = true;
                    caps.have_body = true;
                    caps.have_head = true;
                    caps.have_stat = true;
                }
                info!(
                    server = %self.server_id,
                    reader = caps.reader,
                    have_body = caps.have_body,
                    have_stat = caps.have_stat,
                    have_article = caps.have_article,
                    hdr = caps.hdr,
                    over = caps.over,
                    implementation = caps.implementation.as_deref().unwrap_or("?"),
                    "NNTP capabilities"
                );
                self.capabilities = caps;
                Ok(())
            }
            _ => {
                debug!(
                    server = %self.server_id,
                    code = resp.code,
                    "CAPABILITIES unsupported — using conservative defaults"
                );
                self.capabilities = NntpCapabilities::default_assumed();
                Ok(())
            }
        }
    }

    /// Transition from transit mode to reader mode. Required by servers that
    /// advertise `MODE-READER` in their CAPABILITIES list (they expose
    /// IHAVE/transit commands at connect, and you must switch modes before
    /// issuing ARTICLE/BODY/HEAD/STAT).
    async fn enter_reader_mode(&mut self) -> NntpResult<()> {
        if self.state != ConnectionState::Ready {
            return Err(NntpError::Protocol(format!(
                "Cannot MODE READER in state {:?}",
                self.state
            )));
        }
        debug!(server = %self.server_id, "MODE READER transition");
        self.send_command("MODE READER").await?;
        let resp = self.read_response_line().await?;
        match resp.code {
            200 | 201 => {
                debug!(
                    server = %self.server_id,
                    code = resp.code,
                    "MODE READER accepted"
                );
                // After MODE READER, RFC 3977 §5.3 says the full reader
                // command set is active. Re-derive the feature flags.
                self.capabilities.reader = true;
                self.capabilities.have_article = true;
                self.capabilities.have_body = true;
                self.capabilities.have_head = true;
                self.capabilities.have_stat = true;
                Ok(())
            }
            _ => Err(NntpError::Protocol(format!(
                "Unexpected MODE READER response {}: {}",
                resp.code, resp.message
            ))),
        }
    }

    // ------------------------------------------------------------------
    // XFEATURE COMPRESS GZIP negotiation
    // ------------------------------------------------------------------

    /// Negotiate XFEATURE COMPRESS GZIP with the server.
    async fn negotiate_compression(&mut self) -> NntpResult<()> {
        debug!(server = %self.server_id, "Compression negotiation starting (LIST EXTENSIONS)");
        self.send_command("LIST EXTENSIONS").await?;
        let resp = self.read_response_line().await?;

        if resp.code == 202 {
            let data = self.read_multiline_body().await?;
            let text = String::from_utf8_lossy(&data);
            let supports_compress = text
                .lines()
                .any(|line| line.trim().eq_ignore_ascii_case("XFEATURE COMPRESS GZIP"));

            if !supports_compress {
                debug!(server = %self.server_id, "Server does not advertise XFEATURE COMPRESS GZIP");
                return Ok(());
            }
        } else {
            debug!(server = %self.server_id, code = resp.code, "LIST EXTENSIONS not supported");
            return Ok(());
        }

        self.send_command("XFEATURE COMPRESS GZIP").await?;
        let resp = self.read_response_line().await?;

        if resp.code == 290 {
            self.compress_enabled = true;
            debug!(server = %self.server_id, "GZIP compression enabled");
        } else {
            debug!(server = %self.server_id, code = resp.code, "XFEATURE COMPRESS GZIP rejected");
        }

        Ok(())
    }

    // ------------------------------------------------------------------
    // Decompression helper
    // ------------------------------------------------------------------

    /// Read a multi-line body, decompressing if gzip compression is active.
    async fn read_multiline_body_maybe_decompress(&mut self) -> NntpResult<Vec<u8>> {
        let raw = self.read_multiline_body().await?;

        if self.compress_enabled && raw.len() >= 2 && raw[0] == 0x1f && raw[1] == 0x8b {
            use flate2::read::GzDecoder;
            use std::io::Read;

            let mut decoder = GzDecoder::new(&raw[..]);
            let mut decompressed = Vec::with_capacity(raw.len() * 4);
            match decoder.read_to_end(&mut decompressed) {
                Ok(_) => {
                    trace!(
                        server = %self.server_id,
                        compressed = raw.len(),
                        decompressed = decompressed.len(),
                        "Decompressed gzip response"
                    );
                    Ok(decompressed)
                }
                Err(e) => {
                    debug!(
                        server = %self.server_id,
                        error = %e,
                        "Gzip decode failed, using raw data"
                    );
                    Ok(raw)
                }
            }
        } else {
            Ok(raw)
        }
    }

    // ------------------------------------------------------------------
    // ARTICLE command
    // ------------------------------------------------------------------

    /// Fetch a complete article by message-id.
    ///
    /// Sends `ARTICLE <message-id>` and reads the multi-line response.
    /// Returns the raw article data (headers + blank line + body).
    pub async fn fetch_article(&mut self, message_id: &str) -> NntpResult<NntpResponse> {
        if self.state != ConnectionState::Ready {
            return Err(NntpError::Protocol(format!(
                "Cannot fetch article in state {:?}",
                self.state
            )));
        }
        self.state = ConnectionState::Busy;

        let mid = normalize_message_id(message_id);
        self.send_command(&format!("ARTICLE {mid}"))
            .await
            .inspect_err(|_| self.state = ConnectionState::Error)?;

        let status = self
            .read_response_line()
            .await
            .inspect_err(|_| self.state = ConnectionState::Error)?;

        match status.code {
            220 => {
                // Article follows — read multi-line body
                let data = self
                    .read_multiline_body_maybe_decompress()
                    .await
                    .inspect_err(|_| self.state = ConnectionState::Error)?;
                self.state = ConnectionState::Ready;
                Ok(NntpResponse {
                    code: status.code,
                    message: status.message,
                    data: Some(data),
                })
            }
            430 => {
                self.state = ConnectionState::Ready;
                Err(NntpError::ArticleNotFound(mid))
            }
            411 => {
                self.state = ConnectionState::Ready;
                Err(NntpError::NoSuchGroup(status.message))
            }
            412 | 420 => {
                self.state = ConnectionState::Ready;
                Err(NntpError::NoArticleSelected(status.message))
            }
            403 => {
                self.state = ConnectionState::Error;
                Err(NntpError::PermissionDenied(status.message))
            }
            480 => {
                warn!(
                    server = %self.server_id,
                    code = 480,
                    msg = %status.message,
                    article = %mid,
                    "NNTP auth required during ARTICLE fetch — session expired?"
                );
                self.state = ConnectionState::Error;
                Err(NntpError::AuthRequired(status.message))
            }
            481 | 482 => {
                warn!(
                    server = %self.server_id,
                    code = status.code,
                    msg = %status.message,
                    article = %mid,
                    "NNTP ARTICLE rejected — auth/account error"
                );
                self.state = ConnectionState::Error;
                Err(NntpError::Auth(format!(
                    "ARTICLE rejected ({}): {}",
                    status.code, status.message
                )))
            }
            502 => {
                warn!(
                    server = %self.server_id,
                    code = 502,
                    msg = %status.message,
                    article = %mid,
                    "NNTP service unavailable during ARTICLE fetch"
                );
                self.state = ConnectionState::Error;
                Err(NntpError::ServiceUnavailable(status.message))
            }
            _ => {
                warn!(
                    server = %self.server_id,
                    code = status.code,
                    msg = %status.message,
                    article = %mid,
                    "NNTP unexpected ARTICLE response"
                );
                self.state = ConnectionState::Error;
                Err(crate::error::unexpected_article_response(
                    status.code,
                    status.message,
                ))
            }
        }
    }

    // ------------------------------------------------------------------
    // STAT command (pre-check)
    // ------------------------------------------------------------------

    /// Check if an article exists on the server without downloading it.
    ///
    /// Sends `STAT <message-id>`. Returns `Ok(response)` with code 223 if
    /// the article exists, or an appropriate error.
    ///
    /// Returns [`NntpError::ArticleNotFound`] if the server's capabilities
    /// report STAT is unsupported. This lets dispatchers treat capability
    /// gaps as "this server can't confirm — try another" without flagging
    /// the connection as broken (which a `Protocol` error would imply).
    pub async fn stat_article(&mut self, message_id: &str) -> NntpResult<NntpResponse> {
        if !self.capabilities.have_stat {
            return Err(NntpError::ArticleNotFound(normalize_message_id(message_id)));
        }
        if self.state != ConnectionState::Ready {
            return Err(NntpError::Protocol(format!(
                "Cannot STAT in state {:?}",
                self.state
            )));
        }
        self.state = ConnectionState::Busy;

        let mid = normalize_message_id(message_id);
        self.send_command(&format!("STAT {mid}"))
            .await
            .inspect_err(|_| self.state = ConnectionState::Error)?;

        let resp = self
            .read_response_line()
            .await
            .inspect_err(|_| self.state = ConnectionState::Error)?;
        self.state = ConnectionState::Ready;

        match resp.code {
            223 => Ok(resp),
            430 => Err(NntpError::ArticleNotFound(mid)),
            480 => {
                self.state = ConnectionState::Error;
                Err(NntpError::AuthRequired(resp.message))
            }
            481 | 482 => {
                self.state = ConnectionState::Error;
                Err(NntpError::Auth(format!(
                    "STAT rejected ({}): {}",
                    resp.code, resp.message
                )))
            }
            _ => Err(NntpError::Protocol(format!(
                "Unexpected STAT response {}: {}",
                resp.code, resp.message
            ))),
        }
    }

    // ------------------------------------------------------------------
    // GROUP command (RFC 3977 Section 6.1.1)
    // ------------------------------------------------------------------

    /// Select a newsgroup and return its article range.
    ///
    /// Sends `GROUP <name>` and parses the `211` response:
    /// `211 count first last name`
    pub async fn group(&mut self, name: &str) -> NntpResult<GroupResponse> {
        if self.state != ConnectionState::Ready {
            return Err(NntpError::Protocol(format!(
                "Cannot GROUP in state {:?}",
                self.state
            )));
        }
        self.state = ConnectionState::Busy;

        self.send_command(&format!("GROUP {name}"))
            .await
            .inspect_err(|_| self.state = ConnectionState::Error)?;
        let resp = self
            .read_response_line()
            .await
            .inspect_err(|_| self.state = ConnectionState::Error)?;

        self.state = ConnectionState::Ready;

        match resp.code {
            211 => {
                let parts: Vec<&str> = resp.message.split_whitespace().collect();
                if parts.len() < 3 {
                    return Err(NntpError::Protocol(format!(
                        "Malformed GROUP response: {}",
                        resp.message
                    )));
                }
                Ok(GroupResponse {
                    count: parts[0].parse().unwrap_or(0),
                    first: parts[1].parse().unwrap_or(0),
                    last: parts[2].parse().unwrap_or(0),
                    name: parts.get(3).unwrap_or(&name).to_string(),
                })
            }
            411 => Err(NntpError::NoSuchGroup(name.to_string())),
            480 => {
                warn!(server = %self.server_id, code = 480, msg = %resp.message, group = %name, "NNTP auth required during GROUP");
                self.state = ConnectionState::Error;
                Err(NntpError::AuthRequired(resp.message))
            }
            481 | 482 => {
                warn!(server = %self.server_id, code = resp.code, msg = %resp.message, group = %name, "NNTP GROUP rejected");
                self.state = ConnectionState::Error;
                Err(NntpError::Auth(format!(
                    "GROUP rejected ({}): {}",
                    resp.code, resp.message
                )))
            }
            502 => {
                warn!(server = %self.server_id, code = 502, msg = %resp.message, group = %name, "NNTP service unavailable during GROUP");
                self.state = ConnectionState::Error;
                Err(NntpError::ServiceUnavailable(resp.message))
            }
            _ => {
                warn!(server = %self.server_id, code = resp.code, msg = %resp.message, group = %name, "NNTP unexpected GROUP response");
                self.state = ConnectionState::Error;
                Err(NntpError::Protocol(format!(
                    "Unexpected GROUP response {}: {}",
                    resp.code, resp.message
                )))
            }
        }
    }

    // ------------------------------------------------------------------
    // XOVER command (RFC 2980 Section 2.8)
    // ------------------------------------------------------------------

    /// Fetch overview data for a range of article numbers.
    ///
    /// Sends `XOVER start-end` and parses the tab-delimited multi-line response.
    /// Response code 224 means overview data follows (dot-terminated).
    pub async fn xover(&mut self, start: u64, end: u64) -> NntpResult<Vec<XoverEntry>> {
        if self.state != ConnectionState::Ready {
            return Err(NntpError::Protocol(format!(
                "Cannot XOVER in state {:?}",
                self.state
            )));
        }
        self.state = ConnectionState::Busy;
        let t_xover = Instant::now();

        self.send_command(&format!("XOVER {start}-{end}"))
            .await
            .inspect_err(|_| self.state = ConnectionState::Error)?;
        let status = self
            .read_response_line()
            .await
            .inspect_err(|_| self.state = ConnectionState::Error)?;

        match status.code {
            224 => {
                let data = self
                    .read_multiline_body_maybe_decompress()
                    .await
                    .inspect_err(|_| self.state = ConnectionState::Error)?;
                self.state = ConnectionState::Ready;
                let entries = parse_xover_data(&data);
                debug!(
                    server = %self.server_id,
                    start,
                    end,
                    range = end.saturating_sub(start) + 1,
                    articles = entries.len(),
                    bytes = data.len(),
                    xover_ms = t_xover.elapsed().as_millis() as u64,
                    "nntp_xover"
                );
                Ok(entries)
            }
            420 => {
                self.state = ConnectionState::Ready;
                Ok(Vec::new()) // No articles in range
            }
            412 => {
                self.state = ConnectionState::Ready;
                Err(NntpError::NoSuchGroup(
                    "No newsgroup selected (send GROUP first)".into(),
                ))
            }
            481 | 482 => {
                self.state = ConnectionState::Error;
                Err(NntpError::Auth(format!(
                    "XOVER rejected ({}): {}",
                    status.code, status.message
                )))
            }
            502 => {
                self.state = ConnectionState::Error;
                Err(NntpError::ServiceUnavailable(status.message))
            }
            _ => {
                self.state = ConnectionState::Error;
                Err(NntpError::Protocol(format!(
                    "Unexpected XOVER response {}: {}",
                    status.code, status.message
                )))
            }
        }
    }

    // ------------------------------------------------------------------
    // XHDR command (RFC 2980 Section 2.6)
    // ------------------------------------------------------------------

    /// Retrieve a specific header field for a range of articles or a single message-id.
    ///
    /// Sends `XHDR header range` and parses the multi-line response.
    /// Response code 221 means header data follows (dot-terminated).
    pub async fn xhdr(
        &mut self,
        header: &str,
        range: ArticleRange,
    ) -> NntpResult<Vec<HeaderEntry>> {
        if self.state != ConnectionState::Ready {
            return Err(NntpError::Protocol(format!(
                "Cannot XHDR in state {:?}",
                self.state
            )));
        }
        self.state = ConnectionState::Busy;

        let arg = range.to_command_arg();
        self.send_command(&format!("XHDR {header} {arg}"))
            .await
            .inspect_err(|_| self.state = ConnectionState::Error)?;
        let status = self
            .read_response_line()
            .await
            .inspect_err(|_| self.state = ConnectionState::Error)?;

        match status.code {
            221 => {
                let data = self
                    .read_multiline_body_maybe_decompress()
                    .await
                    .inspect_err(|_| self.state = ConnectionState::Error)?;
                self.state = ConnectionState::Ready;
                Ok(parse_header_data(&data))
            }
            420 => {
                self.state = ConnectionState::Ready;
                Ok(Vec::new()) // No articles in range
            }
            412 => {
                self.state = ConnectionState::Ready;
                Err(NntpError::NoSuchGroup(
                    "No newsgroup selected (send GROUP first)".into(),
                ))
            }
            430 => {
                self.state = ConnectionState::Ready;
                Err(NntpError::ArticleNotFound(arg))
            }
            481 | 482 => {
                self.state = ConnectionState::Error;
                Err(NntpError::Auth(format!(
                    "XHDR rejected ({}): {}",
                    status.code, status.message
                )))
            }
            502 => {
                self.state = ConnectionState::Error;
                Err(NntpError::ServiceUnavailable(status.message))
            }
            _ => {
                self.state = ConnectionState::Error;
                Err(NntpError::Protocol(format!(
                    "Unexpected XHDR response {}: {}",
                    status.code, status.message
                )))
            }
        }
    }

    // ------------------------------------------------------------------
    // XPAT command (RFC 2980 Section 2.8)
    // ------------------------------------------------------------------

    /// Search for articles matching wildmat pattern(s) on a specific header field.
    ///
    /// Sends `XPAT header range pattern [pattern...]` and parses matching results.
    /// Patterns use NNTP wildmat syntax: `*` matches any string, `?` matches one char.
    pub async fn xpat(
        &mut self,
        header: &str,
        range: ArticleRange,
        patterns: &[&str],
    ) -> NntpResult<Vec<HeaderEntry>> {
        if self.state != ConnectionState::Ready {
            return Err(NntpError::Protocol(format!(
                "Cannot XPAT in state {:?}",
                self.state
            )));
        }
        if patterns.is_empty() {
            return Err(NntpError::Protocol(
                "XPAT requires at least one pattern".into(),
            ));
        }
        self.state = ConnectionState::Busy;

        let arg = range.to_command_arg();
        let pattern_str = patterns.join(" ");
        self.send_command(&format!("XPAT {header} {arg} {pattern_str}"))
            .await
            .inspect_err(|_| self.state = ConnectionState::Error)?;
        let status = self
            .read_response_line()
            .await
            .inspect_err(|_| self.state = ConnectionState::Error)?;

        match status.code {
            221 => {
                let data = self
                    .read_multiline_body_maybe_decompress()
                    .await
                    .inspect_err(|_| self.state = ConnectionState::Error)?;
                self.state = ConnectionState::Ready;
                Ok(parse_header_data(&data))
            }
            420 => {
                self.state = ConnectionState::Ready;
                Ok(Vec::new()) // No articles matched
            }
            412 => {
                self.state = ConnectionState::Ready;
                Err(NntpError::NoSuchGroup(
                    "No newsgroup selected (send GROUP first)".into(),
                ))
            }
            430 => {
                self.state = ConnectionState::Ready;
                Err(NntpError::ArticleNotFound(arg))
            }
            481 | 482 => {
                self.state = ConnectionState::Error;
                Err(NntpError::Auth(format!(
                    "XPAT rejected ({}): {}",
                    status.code, status.message
                )))
            }
            502 => {
                self.state = ConnectionState::Error;
                Err(NntpError::ServiceUnavailable(status.message))
            }
            _ => {
                self.state = ConnectionState::Error;
                Err(NntpError::Protocol(format!(
                    "Unexpected XPAT response {}: {}",
                    status.code, status.message
                )))
            }
        }
    }

    // ------------------------------------------------------------------
    // BODY command
    // ------------------------------------------------------------------

    /// Fetch article body by message-id (headers excluded).
    ///
    /// Sends `BODY <message-id>` and returns the raw body data.
    ///
    /// Capability-aware: if the server's advertised capabilities indicate
    /// BODY is not supported but ARTICLE is, this transparently issues
    /// `ARTICLE <message-id>` and strips the header section before returning,
    /// so the caller always receives body-only data regardless of which
    /// command was actually used. Mirrors SABnzbd's `have_body` switch.
    pub async fn fetch_body(&mut self, message_id: &str) -> NntpResult<NntpResponse> {
        if !self.capabilities.have_body && self.capabilities.have_article {
            trace!(
                server = %self.server_id,
                "Server lacks BODY capability — falling back to ARTICLE and stripping headers"
            );
            let mut resp = self.fetch_article(message_id).await?;
            if let Some(data) = resp.data.as_mut() {
                *data = strip_article_headers(data);
            }
            // Normalize the status code so callers can't tell which path was
            // taken — BODY's success code is 222.
            resp.code = 222;
            return Ok(resp);
        }
        if self.state != ConnectionState::Ready {
            return Err(NntpError::Protocol(format!(
                "Cannot BODY in state {:?}",
                self.state
            )));
        }
        self.state = ConnectionState::Busy;

        let mid = normalize_message_id(message_id);
        self.send_command(&format!("BODY {mid}"))
            .await
            .inspect_err(|_| self.state = ConnectionState::Error)?;
        let status = self
            .read_response_line()
            .await
            .inspect_err(|_| self.state = ConnectionState::Error)?;

        match status.code {
            222 => {
                let data = self
                    .read_multiline_body_maybe_decompress()
                    .await
                    .inspect_err(|_| self.state = ConnectionState::Error)?;
                self.state = ConnectionState::Ready;
                Ok(NntpResponse {
                    code: status.code,
                    message: status.message,
                    data: Some(data),
                })
            }
            430 => {
                self.state = ConnectionState::Ready;
                Err(NntpError::ArticleNotFound(mid))
            }
            412 | 420 => {
                self.state = ConnectionState::Ready;
                Err(NntpError::NoArticleSelected(status.message))
            }
            480 => {
                self.state = ConnectionState::Error;
                Err(NntpError::AuthRequired(status.message))
            }
            481 | 482 => {
                self.state = ConnectionState::Error;
                Err(NntpError::Auth(format!(
                    "BODY rejected ({}): {}",
                    status.code, status.message
                )))
            }
            502 => {
                self.state = ConnectionState::Error;
                Err(NntpError::ServiceUnavailable(status.message))
            }
            _ => {
                self.state = ConnectionState::Error;
                Err(NntpError::Protocol(format!(
                    "Unexpected BODY response {}: {}",
                    status.code, status.message
                )))
            }
        }
    }

    // ------------------------------------------------------------------
    // LIST ACTIVE (RFC 3977 Section 7.6.3)
    // ------------------------------------------------------------------

    /// Fetch the list of active newsgroups from the server.
    ///
    /// Sends `LIST ACTIVE` and parses the multi-line response.
    /// Each line: `groupname last first posting_flag`
    /// Response code 215 means list follows (dot-terminated).
    ///
    /// Optionally pass a wildmat pattern to filter groups (e.g., "alt.binaries.*").
    pub async fn list_active(&mut self, wildmat: Option<&str>) -> NntpResult<Vec<ListActiveEntry>> {
        if self.state != ConnectionState::Ready {
            return Err(NntpError::Protocol(format!(
                "Cannot LIST ACTIVE in state {:?}",
                self.state
            )));
        }
        self.state = ConnectionState::Busy;

        let cmd = match wildmat {
            Some(pattern) => format!("LIST ACTIVE {pattern}"),
            None => "LIST ACTIVE".to_string(),
        };
        self.send_command(&cmd)
            .await
            .inspect_err(|_| self.state = ConnectionState::Error)?;
        let status = self
            .read_response_line()
            .await
            .inspect_err(|_| self.state = ConnectionState::Error)?;

        match status.code {
            215 => {
                let data = self
                    .read_multiline_body_maybe_decompress()
                    .await
                    .inspect_err(|_| self.state = ConnectionState::Error)?;
                self.state = ConnectionState::Ready;
                Ok(parse_list_active_data(&data))
            }
            481 | 482 => {
                self.state = ConnectionState::Error;
                Err(NntpError::Auth(format!(
                    "LIST ACTIVE rejected ({}): {}",
                    status.code, status.message
                )))
            }
            502 => {
                self.state = ConnectionState::Error;
                Err(NntpError::ServiceUnavailable(status.message))
            }
            _ => {
                self.state = ConnectionState::Error;
                Err(NntpError::Protocol(format!(
                    "Unexpected LIST ACTIVE response {}: {}",
                    status.code, status.message
                )))
            }
        }
    }

    // ------------------------------------------------------------------
    // POST (RFC 3977 Section 6.3.1)
    // ------------------------------------------------------------------

    /// Post an article to the server.
    ///
    /// The `article` parameter should be a complete article including headers
    /// and body, separated by a blank line. Headers must include at least
    /// From, Newsgroups, and Subject.
    ///
    /// Returns the server's response after posting.
    /// Response code 240 = article posted successfully.
    /// Response code 440 = posting not permitted.
    /// Response code 441 = posting failed.
    pub async fn post_article(&mut self, article: &str) -> NntpResult<NntpResponse> {
        if self.state != ConnectionState::Ready {
            return Err(NntpError::Protocol(format!(
                "Cannot POST in state {:?}",
                self.state
            )));
        }
        self.state = ConnectionState::Busy;

        // Send POST command
        self.send_command("POST")
            .await
            .inspect_err(|_| self.state = ConnectionState::Error)?;
        let status = self
            .read_response_line()
            .await
            .inspect_err(|_| self.state = ConnectionState::Error)?;

        match status.code {
            340 => {
                // Server says "send article"
                // Send each line, dot-stuffing lines that start with '.'
                let transport = self
                    .transport
                    .as_mut()
                    .ok_or(NntpError::Connection("Not connected".into()))?;

                for line in article.lines() {
                    if line.starts_with('.') {
                        transport
                            .write_all(format!(".{line}\r\n").as_bytes())
                            .await
                            .map_err(|e| {
                                self.state = ConnectionState::Error;
                                NntpError::Io(e)
                            })?;
                    } else {
                        transport
                            .write_all(format!("{line}\r\n").as_bytes())
                            .await
                            .map_err(|e| {
                                self.state = ConnectionState::Error;
                                NntpError::Io(e)
                            })?;
                    }
                }

                // Send termination line
                transport.write_all(b".\r\n").await.map_err(|e| {
                    self.state = ConnectionState::Error;
                    NntpError::Io(e)
                })?;

                // Read final response
                let result = self
                    .read_response_line()
                    .await
                    .inspect_err(|_| self.state = ConnectionState::Error)?;
                self.state = ConnectionState::Ready;
                Ok(result)
            }
            440 => {
                self.state = ConnectionState::Ready;
                Ok(status) // Posting not permitted
            }
            _ => {
                self.state = ConnectionState::Error;
                Err(NntpError::Protocol(format!(
                    "Unexpected POST response {}: {}",
                    status.code, status.message
                )))
            }
        }
    }

    /// Post a binary article where the body may contain arbitrary bytes (e.g. yEnc).
    ///
    /// `article` must be a complete NNTP article: RFC 2822 headers followed by
    /// a blank line, then the body, all with CRLF line endings. Lines starting
    /// with `.` are dot-stuffed automatically. The terminating `.\r\n` is added
    /// by this method.
    ///
    /// Returns the server's 240 response on success.
    pub async fn post_article_bytes(&mut self, article: &[u8]) -> NntpResult<NntpResponse> {
        if self.state != ConnectionState::Ready {
            return Err(NntpError::Protocol(format!(
                "Cannot POST in state {:?}",
                self.state
            )));
        }
        self.state = ConnectionState::Busy;

        self.send_command("POST")
            .await
            .inspect_err(|_| self.state = ConnectionState::Error)?;
        let status = self
            .read_response_line()
            .await
            .inspect_err(|_| self.state = ConnectionState::Error)?;

        match status.code {
            340 => {
                let transport = self
                    .transport
                    .as_mut()
                    .ok_or(NntpError::Connection("Not connected".into()))?;

                let mut start = 0usize;
                while start < article.len() {
                    let nl_pos = article[start..]
                        .iter()
                        .position(|&b| b == b'\n')
                        .map(|p| start + p);
                    let line_end = nl_pos.unwrap_or(article.len());
                    let line = article[start..line_end]
                        .strip_suffix(b"\r")
                        .unwrap_or(&article[start..line_end]);

                    if line.starts_with(b".") {
                        transport.write_all(b".").await.map_err(|e| {
                            self.state = ConnectionState::Error;
                            NntpError::Io(e)
                        })?;
                    }
                    transport.write_all(line).await.map_err(|e| {
                        self.state = ConnectionState::Error;
                        NntpError::Io(e)
                    })?;
                    transport.write_all(b"\r\n").await.map_err(|e| {
                        self.state = ConnectionState::Error;
                        NntpError::Io(e)
                    })?;

                    start = match nl_pos {
                        Some(p) => p + 1,
                        None => article.len(),
                    };
                }

                transport.write_all(b".\r\n").await.map_err(|e| {
                    self.state = ConnectionState::Error;
                    NntpError::Io(e)
                })?;

                let result = self
                    .read_response_line()
                    .await
                    .inspect_err(|_| self.state = ConnectionState::Error)?;
                self.state = ConnectionState::Ready;
                Ok(result)
            }
            440 => {
                self.state = ConnectionState::Ready;
                Ok(status)
            }
            _ => {
                self.state = ConnectionState::Error;
                Err(NntpError::Protocol(format!(
                    "Unexpected POST response {}: {}",
                    status.code, status.message
                )))
            }
        }
    }

    // ------------------------------------------------------------------
    // QUIT
    // ------------------------------------------------------------------

    /// Send QUIT and close the connection gracefully.
    pub async fn quit(&mut self) -> NntpResult<()> {
        info!(server = %self.server_id, state = ?self.state, "NNTP disconnecting (QUIT)");
        if self.transport.is_some() {
            // Best-effort: send QUIT, ignore errors
            if let Err(e) = self.send_command("QUIT").await {
                debug!(server = %self.server_id, "QUIT send failed (ignored): {e}");
            } else {
                // Try to read the 205 response
                match self.read_response_line().await {
                    Ok(resp) => {
                        trace!(server = %self.server_id, code = resp.code, "QUIT response");
                    }
                    Err(e) => {
                        debug!(server = %self.server_id, "QUIT response read failed (ignored): {e}");
                    }
                }
            }

            // Shut down the socket
            if let Some(ref mut transport) = self.transport {
                let _ = transport.shutdown().await;
            }
        }

        self.transport = None;
        self.state = ConnectionState::Disconnected;
        Ok(())
    }

    // ------------------------------------------------------------------
    // Send a raw ARTICLE command and read status line only (for pipeline)
    // ------------------------------------------------------------------

    /// Send a raw NNTP command followed by `\r\n`. Does not read the response.
    pub(crate) async fn send_command(&mut self, cmd: &str) -> NntpResult<()> {
        self.send_command_no_flush(cmd).await?;
        self.flush().await
    }

    /// Write a command WITHOUT flushing the TCP buffer.
    /// Used by the pipeline to batch multiple ARTICLE commands before a single flush.
    pub(crate) async fn send_command_no_flush(&mut self, cmd: &str) -> NntpResult<()> {
        let transport = self
            .transport
            .as_mut()
            .ok_or(NntpError::Connection("Not connected".into()))?;

        trace!(server = %self.server_id, cmd = %cmd.split_whitespace().next().unwrap_or(""), ">> NNTP");

        let mut line = cmd.to_string();
        line.push_str("\r\n");
        match transport {
            Transport::Plain(r) => r.get_mut().write_all(line.as_bytes()).await,
            Transport::Tls(r) => r.get_mut().write_all(line.as_bytes()).await,
        }
        .map_err(NntpError::Io)?;
        Ok(())
    }

    /// Flush the TCP write buffer.
    pub(crate) async fn flush(&mut self) -> NntpResult<()> {
        let transport = self
            .transport
            .as_mut()
            .ok_or(NntpError::Connection("Not connected".into()))?;
        match transport {
            Transport::Plain(r) => r.get_mut().flush().await,
            Transport::Tls(r) => r.get_mut().flush().await,
        }
        .map_err(NntpError::Io)?;
        Ok(())
    }

    /// Read a single response line (status code + message). Public for pipeline use.
    pub(crate) async fn read_response_line(&mut self) -> NntpResult<NntpResponse> {
        let transport = self
            .transport
            .as_mut()
            .ok_or(NntpError::Connection("Not connected".into()))?;

        let mut line = String::with_capacity(256);
        let n = tokio::time::timeout(READ_LINE_TIMEOUT, transport.read_line(&mut line))
            .await
            .map_err(|_| {
                warn!(
                    server = %self.server_id,
                    "read_response_line timed out after {}s — connection likely dead",
                    READ_LINE_TIMEOUT.as_secs()
                );
                NntpError::Io(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    format!(
                        "read_response_line timed out after {}s",
                        READ_LINE_TIMEOUT.as_secs()
                    ),
                ))
            })?
            .map_err(NntpError::Io)?;

        if n == 0 {
            return Err(NntpError::Connection("Server closed connection".into()));
        }

        // Successful read → connection is alive. Tick the liveness heartbeat
        // so higher-level watchdogs don't falsely evict slow-but-working
        // workers. Matches SABnzbd's per-byte timeout reset model.
        self.tick_io_heartbeat();

        parse_response_line(&line)
    }

    /// Read a multi-line body terminated by `.\r\n`. Un-does dot-stuffing.
    /// Public for pipeline use.
    pub(crate) async fn read_multiline_body(&mut self) -> NntpResult<Vec<u8>> {
        // Clone the heartbeat ref before we take the mutable borrow on
        // `transport`, so we can tick it inside the loop body. Cheap: just
        // an Arc clone plus an Instant copy.
        let heartbeat = self.io_heartbeat.clone();
        let transport = self
            .transport
            .as_mut()
            .ok_or(NntpError::Connection("Not connected".into()))?;

        let mut body = Vec::with_capacity(1024 * 1024);
        let mut line_buf: Vec<u8> = Vec::with_capacity(16 * 1024);

        loop {
            line_buf.clear();
            let n = tokio::time::timeout(
                READ_BODY_LINE_TIMEOUT,
                transport.read_line_bytes(&mut line_buf),
            )
            .await
            .map_err(|_| {
                warn!(
                    server = %self.server_id,
                    body_bytes = body.len(),
                    "read_multiline_body timed out after {}s — connection likely dead",
                    READ_BODY_LINE_TIMEOUT.as_secs()
                );
                NntpError::Io(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    format!(
                        "read_multiline_body timed out after {}s (received {} bytes so far)",
                        READ_BODY_LINE_TIMEOUT.as_secs(),
                        body.len()
                    ),
                ))
            })?
            .map_err(NntpError::Io)?;

            if n == 0 {
                return Err(NntpError::Connection(
                    "Server closed connection during multi-line read".into(),
                ));
            }

            // Successful read → connection is alive. Tick the liveness
            // heartbeat on every line of a multi-line body so the watchdog
            // sees steady activity during large articles.
            if let Some(hb) = &heartbeat {
                hb.tick();
            }

            // Check for termination: a lone dot followed by CRLF
            if line_buf == b".\r\n" || line_buf == b".\n" {
                break;
            }

            // Dot-unstuffing: if a line starts with "..", remove the first dot
            if line_buf.starts_with(b"..") {
                body.extend_from_slice(&line_buf[1..]);
            } else {
                body.extend_from_slice(&line_buf);
            }
        }

        Ok(body)
    }

    /// Returns `true` if the connection has an active transport.
    pub fn is_connected(&self) -> bool {
        self.transport.is_some() && self.state != ConnectionState::Disconnected
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Parse a single NNTP response line into code + message.
fn parse_response_line(line: &str) -> NntpResult<NntpResponse> {
    let trimmed = line.trim_end_matches(['\r', '\n']);
    if trimmed.len() < 3 {
        return Err(NntpError::Protocol(format!(
            "Response line too short: {trimmed:?}"
        )));
    }

    let code: u16 = trimmed[..3]
        .parse()
        .map_err(|_| NntpError::Protocol(format!("Invalid response code in: {trimmed:?}")))?;

    let message = if trimmed.len() > 4 {
        trimmed[4..].to_string()
    } else {
        String::new()
    };

    Ok(NntpResponse {
        code,
        message,
        data: None,
    })
}

/// Ensure message-id is wrapped in angle brackets.
fn normalize_message_id(mid: &str) -> String {
    if mid.starts_with('<') && mid.ends_with('>') {
        mid.to_string()
    } else {
        format!("<{mid}>")
    }
}

/// Strip the header section from an ARTICLE response body, returning only the
/// body bytes. Used by `fetch_body` when the server lacks BODY capability and
/// we have to fall back to ARTICLE.
///
/// RFC 3977 §6.2.1: an ARTICLE response is `headers \r\n \r\n body`. We split
/// on the first blank line. If no blank line exists (degenerate / header-only
/// article), returns an empty Vec — matching what BODY would have returned.
fn strip_article_headers(article: &[u8]) -> Vec<u8> {
    let mut i = 0;
    while i + 1 < article.len() {
        // Match either CRLF CRLF or LF LF as the header/body separator.
        if article[i] == b'\r' && article[i + 1] == b'\n' {
            if i + 3 < article.len() && article[i + 2] == b'\r' && article[i + 3] == b'\n' {
                return article[i + 4..].to_vec();
            }
        } else if article[i] == b'\n' && article[i + 1] == b'\n' {
            return article[i + 2..].to_vec();
        }
        i += 1;
    }
    Vec::new()
}

/// Parse XOVER multi-line body into structured entries.
/// Each line is tab-delimited:
/// article_num \t subject \t from \t date \t message-id \t references \t bytes \t lines
fn parse_xover_data(data: &[u8]) -> Vec<XoverEntry> {
    let text = String::from_utf8_lossy(data);
    let mut entries = Vec::new();

    for line in text.lines() {
        let parts: Vec<&str> = line.split('\t').collect();
        if parts.len() < 8 {
            continue; // Malformed line, skip
        }
        let message_id = parts[4].trim_matches(|c| c == '<' || c == '>').to_string();
        entries.push(XoverEntry {
            article_num: parts[0].parse().unwrap_or(0),
            subject: parts[1].to_string(),
            from: parts[2].to_string(),
            date: parts[3].to_string(),
            message_id,
            references: parts[5].to_string(),
            bytes: parts[6].parse().unwrap_or(0),
            lines: parts[7].trim().parse().unwrap_or(0),
        });
    }

    entries
}

/// Parse XHDR/XPAT multi-line body into structured entries.
///
/// Each line format: `article_num value` (space or tab separated).
/// The first token is the article number; everything after the first
/// whitespace is the header value.
fn parse_header_data(data: &[u8]) -> Vec<HeaderEntry> {
    let text = String::from_utf8_lossy(data);
    let mut entries = Vec::new();

    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        // Split at first whitespace: "article_num rest_of_value"
        if let Some(pos) = trimmed.find([' ', '\t']) {
            let num_str = &trimmed[..pos];
            let value = trimmed[pos..].trim_start().to_string();
            if let Ok(article_num) = num_str.parse::<u64>() {
                entries.push(HeaderEntry { article_num, value });
            }
        }
    }

    entries
}

/// Parse LIST ACTIVE multi-line body into structured entries.
///
/// Each line format: `groupname last first posting_flag`
/// Fields are whitespace-separated.
fn parse_list_active_data(data: &[u8]) -> Vec<ListActiveEntry> {
    let text = String::from_utf8_lossy(data);
    let mut entries = Vec::new();

    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let parts: Vec<&str> = trimmed.split_whitespace().collect();
        if parts.len() < 4 {
            continue;
        }
        entries.push(ListActiveEntry {
            name: parts[0].to_string(),
            high: parts[1].parse().unwrap_or(0),
            low: parts[2].parse().unwrap_or(0),
            status: parts[3].to_string(),
        });
    }

    entries
}

/// Parsed SOCKS5 proxy URL components.
#[derive(Debug)]
struct Socks5Proxy {
    addr: String,
    auth: Option<(String, String)>,
}

/// Parse a SOCKS5 proxy URL: `socks5://[username:password@]host:port`
fn parse_socks5_url(url: &str) -> Result<Socks5Proxy, String> {
    let rest = url
        .strip_prefix("socks5://")
        .ok_or_else(|| format!("proxy URL must start with socks5://, got: {url}"))?;

    let (auth, host_port) = if let Some(at_pos) = rest.rfind('@') {
        let auth_part = &rest[..at_pos];
        let host_part = &rest[at_pos + 1..];
        let (user, pass) = auth_part
            .split_once(':')
            .ok_or_else(|| "proxy auth must be username:password".to_string())?;
        (
            Some((user.to_string(), pass.to_string())),
            host_part.to_string(),
        )
    } else {
        (None, rest.to_string())
    };

    if host_port.is_empty() {
        return Err("proxy URL must contain host:port".to_string());
    }

    Ok(Socks5Proxy {
        addr: host_port,
        auth,
    })
}

/// Build a `rustls::ClientConfig` for NNTP TLS connections.
///
/// Three verification modes, in priority order:
///   1. `trusted_fingerprint` set → match server cert SHA-256 only
///      (hostname / CA chain ignored). For pinning self-signed certs.
///   2. `verify_certs=true`        → full WebPKI validation against built-in
///      trust roots (standard mode for public servers).
///   3. `verify_certs=false`       → accept any cert (insecure; dev/test).
///
/// Uses the `ring` crypto provider explicitly so callers don't need to install
/// a process-level default via `CryptoProvider::install_default()`.
fn build_tls_config(
    verify_certs: bool,
    trusted_fingerprint: Option<&str>,
) -> NntpResult<rustls::ClientConfig> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());

    if let Some(fp_hex) = trusted_fingerprint {
        let expected = parse_fingerprint(fp_hex).ok_or_else(|| {
            NntpError::Connection(format!("invalid trusted_fingerprint: {fp_hex}"))
        })?;
        let verifier = Arc::new(FingerprintVerifier { expected });
        let config = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .map_err(|e| NntpError::Connection(format!("TLS config error: {e}")))?
            .dangerous()
            .with_custom_certificate_verifier(verifier)
            .with_no_client_auth();
        return Ok(config);
    }

    if verify_certs {
        let mut root_store = rustls::RootCertStore::empty();
        root_store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());

        let config = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .map_err(|e| NntpError::Connection(format!("TLS config error: {e}")))?
            .with_root_certificates(root_store)
            .with_no_client_auth();
        Ok(config)
    } else {
        let config = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .map_err(|e| NntpError::Connection(format!("TLS config error: {e}")))?
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(NoVerifier))
            .with_no_client_auth();
        Ok(config)
    }
}

/// Parse a hex SHA-256 fingerprint (optional colons, any case) into 32 bytes.
fn parse_fingerprint(s: &str) -> Option<[u8; 32]> {
    let cleaned: String = s
        .chars()
        .filter(|c| !c.is_whitespace() && *c != ':')
        .collect();
    if cleaned.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, chunk) in cleaned.as_bytes().chunks(2).enumerate() {
        let hex = std::str::from_utf8(chunk).ok()?;
        out[i] = u8::from_str_radix(hex, 16).ok()?;
    }
    Some(out)
}

/// Accepts the server cert iff SHA-256(DER) == `expected`. Hostname,
/// expiry, and CA chain are not validated.
#[derive(Debug)]
struct FingerprintVerifier {
    expected: [u8; 32],
}

impl rustls::client::danger::ServerCertVerifier for FingerprintVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &rustls_pki_types::CertificateDer<'_>,
        _intermediates: &[rustls_pki_types::CertificateDer<'_>],
        _server_name: &rustls_pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls_pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        use sha2::{Digest, Sha256};
        let got = Sha256::digest(end_entity.as_ref());
        if got.as_slice() == self.expected.as_slice() {
            Ok(rustls::client::danger::ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::General(format!(
                "server cert fingerprint mismatch (expected {}, got {})",
                hex_encode_short(&self.expected),
                hex_encode_short(&got),
            )))
        }
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &rustls_pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &rustls_pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

fn hex_encode_short(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let take = bytes.len().min(8);
    let mut s = String::with_capacity(take * 2);
    for b in &bytes[..take] {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0x0f) as usize] as char);
    }
    s
}

/// A certificate verifier that accepts any certificate (for `ssl_verify: false`).
#[derive(Debug)]
struct NoVerifier;

impl rustls::client::danger::ServerCertVerifier for NoVerifier {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls_pki_types::CertificateDer<'_>,
        _intermediates: &[rustls_pki_types::CertificateDer<'_>],
        _server_name: &rustls_pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls_pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &rustls_pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &rustls_pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{MockConfig, MockNntpServer, test_config, test_config_with_auth};
    use std::collections::HashMap;
    use std::sync::Arc;

    // -----------------------------------------------------------------------
    // Pure helper function tests (existing)
    // -----------------------------------------------------------------------

    #[test]
    fn test_parse_response_line() {
        let resp = parse_response_line("200 NNTP Service Ready\r\n").unwrap();
        assert_eq!(resp.code, 200);
        assert_eq!(resp.message, "NNTP Service Ready");
    }

    #[test]
    fn parse_fingerprint_accepts_various_formats() {
        let hex = "40f9310af55480b2d2f1d6e253bb557c7ab5cbdb2f2a417903aada0a131ad9c0";
        let upper = hex.to_uppercase();
        let colons = "40:f9:31:0a:f5:54:80:b2:d2:f1:d6:e2:53:bb:55:7c:\
                      7a:b5:cb:db:2f:2a:41:79:03:aa:da:0a:13:1a:d9:c0";
        let spaced = "40 f9 31 0a f5 54 80 b2 d2 f1 d6 e2 53 bb 55 7c 7a b5 cb db 2f 2a 41 79 03 aa da 0a 13 1a d9 c0";
        let expected = parse_fingerprint(hex).unwrap();
        assert_eq!(parse_fingerprint(&upper).unwrap(), expected);
        assert_eq!(parse_fingerprint(colons).unwrap(), expected);
        assert_eq!(parse_fingerprint(spaced).unwrap(), expected);
        assert!(parse_fingerprint("too-short").is_none());
        assert!(parse_fingerprint(&format!("{hex}extra")).is_none());
        assert!(parse_fingerprint("zz".repeat(32).as_str()).is_none());
    }

    #[test]
    fn fingerprint_mode_overrides_verify_certs_flag() {
        let fp = "40f9310af55480b2d2f1d6e253bb557c7ab5cbdb2f2a417903aada0a131ad9c0";
        // Should succeed regardless of verify_certs — fingerprint takes priority.
        let _cfg_a = build_tls_config(true, Some(fp)).expect("fingerprint+verify");
        let _cfg_b = build_tls_config(false, Some(fp)).expect("fingerprint+noverify");
        // Malformed fingerprint → error.
        assert!(build_tls_config(true, Some("not-hex")).is_err());
    }

    #[test]
    fn test_parse_response_line_no_message() {
        let resp = parse_response_line("200\r\n").unwrap();
        assert_eq!(resp.code, 200);
        assert_eq!(resp.message, "");
    }

    #[test]
    fn test_parse_response_line_too_short() {
        let err = parse_response_line("20\r\n");
        assert!(err.is_err());
    }

    #[test]
    fn test_parse_response_line_invalid_code() {
        let err = parse_response_line("ABC some message\r\n");
        assert!(err.is_err());
    }

    #[test]
    fn test_normalize_message_id() {
        assert_eq!(normalize_message_id("abc@example.com"), "<abc@example.com>");
        assert_eq!(
            normalize_message_id("<abc@example.com>"),
            "<abc@example.com>"
        );
    }

    #[test]
    fn test_parse_xover_data() {
        let data = b"123456\tSubject line\tposter@example.com\tMon, 01 Jan 2024 00:00:00 UTC\t<msg-id@host>\t\t768000\t1000\r\n";
        let entries = parse_xover_data(data);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].article_num, 123456);
        assert_eq!(entries[0].subject, "Subject line");
        assert_eq!(entries[0].from, "poster@example.com");
        assert_eq!(entries[0].message_id, "msg-id@host");
        assert_eq!(entries[0].bytes, 768000);
        assert_eq!(entries[0].lines, 1000);
    }

    #[test]
    fn test_parse_xover_strips_angle_brackets() {
        let data = b"1\tSubj\tPoster\tDate\t<abc@def.com>\t\t100\t10\r\n";
        let entries = parse_xover_data(data);
        assert_eq!(entries[0].message_id, "abc@def.com");
    }

    #[test]
    fn test_parse_xover_skips_malformed_lines() {
        let data = b"too\tfew\tfields\r\n123\tSubj\tFrom\tDate\t<mid@x>\t\t500\t50\r\n";
        let entries = parse_xover_data(data);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].article_num, 123);
    }

    #[test]
    fn test_parse_xover_multiple_entries() {
        let data =
            b"100\tS1\tF1\tD1\t<m1@x>\t\t1000\t10\r\n200\tS2\tF2\tD2\t<m2@x>\tref\t2000\t20\r\n";
        let entries = parse_xover_data(data);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].article_num, 100);
        assert_eq!(entries[1].article_num, 200);
        assert_eq!(entries[1].references, "ref");
    }

    #[test]
    fn test_parse_xover_empty() {
        let entries = parse_xover_data(b"");
        assert!(entries.is_empty());
    }

    // -----------------------------------------------------------------------
    // NntpResponse helper tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_response_is_success() {
        assert!(
            NntpResponse {
                code: 200,
                message: "OK".into(),
                data: None
            }
            .is_success()
        );
        assert!(
            NntpResponse {
                code: 220,
                message: "OK".into(),
                data: None
            }
            .is_success()
        );
        assert!(
            NntpResponse {
                code: 281,
                message: "OK".into(),
                data: None
            }
            .is_success()
        );
        assert!(
            !NntpResponse {
                code: 430,
                message: "Not found".into(),
                data: None
            }
            .is_success()
        );
        assert!(
            !NntpResponse {
                code: 502,
                message: "Err".into(),
                data: None
            }
            .is_success()
        );
    }

    #[test]
    fn test_response_needs_auth() {
        assert!(
            NntpResponse {
                code: 480,
                message: "Auth".into(),
                data: None
            }
            .needs_auth()
        );
        assert!(
            !NntpResponse {
                code: 200,
                message: "OK".into(),
                data: None
            }
            .needs_auth()
        );
    }

    // -----------------------------------------------------------------------
    // NntpConnection unit tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_new_connection_state() {
        let conn = NntpConnection::new("test-1".into());
        assert_eq!(conn.server_id, "test-1");
        assert_eq!(conn.state, ConnectionState::Disconnected);
        assert!(!conn.is_connected());
    }

    // -----------------------------------------------------------------------
    // Mock server integration tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_connect_plain() {
        let server = MockNntpServer::start(MockConfig::default()).await;
        let config = test_config(server.port());
        let mut conn = NntpConnection::new("test".into());

        conn.connect(&config).await.unwrap();
        assert_eq!(conn.state, ConnectionState::Ready);
        assert!(conn.is_connected());
    }

    #[tokio::test]
    async fn test_connect_read_only_server() {
        let server = MockNntpServer::start(MockConfig {
            welcome_code: 201,
            welcome_message: "Read-only".into(),
            ..MockConfig::default()
        })
        .await;
        let config = test_config(server.port());
        let mut conn = NntpConnection::new("test".into());

        conn.connect(&config).await.unwrap();
        assert_eq!(conn.state, ConnectionState::Ready);
    }

    #[tokio::test]
    async fn test_connect_with_auth() {
        let server = MockNntpServer::start(MockConfig {
            auth_required: true,
            valid_credentials: Some(("myuser".into(), "mypass".into())),
            ..MockConfig::default()
        })
        .await;
        let config = test_config_with_auth(server.port(), "myuser", "mypass");
        let mut conn = NntpConnection::new("test".into());

        conn.connect(&config).await.unwrap();
        assert_eq!(conn.state, ConnectionState::Ready);
    }

    #[tokio::test]
    async fn test_connect_auth_wrong_password() {
        let server = MockNntpServer::start(MockConfig {
            auth_required: true,
            valid_credentials: Some(("myuser".into(), "correct".into())),
            ..MockConfig::default()
        })
        .await;
        let config = test_config_with_auth(server.port(), "myuser", "wrong");
        let mut conn = NntpConnection::new("test".into());

        let result = conn.connect(&config).await;
        assert!(result.is_err());
        assert_eq!(conn.state, ConnectionState::Error);
    }

    #[tokio::test]
    async fn test_connect_auth_rejected() {
        let server = MockNntpServer::start(MockConfig {
            auth_required: true,
            fail_auth: true,
            ..MockConfig::default()
        })
        .await;
        let config = test_config_with_auth(server.port(), "user", "pass");
        let mut conn = NntpConnection::new("test".into());

        let result = conn.connect(&config).await;
        assert!(result.is_err());
        assert_eq!(conn.state, ConnectionState::Error);
    }

    #[tokio::test]
    async fn test_connect_service_unavailable() {
        let server = MockNntpServer::start(MockConfig {
            service_unavailable: true,
            ..MockConfig::default()
        })
        .await;
        let config = test_config(server.port());
        let mut conn = NntpConnection::new("test".into());

        let result = conn.connect(&config).await;
        assert!(result.is_err());
        assert_eq!(conn.state, ConnectionState::Error);
    }

    #[tokio::test]
    async fn test_connect_refused() {
        // Connect to a port with nothing listening
        let config = test_config(19999);
        let mut conn = NntpConnection::new("test".into());

        let result = conn.connect(&config).await;
        assert!(result.is_err());
        assert_eq!(conn.state, ConnectionState::Error);
    }

    #[tokio::test]
    async fn test_group_success() {
        let mut groups = HashMap::new();
        groups.insert("alt.binaries.test".into(), (5000u64, 1u64, 5000u64));

        let server = MockNntpServer::start(MockConfig {
            groups,
            ..MockConfig::default()
        })
        .await;
        let config = test_config(server.port());
        let mut conn = NntpConnection::new("test".into());
        conn.connect(&config).await.unwrap();

        let group = conn.group("alt.binaries.test").await.unwrap();
        assert_eq!(group.count, 5000);
        assert_eq!(group.first, 1);
        assert_eq!(group.last, 5000);
        assert_eq!(group.name, "alt.binaries.test");
        assert_eq!(conn.state, ConnectionState::Ready);
    }

    #[tokio::test]
    async fn test_group_not_found() {
        let server = MockNntpServer::start(MockConfig::default()).await;
        let config = test_config(server.port());
        let mut conn = NntpConnection::new("test".into());
        conn.connect(&config).await.unwrap();

        let result = conn.group("nonexistent.group").await;
        assert!(matches!(
            result,
            Err(crate::error::NntpError::NoSuchGroup(_))
        ));
        assert_eq!(conn.state, ConnectionState::Ready);
    }

    #[tokio::test]
    async fn test_xover_success() {
        let mut groups = HashMap::new();
        groups.insert("alt.binaries.test".into(), (100u64, 1u64, 100u64));

        let xover_entries = vec![
            "1\tTest Subject 1\tposter@test.com\tMon, 01 Jan 2024\t<art1@test>\t\t50000\t100"
                .into(),
            "2\tTest Subject 2\tposter@test.com\tMon, 01 Jan 2024\t<art2@test>\t\t60000\t120"
                .into(),
        ];

        let server = MockNntpServer::start(MockConfig {
            groups,
            xover_entries,
            ..MockConfig::default()
        })
        .await;
        let config = test_config(server.port());
        let mut conn = NntpConnection::new("test".into());
        conn.connect(&config).await.unwrap();

        conn.group("alt.binaries.test").await.unwrap();
        let entries = conn.xover(1, 100).await.unwrap();

        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].article_num, 1);
        assert_eq!(entries[0].subject, "Test Subject 1");
        assert_eq!(entries[0].message_id, "art1@test");
        assert_eq!(entries[0].bytes, 50000);
        assert_eq!(entries[1].article_num, 2);
        assert_eq!(entries[1].bytes, 60000);
        assert_eq!(conn.state, ConnectionState::Ready);
    }

    #[tokio::test]
    async fn test_xover_empty_range() {
        let mut groups = HashMap::new();
        groups.insert("alt.binaries.test".into(), (100u64, 1u64, 100u64));

        let server = MockNntpServer::start(MockConfig {
            groups,
            xover_entries: Vec::new(),
            ..MockConfig::default()
        })
        .await;
        let config = test_config(server.port());
        let mut conn = NntpConnection::new("test".into());
        conn.connect(&config).await.unwrap();

        conn.group("alt.binaries.test").await.unwrap();
        let entries = conn.xover(1, 100).await.unwrap();
        assert!(entries.is_empty());
        assert_eq!(conn.state, ConnectionState::Ready);
    }

    #[tokio::test]
    async fn test_xover_no_group_selected() {
        let server = MockNntpServer::start(MockConfig::default()).await;
        let config = test_config(server.port());
        let mut conn = NntpConnection::new("test".into());
        conn.connect(&config).await.unwrap();

        let result = conn.xover(1, 100).await;
        assert!(matches!(
            result,
            Err(crate::error::NntpError::NoSuchGroup(_))
        ));
    }

    #[tokio::test]
    async fn test_fetch_article_success() {
        let mut articles = HashMap::new();
        articles.insert("art1@test".into(), b"This is article body data".to_vec());

        let server = MockNntpServer::start(MockConfig {
            articles,
            ..MockConfig::default()
        })
        .await;
        let config = test_config(server.port());
        let mut conn = NntpConnection::new("test".into());
        conn.connect(&config).await.unwrap();

        let response = conn.fetch_article("art1@test").await.unwrap();
        assert_eq!(response.code, 220);
        let data = response.data.unwrap();
        let body = String::from_utf8_lossy(&data);
        assert!(body.contains("This is article body data"));
        assert_eq!(conn.state, ConnectionState::Ready);
    }

    #[tokio::test]
    async fn test_fetch_article_not_found() {
        let server = MockNntpServer::start(MockConfig::default()).await;
        let config = test_config(server.port());
        let mut conn = NntpConnection::new("test".into());
        conn.connect(&config).await.unwrap();

        let result = conn.fetch_article("nonexistent@test").await;
        assert!(matches!(
            result,
            Err(crate::error::NntpError::ArticleNotFound(_))
        ));
        assert_eq!(conn.state, ConnectionState::Ready);
    }

    #[tokio::test]
    async fn test_fetch_body_success() {
        let mut articles = HashMap::new();
        articles.insert("body1@test".into(), b"Body content here".to_vec());

        let server = MockNntpServer::start(MockConfig {
            articles,
            ..MockConfig::default()
        })
        .await;
        let config = test_config(server.port());
        let mut conn = NntpConnection::new("test".into());
        conn.connect(&config).await.unwrap();

        let response = conn.fetch_body("body1@test").await.unwrap();
        assert_eq!(response.code, 222);
        let data = response.data.unwrap();
        let body = String::from_utf8_lossy(&data);
        assert!(body.contains("Body content here"));
        assert_eq!(conn.state, ConnectionState::Ready);
    }

    #[tokio::test]
    async fn test_io_heartbeat_ticks_on_every_read() {
        // Verify the socket-liveness heartbeat advances on each NNTP response
        // line — this is the signal nzb-web's idle watchdog uses to distinguish
        // slow-but-alive connections from zombied ones. Without this, workers
        // fetching a single slow article would appear dead for the full fetch
        // duration and get false-evicted (the exact production bug this fixes).
        let mut articles = HashMap::new();
        articles.insert("hb@test".into(), b"x".repeat(16_384)); // multi-line body

        let server = MockNntpServer::start(MockConfig {
            articles,
            ..MockConfig::default()
        })
        .await;
        let config = test_config(server.port());

        let epoch = Instant::now();
        let heartbeat = Arc::new(AtomicU64::new(0));

        // Ensure epoch.elapsed().as_millis() is > 0 by the time the first
        // read ticks the heartbeat — otherwise a localhost connect faster
        // than 1ms would tick `0` and we couldn't distinguish from unset.
        tokio::time::sleep(Duration::from_millis(5)).await;

        let mut conn = NntpConnection::new("test".into());
        conn.set_io_heartbeat(heartbeat.clone(), epoch);
        conn.connect(&config).await.unwrap();

        // The connect sequence (welcome + any auth responses) must have
        // advanced the heartbeat at least once.
        let after_connect = heartbeat.load(Ordering::Relaxed);
        assert!(
            after_connect > 0,
            "heartbeat should tick during connect (welcome banner read); got {after_connect}"
        );

        // Briefly sleep so elapsed millis advance between reads — otherwise
        // a fast mock reply could tick within the same ms.
        tokio::time::sleep(Duration::from_millis(5)).await;

        conn.fetch_body("hb@test").await.unwrap();

        let after_fetch = heartbeat.load(Ordering::Relaxed);
        assert!(
            after_fetch > after_connect,
            "heartbeat should advance during multi-line body read \
             (before={after_connect}ms, after={after_fetch}ms)"
        );
    }

    #[tokio::test]
    async fn test_io_heartbeat_optional_noop_when_unset() {
        // Connections without a heartbeat installed must still work identically
        // to pre-patch behaviour. Guards against regression for consumers that
        // don't opt into the liveness signal.
        let mut articles = HashMap::new();
        articles.insert("noop@test".into(), b"payload".to_vec());

        let server = MockNntpServer::start(MockConfig {
            articles,
            ..MockConfig::default()
        })
        .await;
        let config = test_config(server.port());
        let mut conn = NntpConnection::new("test".into());
        // No set_io_heartbeat call.
        conn.connect(&config).await.unwrap();
        let response = conn.fetch_body("noop@test").await.unwrap();
        assert_eq!(response.code, 222);
    }

    #[tokio::test]
    async fn test_fetch_body_not_found() {
        let server = MockNntpServer::start(MockConfig::default()).await;
        let config = test_config(server.port());
        let mut conn = NntpConnection::new("test".into());
        conn.connect(&config).await.unwrap();

        let result = conn.fetch_body("missing@test").await;
        assert!(matches!(
            result,
            Err(crate::error::NntpError::ArticleNotFound(_))
        ));
        assert_eq!(conn.state, ConnectionState::Ready);
    }

    #[tokio::test]
    async fn test_post_article_bytes_success_preserves_binary_and_dot_lines() {
        let posted_articles = Arc::new(parking_lot::Mutex::new(Vec::<Vec<u8>>::new()));
        let server = MockNntpServer::start(MockConfig {
            posted_articles: Some(posted_articles.clone()),
            ..MockConfig::default()
        })
        .await;
        let config = test_config(server.port());
        let mut conn = NntpConnection::new("test".into());
        conn.connect(&config).await.unwrap();

        let article = [
            b"From: test@example.com\r\n".as_slice(),
            b"Newsgroups: alt.binaries.test\r\n",
            b"Subject: binary-post\r\n",
            b"\r\n",
            b"plain-line\r\n",
            b".leading-dot\r\n",
            b"..double-dot\r\n",
            &[0xff, 0x00, 0x80, b'\r', b'\n'],
        ]
        .concat();

        let response = conn.post_article_bytes(&article).await.unwrap();
        assert_eq!(response.code, 240);
        assert_eq!(conn.state, ConnectionState::Ready);

        let captured = posted_articles.lock();
        assert_eq!(captured.len(), 1);
        assert_eq!(captured[0], article);
    }

    #[tokio::test]
    async fn test_post_article_bytes_not_permitted() {
        let server = MockNntpServer::start(MockConfig {
            post_not_permitted: true,
            ..MockConfig::default()
        })
        .await;
        let config = test_config(server.port());
        let mut conn = NntpConnection::new("test".into());
        conn.connect(&config).await.unwrap();

        let response = conn
            .post_article_bytes(b"From: a\r\n\r\nbody\r\n")
            .await
            .unwrap();
        assert_eq!(response.code, 440);
        assert_eq!(conn.state, ConnectionState::Ready);
    }

    #[tokio::test]
    async fn test_post_article_bytes_wrong_state() {
        let mut conn = NntpConnection::new("test".into());
        let result = conn.post_article_bytes(b"From: a\r\n\r\nbody\r\n").await;
        assert!(matches!(result, Err(crate::error::NntpError::Protocol(_))));
    }

    #[tokio::test]
    async fn test_stat_article_exists() {
        let mut articles = HashMap::new();
        articles.insert("stat1@test".into(), b"data".to_vec());

        let server = MockNntpServer::start(MockConfig {
            articles,
            ..MockConfig::default()
        })
        .await;
        let config = test_config(server.port());
        let mut conn = NntpConnection::new("test".into());
        conn.connect(&config).await.unwrap();

        let response = conn.stat_article("stat1@test").await.unwrap();
        assert_eq!(response.code, 223);
        assert_eq!(conn.state, ConnectionState::Ready);
    }

    #[tokio::test]
    async fn test_stat_article_not_found() {
        let server = MockNntpServer::start(MockConfig::default()).await;
        let config = test_config(server.port());
        let mut conn = NntpConnection::new("test".into());
        conn.connect(&config).await.unwrap();

        let result = conn.stat_article("missing@test").await;
        assert!(matches!(
            result,
            Err(crate::error::NntpError::ArticleNotFound(_))
        ));
        assert_eq!(conn.state, ConnectionState::Ready);
    }

    #[tokio::test]
    async fn test_quit_graceful() {
        let server = MockNntpServer::start(MockConfig::default()).await;
        let config = test_config(server.port());
        let mut conn = NntpConnection::new("test".into());
        conn.connect(&config).await.unwrap();
        assert!(conn.is_connected());

        conn.quit().await.unwrap();
        assert_eq!(conn.state, ConnectionState::Disconnected);
        assert!(!conn.is_connected());
    }

    #[tokio::test]
    async fn test_quit_when_not_connected() {
        let mut conn = NntpConnection::new("test".into());
        // Should not error even when not connected
        conn.quit().await.unwrap();
        assert_eq!(conn.state, ConnectionState::Disconnected);
    }

    #[tokio::test]
    async fn test_command_in_wrong_state() {
        let mut conn = NntpConnection::new("test".into());
        // All commands should fail when disconnected (no transport)
        let result = conn.fetch_article("test@msg").await;
        assert!(matches!(result, Err(crate::error::NntpError::Protocol(_))));

        let result = conn.fetch_body("test@msg").await;
        assert!(matches!(result, Err(crate::error::NntpError::Protocol(_))));

        let result = conn.stat_article("test@msg").await;
        assert!(matches!(result, Err(crate::error::NntpError::Protocol(_))));

        let result = conn.group("test.group").await;
        assert!(matches!(result, Err(crate::error::NntpError::Protocol(_))));

        let result = conn.xover(1, 10).await;
        assert!(matches!(result, Err(crate::error::NntpError::Protocol(_))));

        let result = conn.xhdr("subject", ArticleRange::Range(1, 10)).await;
        assert!(matches!(result, Err(crate::error::NntpError::Protocol(_))));

        let result = conn
            .xpat("subject", ArticleRange::Range(1, 10), &["*test*"])
            .await;
        assert!(matches!(result, Err(crate::error::NntpError::Protocol(_))));
    }

    #[tokio::test]
    async fn test_multiple_commands_sequentially() {
        let mut articles = HashMap::new();
        articles.insert("a1@test".into(), b"data1".to_vec());
        articles.insert("a2@test".into(), b"data2".to_vec());

        let mut groups = HashMap::new();
        groups.insert("alt.test".into(), (100u64, 1u64, 100u64));

        let server = MockNntpServer::start(MockConfig {
            articles,
            groups,
            ..MockConfig::default()
        })
        .await;
        let config = test_config(server.port());
        let mut conn = NntpConnection::new("test".into());
        conn.connect(&config).await.unwrap();

        // GROUP
        let group = conn.group("alt.test").await.unwrap();
        assert_eq!(group.count, 100);

        // STAT
        let stat = conn.stat_article("a1@test").await.unwrap();
        assert_eq!(stat.code, 223);

        // ARTICLE
        let art = conn.fetch_article("a1@test").await.unwrap();
        assert_eq!(art.code, 220);

        // BODY
        let body = conn.fetch_body("a2@test").await.unwrap();
        assert_eq!(body.code, 222);

        // STAT not found
        let result = conn.stat_article("missing@test").await;
        assert!(result.is_err());

        // Connection should still be ready after non-fatal error
        assert_eq!(conn.state, ConnectionState::Ready);

        // QUIT
        conn.quit().await.unwrap();
        assert_eq!(conn.state, ConnectionState::Disconnected);
    }

    // -----------------------------------------------------------------------
    // parse_header_data tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_parse_header_data_basic() {
        let data = b"123 Some Subject Line\r\n456 Another Subject\r\n";
        let entries = parse_header_data(data);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].article_num, 123);
        assert_eq!(entries[0].value, "Some Subject Line");
        assert_eq!(entries[1].article_num, 456);
        assert_eq!(entries[1].value, "Another Subject");
    }

    #[test]
    fn test_parse_header_data_tab_separated() {
        let data = b"789\tSubject with tabs\r\n";
        let entries = parse_header_data(data);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].article_num, 789);
        assert_eq!(entries[0].value, "Subject with tabs");
    }

    #[test]
    fn test_parse_header_data_value_with_spaces() {
        let data = b"100 The Quick Brown Fox Jumps Over\r\n";
        let entries = parse_header_data(data);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].value, "The Quick Brown Fox Jumps Over");
    }

    #[test]
    fn test_parse_header_data_skips_malformed() {
        let data = b"notanumber Some value\r\n200 Valid entry\r\n\r\n";
        let entries = parse_header_data(data);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].article_num, 200);
    }

    #[test]
    fn test_parse_header_data_empty() {
        let entries = parse_header_data(b"");
        assert!(entries.is_empty());
    }

    // -----------------------------------------------------------------------
    // ArticleRange tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_article_range_format() {
        let r = ArticleRange::Range(1, 100);
        assert_eq!(r.to_command_arg(), "1-100");

        let m = ArticleRange::MessageId("abc@test.com".into());
        assert_eq!(m.to_command_arg(), "<abc@test.com>");

        let m2 = ArticleRange::MessageId("<already@wrapped>".into());
        assert_eq!(m2.to_command_arg(), "<already@wrapped>");
    }

    // -----------------------------------------------------------------------
    // XHDR mock server tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_xhdr_success() {
        let mut groups = HashMap::new();
        groups.insert("alt.binaries.test".into(), (100u64, 1u64, 100u64));

        let xhdr_entries = vec![
            "1 Agent Zeta S01E01 720p".into(),
            "2 Agent Zeta S01E02 1080p".into(),
            "3 Breaking Bad S05E16".into(),
        ];

        let server = MockNntpServer::start(MockConfig {
            groups,
            xhdr_entries,
            ..MockConfig::default()
        })
        .await;
        let config = test_config(server.port());
        let mut conn = NntpConnection::new("test".into());
        conn.connect(&config).await.unwrap();

        conn.group("alt.binaries.test").await.unwrap();
        let entries = conn
            .xhdr("subject", ArticleRange::Range(1, 100))
            .await
            .unwrap();

        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].article_num, 1);
        assert_eq!(entries[0].value, "Agent Zeta S01E01 720p");
        assert_eq!(entries[2].article_num, 3);
        assert_eq!(entries[2].value, "Breaking Bad S05E16");
        assert_eq!(conn.state, ConnectionState::Ready);
    }

    #[tokio::test]
    async fn test_xhdr_empty_range() {
        let mut groups = HashMap::new();
        groups.insert("alt.binaries.test".into(), (100u64, 1u64, 100u64));

        let server = MockNntpServer::start(MockConfig {
            groups,
            xhdr_entries: Vec::new(),
            ..MockConfig::default()
        })
        .await;
        let config = test_config(server.port());
        let mut conn = NntpConnection::new("test".into());
        conn.connect(&config).await.unwrap();

        conn.group("alt.binaries.test").await.unwrap();
        let entries = conn
            .xhdr("subject", ArticleRange::Range(1, 100))
            .await
            .unwrap();
        assert!(entries.is_empty());
        assert_eq!(conn.state, ConnectionState::Ready);
    }

    #[tokio::test]
    async fn test_xhdr_no_group_selected() {
        // Mock returns 420 (no articles) when no entries configured,
        // which maps to an empty result rather than an error.
        let server = MockNntpServer::start(MockConfig::default()).await;
        let config = test_config(server.port());
        let mut conn = NntpConnection::new("test".into());
        conn.connect(&config).await.unwrap();

        let result = conn
            .xhdr("subject", ArticleRange::Range(1, 100))
            .await
            .unwrap();
        assert!(result.is_empty());
    }

    // -----------------------------------------------------------------------
    // XPAT mock server tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_xpat_success() {
        let mut groups = HashMap::new();
        groups.insert("alt.binaries.test".into(), (100000u64, 1u64, 100000u64));

        let xpat_entries = vec![
            "500 Agent Zeta S01E01 720p WEB-DL".into(),
            "12345 Agent Zeta S01E02 1080p BluRay".into(),
        ];

        let server = MockNntpServer::start(MockConfig {
            groups,
            xpat_entries,
            ..MockConfig::default()
        })
        .await;
        let config = test_config(server.port());
        let mut conn = NntpConnection::new("test".into());
        conn.connect(&config).await.unwrap();

        conn.group("alt.binaries.test").await.unwrap();
        let entries = conn
            .xpat(
                "subject",
                ArticleRange::Range(1, 99999999),
                &["*Agent Zeta*"],
            )
            .await
            .unwrap();

        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].article_num, 500);
        assert!(entries[0].value.contains("Agent Zeta"));
        assert_eq!(entries[1].article_num, 12345);
        assert_eq!(conn.state, ConnectionState::Ready);
    }

    #[tokio::test]
    async fn test_xpat_no_matches() {
        let mut groups = HashMap::new();
        groups.insert("alt.binaries.test".into(), (100u64, 1u64, 100u64));

        let server = MockNntpServer::start(MockConfig {
            groups,
            xpat_entries: Vec::new(),
            ..MockConfig::default()
        })
        .await;
        let config = test_config(server.port());
        let mut conn = NntpConnection::new("test".into());
        conn.connect(&config).await.unwrap();

        conn.group("alt.binaries.test").await.unwrap();
        let entries = conn
            .xpat("subject", ArticleRange::Range(1, 100), &["*NonExistent*"])
            .await
            .unwrap();
        assert!(entries.is_empty());
        assert_eq!(conn.state, ConnectionState::Ready);
    }

    #[tokio::test]
    async fn test_xpat_multiple_patterns() {
        let mut groups = HashMap::new();
        groups.insert("alt.binaries.test".into(), (100u64, 1u64, 100u64));

        let xpat_entries = vec![
            "10 Agent Zeta S01E01".into(),
            "20 Breaking Bad S05E16".into(),
        ];

        let server = MockNntpServer::start(MockConfig {
            groups,
            xpat_entries,
            ..MockConfig::default()
        })
        .await;
        let config = test_config(server.port());
        let mut conn = NntpConnection::new("test".into());
        conn.connect(&config).await.unwrap();

        conn.group("alt.binaries.test").await.unwrap();
        let entries = conn
            .xpat(
                "subject",
                ArticleRange::Range(1, 100),
                &["*Agent Zeta*", "*Breaking Bad*"],
            )
            .await
            .unwrap();

        assert_eq!(entries.len(), 2);
    }

    #[tokio::test]
    async fn test_xpat_no_group_selected() {
        // Mock returns 420 (no articles matched) when no entries configured.
        let server = MockNntpServer::start(MockConfig::default()).await;
        let config = test_config(server.port());
        let mut conn = NntpConnection::new("test".into());
        conn.connect(&config).await.unwrap();

        let result = conn
            .xpat("subject", ArticleRange::Range(1, 100), &["*test*"])
            .await
            .unwrap();
        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn test_xpat_empty_patterns_error() {
        let mut groups = HashMap::new();
        groups.insert("alt.binaries.test".into(), (100u64, 1u64, 100u64));

        let server = MockNntpServer::start(MockConfig {
            groups,
            ..MockConfig::default()
        })
        .await;
        let config = test_config(server.port());
        let mut conn = NntpConnection::new("test".into());
        conn.connect(&config).await.unwrap();

        conn.group("alt.binaries.test").await.unwrap();
        let result = conn.xpat("subject", ArticleRange::Range(1, 100), &[]).await;
        assert!(matches!(result, Err(crate::error::NntpError::Protocol(_))));
        // State should still be Ready since we failed before sending
        assert_eq!(conn.state, ConnectionState::Ready);
    }

    // -----------------------------------------------------------------------
    // parse_list_active_data tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_parse_list_active_basic() {
        let data = b"alt.binaries.test 12345 1 y\r\nalt.binaries.misc 99999 500 n\r\n";
        let entries = parse_list_active_data(data);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].name, "alt.binaries.test");
        assert_eq!(entries[0].high, 12345);
        assert_eq!(entries[0].low, 1);
        assert_eq!(entries[0].status, "y");
        assert_eq!(entries[1].name, "alt.binaries.misc");
        assert_eq!(entries[1].high, 99999);
        assert_eq!(entries[1].low, 500);
        assert_eq!(entries[1].status, "n");
    }

    #[test]
    fn test_parse_list_active_moderated() {
        let data = b"comp.lang.rust 5000 1 m\r\n";
        let entries = parse_list_active_data(data);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].status, "m");
    }

    #[test]
    fn test_parse_list_active_empty() {
        let entries = parse_list_active_data(b"");
        assert!(entries.is_empty());
    }

    #[test]
    fn test_parse_list_active_skips_malformed() {
        let data = b"too.few 100\r\nalt.valid.group 5000 1 y\r\n\r\n";
        let entries = parse_list_active_data(data);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "alt.valid.group");
    }

    #[test]
    fn test_parse_list_active_large_article_numbers() {
        let data = b"alt.binaries.large 999999999999 1000000000 y\r\n";
        let entries = parse_list_active_data(data);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].high, 999999999999);
        assert_eq!(entries[0].low, 1000000000);
    }

    // -----------------------------------------------------------------------
    // parse_socks5_url tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_parse_socks5_url_basic() {
        let proxy = parse_socks5_url("socks5://127.0.0.1:1080").unwrap();
        assert_eq!(proxy.addr, "127.0.0.1:1080");
        assert!(proxy.auth.is_none());
    }

    #[test]
    fn test_parse_socks5_url_with_auth() {
        let proxy = parse_socks5_url("socks5://user:pass@proxy.example.com:9050").unwrap();
        assert_eq!(proxy.addr, "proxy.example.com:9050");
        let (user, pass) = proxy.auth.unwrap();
        assert_eq!(user, "user");
        assert_eq!(pass, "pass");
    }

    #[test]
    fn test_parse_socks5_url_invalid_scheme() {
        let result = parse_socks5_url("http://127.0.0.1:1080");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("socks5://"));
    }

    #[test]
    fn test_parse_socks5_url_missing_port() {
        // Still valid syntax — host_port will be "127.0.0.1" (no port)
        let proxy = parse_socks5_url("socks5://127.0.0.1").unwrap();
        assert_eq!(proxy.addr, "127.0.0.1");
    }

    #[test]
    fn test_parse_socks5_url_empty_host() {
        let result = parse_socks5_url("socks5://");
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_socks5_url_auth_missing_password() {
        let result = parse_socks5_url("socks5://user@host:1080");
        assert!(result.is_err());
    }

    // -----------------------------------------------------------------------
    // Dot-stuffing / multiline body edge cases
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_article_with_dot_stuffed_body() {
        let mut articles = HashMap::new();
        // Body with lines starting with dots — mock server will dot-stuff them
        articles.insert(
            "dot@test".into(),
            b"Line one\n.This starts with dot\n..Two dots\nEnd".to_vec(),
        );

        let server = MockNntpServer::start(MockConfig {
            articles,
            ..MockConfig::default()
        })
        .await;
        let config = test_config(server.port());
        let mut conn = NntpConnection::new("test".into());
        conn.connect(&config).await.unwrap();

        let response = conn.fetch_article("dot@test").await.unwrap();
        let data = response.data.unwrap();
        let body = String::from_utf8_lossy(&data);
        // After dot-unstuffing, dots should be restored correctly
        assert!(body.contains(".This starts with dot"));
        assert!(body.contains("..Two dots"));
    }

    #[tokio::test]
    async fn test_article_empty_body() {
        let mut articles = HashMap::new();
        articles.insert("empty@test".into(), b"".to_vec());

        let server = MockNntpServer::start(MockConfig {
            articles,
            ..MockConfig::default()
        })
        .await;
        let config = test_config(server.port());
        let mut conn = NntpConnection::new("test".into());
        conn.connect(&config).await.unwrap();

        let response = conn.fetch_article("empty@test").await.unwrap();
        assert_eq!(response.code, 220);
        // Body should exist but be minimal (just the blank line from mock)
        assert!(response.data.is_some());
    }

    // -----------------------------------------------------------------------
    // Compression helper tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_compression_flag_defaults_off() {
        let conn = NntpConnection::new("test".into());
        assert!(!conn.is_compress_enabled());
    }

    #[test]
    fn test_disable_compression() {
        let mut conn = NntpConnection::new("test".into());
        conn.compress_enabled = true;
        assert!(conn.is_compress_enabled());
        conn.disable_compression();
        assert!(!conn.is_compress_enabled());
    }

    // -----------------------------------------------------------------------
    // Auth edge cases
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_auth_required_but_no_credentials() {
        let server = MockNntpServer::start(MockConfig {
            auth_required: true,
            ..MockConfig::default()
        })
        .await;
        // Config without credentials
        let config = test_config(server.port());
        let mut conn = NntpConnection::new("test".into());

        // Should connect and be ready (no creds → no AUTH attempt)
        // but commands will get 480
        conn.connect(&config).await.unwrap();
        assert_eq!(conn.state, ConnectionState::Ready);

        // GROUP should trigger 480 from the mock
        let result = conn.group("alt.test").await;
        assert!(matches!(
            result,
            Err(crate::error::NntpError::AuthRequired(_))
        ));
    }

    #[tokio::test]
    async fn test_auth_any_credentials_accepted() {
        let server = MockNntpServer::start(MockConfig {
            auth_required: true,
            valid_credentials: None, // None = accept anything
            ..MockConfig::default()
        })
        .await;
        let config = test_config_with_auth(server.port(), "any_user", "any_pass");
        let mut conn = NntpConnection::new("test".into());

        conn.connect(&config).await.unwrap();
        assert_eq!(conn.state, ConnectionState::Ready);
    }

    // -----------------------------------------------------------------------
    // LIST ACTIVE mock server tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_list_active_success() {
        let list_active_entries = vec![
            "alt.binaries.test 50000 1 y".into(),
            "alt.binaries.misc 99999 500 n".into(),
            "comp.lang.rust 3000 1 m".into(),
        ];

        let server = MockNntpServer::start(MockConfig {
            list_active_entries,
            ..MockConfig::default()
        })
        .await;
        let config = test_config(server.port());
        let mut conn = NntpConnection::new("test".into());
        conn.connect(&config).await.unwrap();

        let entries = conn.list_active(None).await.unwrap();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].name, "alt.binaries.test");
        assert_eq!(entries[0].high, 50000);
        assert_eq!(entries[0].low, 1);
        assert_eq!(entries[0].status, "y");
        assert_eq!(entries[1].name, "alt.binaries.misc");
        assert_eq!(entries[1].status, "n");
        assert_eq!(entries[2].status, "m");
        assert_eq!(conn.state, ConnectionState::Ready);
    }

    #[tokio::test]
    async fn test_list_active_empty() {
        let server = MockNntpServer::start(MockConfig::default()).await;
        let config = test_config(server.port());
        let mut conn = NntpConnection::new("test".into());
        conn.connect(&config).await.unwrap();

        let entries = conn.list_active(None).await.unwrap();
        assert!(entries.is_empty());
        assert_eq!(conn.state, ConnectionState::Ready);
    }

    #[tokio::test]
    async fn test_list_active_with_wildmat() {
        let list_active_entries = vec!["alt.binaries.test 1000 1 y".into()];

        let server = MockNntpServer::start(MockConfig {
            list_active_entries,
            ..MockConfig::default()
        })
        .await;
        let config = test_config(server.port());
        let mut conn = NntpConnection::new("test".into());
        conn.connect(&config).await.unwrap();

        // Wildmat is sent but mock doesn't filter — just tests command formatting
        let entries = conn.list_active(Some("alt.binaries.*")).await.unwrap();
        assert_eq!(entries.len(), 1);
    }

    // -----------------------------------------------------------------------
    // Message-ID normalization edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn test_normalize_message_id_empty() {
        assert_eq!(normalize_message_id(""), "<>");
    }

    #[test]
    fn test_normalize_message_id_only_angle_brackets() {
        assert_eq!(normalize_message_id("<>"), "<>");
    }

    #[test]
    fn test_normalize_message_id_partial_brackets() {
        // Only opening bracket — not recognized as wrapped
        assert_eq!(normalize_message_id("<abc@test"), "<<abc@test>");
        // Only closing bracket
        assert_eq!(normalize_message_id("abc@test>"), "<abc@test>>");
    }

    // -----------------------------------------------------------------------
    // Response line edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn test_parse_response_line_with_extra_spaces() {
        let resp = parse_response_line("200  Multiple  Spaces\r\n").unwrap();
        assert_eq!(resp.code, 200);
        assert_eq!(resp.message, " Multiple  Spaces");
    }

    #[test]
    fn test_parse_response_line_bare_lf() {
        let resp = parse_response_line("200 OK\n").unwrap();
        assert_eq!(resp.code, 200);
        assert_eq!(resp.message, "OK");
    }

    #[test]
    fn test_parse_response_line_all_status_codes() {
        // Verify we can parse common NNTP codes
        for &(code, msg) in &[
            (200, "Service available, posting allowed"),
            (201, "Service available, posting prohibited"),
            (205, "Closing connection"),
            (211, "1234 5 6789 alt.test"),
            (215, "List of newsgroups follows"),
            (220, "0 <msg@id> Article follows"),
            (221, "Header follows"),
            (222, "0 <msg@id> Body follows"),
            (223, "0 <msg@id> Article exists"),
            (224, "Overview information follows"),
            (281, "Authentication accepted"),
            (290, "Compression enabled"),
            (381, "Password required"),
            (411, "No such newsgroup"),
            (412, "No newsgroup selected"),
            (420, "No article selected"),
            (430, "No such article"),
            (480, "Authentication required"),
            (481, "Authentication rejected"),
            (482, "Authentication rejected (temp)"),
            (500, "Command not recognized"),
            (502, "Service permanently unavailable"),
        ] {
            let line = format!("{code} {msg}\r\n");
            let resp = parse_response_line(&line).unwrap();
            assert_eq!(resp.code, code);
            assert_eq!(resp.message, msg);
        }
    }

    // -----------------------------------------------------------------------
    // XOVER parsing edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn test_parse_xover_with_extra_fields() {
        // Some servers include additional fields beyond the standard 8
        let data = b"100\tSubj\tFrom\tDate\t<mid@x>\trefs\t500\t50\textra1\textra2\r\n";
        let entries = parse_xover_data(data);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].article_num, 100);
        assert_eq!(entries[0].lines, 50);
    }

    #[test]
    fn test_parse_xover_unparseable_numbers() {
        let data = b"notnum\tSubj\tFrom\tDate\t<mid@x>\trefs\tnotnum\tnotnum\r\n";
        let entries = parse_xover_data(data);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].article_num, 0); // unwrap_or(0)
        assert_eq!(entries[0].bytes, 0);
        assert_eq!(entries[0].lines, 0);
    }

    #[test]
    fn test_parse_xover_message_id_without_brackets() {
        let data = b"1\tSubj\tFrom\tDate\tmid@noBrackets\trefs\t100\t10\r\n";
        let entries = parse_xover_data(data);
        assert_eq!(entries[0].message_id, "mid@noBrackets");
    }

    // -----------------------------------------------------------------------
    // Connection state checks
    // -----------------------------------------------------------------------

    #[test]
    fn test_connection_state_equality() {
        assert_eq!(ConnectionState::Disconnected, ConnectionState::Disconnected);
        assert_ne!(ConnectionState::Ready, ConnectionState::Busy);
        assert_ne!(ConnectionState::Error, ConnectionState::Ready);
    }

    #[test]
    fn test_is_connected_when_disconnected() {
        let conn = NntpConnection::new("test".into());
        assert!(!conn.is_connected());
        assert_eq!(conn.state, ConnectionState::Disconnected);
    }

    // -----------------------------------------------------------------------
    // Sequential command flow tests (RFC 3977 state machine)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_group_then_xover_then_article() {
        let mut groups = HashMap::new();
        groups.insert("alt.test".into(), (10u64, 1u64, 10u64));

        let mut articles = HashMap::new();
        articles.insert("a1@test".into(), b"Article one body".to_vec());

        let xover_entries = vec!["1\tSubject\tposter@x\tDate\t<a1@test>\t\t1000\t20".into()];

        let server = MockNntpServer::start(MockConfig {
            groups,
            articles,
            xover_entries,
            ..MockConfig::default()
        })
        .await;
        let config = test_config(server.port());
        let mut conn = NntpConnection::new("test".into());
        conn.connect(&config).await.unwrap();

        // 1. Select group (RFC 3977 §6.1.1)
        let group = conn.group("alt.test").await.unwrap();
        assert_eq!(group.count, 10);

        // 2. Get overview (RFC 2980 §2.8)
        let overview = conn.xover(1, 10).await.unwrap();
        assert_eq!(overview.len(), 1);
        assert_eq!(overview[0].message_id, "a1@test");

        // 3. Fetch article using message-id from overview
        let article = conn.fetch_article(&overview[0].message_id).await.unwrap();
        assert_eq!(article.code, 220);
        let data = article.data.unwrap();
        let body = String::from_utf8_lossy(&data);
        assert!(body.contains("Article one body"));
    }

    #[tokio::test]
    async fn test_stat_then_article_workflow() {
        let mut articles = HashMap::new();
        articles.insert("check@test".into(), b"fetched after stat".to_vec());

        let server = MockNntpServer::start(MockConfig {
            articles,
            ..MockConfig::default()
        })
        .await;
        let config = test_config(server.port());
        let mut conn = NntpConnection::new("test".into());
        conn.connect(&config).await.unwrap();

        // STAT to check existence first
        let stat = conn.stat_article("check@test").await.unwrap();
        assert_eq!(stat.code, 223);

        // Then fetch
        let art = conn.fetch_article("check@test").await.unwrap();
        assert_eq!(art.code, 220);
        assert!(String::from_utf8_lossy(&art.data.unwrap()).contains("fetched after stat"));
    }

    // -----------------------------------------------------------------------
    // Auth-gated command tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_commands_after_successful_auth() {
        let mut groups = HashMap::new();
        groups.insert("alt.test".into(), (50u64, 1u64, 50u64));

        let mut articles = HashMap::new();
        articles.insert("authed@test".into(), b"secured data".to_vec());

        let server = MockNntpServer::start(MockConfig {
            auth_required: true,
            valid_credentials: Some(("user".into(), "pass".into())),
            groups,
            articles,
            ..MockConfig::default()
        })
        .await;
        let config = test_config_with_auth(server.port(), "user", "pass");
        let mut conn = NntpConnection::new("test".into());
        conn.connect(&config).await.unwrap();

        // All commands should work after auth
        let group = conn.group("alt.test").await.unwrap();
        assert_eq!(group.count, 50);

        let stat = conn.stat_article("authed@test").await.unwrap();
        assert_eq!(stat.code, 223);

        let art = conn.fetch_article("authed@test").await.unwrap();
        assert_eq!(art.code, 220);

        let body = conn.fetch_body("authed@test").await.unwrap();
        assert_eq!(body.code, 222);
    }
}
