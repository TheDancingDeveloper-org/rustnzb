# NNTP Specification Compliance

This document validates the `nzb-nntp` crate's alignment with the relevant NNTP RFCs and de-facto extensions used by Usenet providers.

## Applicable Standards

| RFC | Title | Relevance |
|-----|-------|-----------|
| [RFC 3977](https://datatracker.ietf.org/doc/html/rfc3977) | Network News Transfer Protocol (NNTP) | Core protocol, supersedes RFC 977 |
| [RFC 4643](https://datatracker.ietf.org/doc/html/rfc4643) | Network News Transfer Protocol (NNTP) Extension for Authentication | AUTHINFO USER/PASS |
| [RFC 2980](https://datatracker.ietf.org/doc/html/rfc2980) | Common NNTP Extensions | XOVER, XHDR, XPAT, LIST EXTENSIONS |
| [RFC 4642](https://datatracker.ietf.org/doc/html/rfc4642) | Using TLS with NNTP | Implicit TLS (port 563) |

---

## RFC 3977 — Core Protocol

### Connection Lifecycle (Section 5)

| Requirement | Status | Notes |
|-------------|--------|-------|
| Client connects via TCP | **Compliant** | `NntpConnection::connect()` establishes TCP via `TcpStream::connect()` |
| Read initial welcome response | **Compliant** | Reads welcome banner after TCP/TLS established |
| 200 = service available, posting allowed | **Compliant** | Accepted as successful connect |
| 201 = service available, posting prohibited | **Compliant** | Accepted as successful connect (read-only is fine for a download client) |
| 400 = service temporarily unavailable | **Not handled** | Uncommon; would fall through to Protocol error. Low risk — most providers use 502 |
| 502 = service permanently unavailable | **Compliant** | Mapped to `NntpError::ServiceUnavailable`, transitions state to Error |
| Commands terminated with CRLF | **Compliant** | `send_command()` appends `\r\n` to all commands |
| Responses terminated with CRLF | **Compliant** | `read_response_line()` reads lines terminated by `\n` (tolerates bare LF) |

### Response Format (Section 3.2)

| Requirement | Status | Notes |
|-------------|--------|-------|
| Three-digit status code | **Compliant** | `parse_response_line()` extracts first 3 characters as numeric code |
| Space separator after code | **Compliant** | Message extracted from position 4 onward |
| Code-only responses (no message) | **Compliant** | Returns empty string for message |
| Multi-line responses dot-terminated | **Compliant** | `read_multiline_body()` reads until lone `.\r\n` |
| Dot-stuffing: leading `..` unstuffed to `.` | **Compliant** | Explicit dot-unstuffing in `read_multiline_body()` |
| Bare `.\n` treated as terminator | **Compliant** | Checks for both `.\r\n` and `.\n` |

### ARTICLE Command (Section 6.2.1)

| Requirement | Status | Notes |
|-------------|--------|-------|
| `ARTICLE <message-id>` syntax | **Compliant** | Message-ID normalized to angle-bracket form |
| `ARTICLE <number>` syntax | **Not implemented** | Only message-id form is used (sufficient for article downloading) |
| 220 = article follows (head + body) | **Compliant** | Reads multi-line body, returns in `NntpResponse.data` |
| 430 = no such article | **Compliant** | Mapped to `NntpError::ArticleNotFound` |
| 412 = no newsgroup selected | **Compliant** | Mapped to `NntpError::NoArticleSelected` |
| 420 = no current article number | **Compliant** | Mapped to `NntpError::NoArticleSelected` |
| State returns to Ready after 430 | **Compliant** | Non-fatal error, connection remains usable |
| State transitions to Error on 480/481/502 | **Compliant** | Fatal errors require reconnection |

### BODY Command (Section 6.2.3)

| Requirement | Status | Notes |
|-------------|--------|-------|
| `BODY <message-id>` syntax | **Compliant** | Message-ID normalized |
| 222 = body follows | **Compliant** | Reads multi-line body |
| 430 = no such article | **Compliant** | Mapped to `NntpError::ArticleNotFound` |
| Error handling matches ARTICLE | **Compliant** | Same response code handling (412, 420, 480, 481, 482, 502) |

### STAT Command (Section 6.2.4)

| Requirement | Status | Notes |
|-------------|--------|-------|
| `STAT <message-id>` syntax | **Compliant** | Message-ID normalized |
| 223 = article exists (single-line response) | **Compliant** | No body to read |
| 430 = no such article | **Compliant** | Mapped to `NntpError::ArticleNotFound` |
| No multi-line body | **Compliant** | Only reads response line |

### GROUP Command (Section 6.1.1)

| Requirement | Status | Notes |
|-------------|--------|-------|
| `GROUP <name>` syntax | **Compliant** | |
| 211 response format: `211 count first last name` | **Compliant** | Parsed into `GroupResponse` struct |
| 411 = no such newsgroup | **Compliant** | Mapped to `NntpError::NoSuchGroup` |
| Malformed 211 response handling | **Compliant** | Returns Protocol error if < 3 fields |
| Group name echoed in response | **Compliant** | Falls back to requested name if not present |

### LIST ACTIVE Command (Section 7.6.3)

| Requirement | Status | Notes |
|-------------|--------|-------|
| `LIST ACTIVE [wildmat]` syntax | **Compliant** | Optional wildmat pattern supported |
| 215 = list follows | **Compliant** | Reads multi-line body |
| Entry format: `name high low status` | **Compliant** | Parsed into `ListActiveEntry` |
| Posting flags: y, n, m | **Compliant** | Stored as string, no validation (correct per spec — values are informational) |

### QUIT Command (Section 5.4)

| Requirement | Status | Notes |
|-------------|--------|-------|
| `QUIT` terminates session | **Compliant** | Best-effort send, reads 205 response |
| 205 = closing connection | **Compliant** | Logged, then transport shut down |
| Graceful on I/O errors | **Compliant** | Errors ignored during quit (best-effort) |
| Transport shutdown after QUIT | **Compliant** | Socket shutdown called after response |

### Message-ID Format (Section 3.6)

| Requirement | Status | Notes |
|-------------|--------|-------|
| Message-IDs enclosed in angle brackets | **Compliant** | `normalize_message_id()` wraps bare IDs in `<>` |
| Already-bracketed IDs left unchanged | **Compliant** | Detected by checking first/last chars |

---

## RFC 4643 — Authentication

### AUTHINFO USER/PASS (Section 2.3)

| Requirement | Status | Notes |
|-------------|--------|-------|
| `AUTHINFO USER <username>` | **Compliant** | Sent as first auth step |
| `AUTHINFO PASS <password>` | **Compliant** | Sent after 381 response |
| 281 = authentication successful | **Compliant** | Transitions to Ready state |
| 381 = password required | **Compliant** | Triggers PASS command |
| 481 = authentication failed | **Compliant** | Mapped to `NntpError::Auth`, state → Error |
| 482 = authentication rejected | **Compliant** | Treated same as 481 (common with providers like Eweka, Tweaknews) |
| 480 = authentication required | **Compliant** | During USER: treated as "continue to PASS". During commands: `NntpError::AuthRequired` |
| 502 = service unavailable | **Compliant** | Mapped to `NntpError::ServiceUnavailable` |
| Auth before other commands | **Compliant** | `connect()` authenticates before returning Ready |
| Username-only auth (281 to USER) | **Compliant** | Handles the unusual case where USER alone suffices |

### Deviation Notes

- **No SASL support**: RFC 4643 also defines AUTHINFO SASL (Section 2.4). This crate only implements the simpler USER/PASS mechanism, which is the de-facto standard for Usenet providers. SASL is not used by any mainstream provider.
- **482 treated as permanent failure**: The RFC defines 482 as "authentication commands issued out of sequence." In practice, providers use 482 for account suspension/block exhaustion. Treating it as a fatal auth error is the pragmatic choice.

---

## RFC 2980 — Common NNTP Extensions

### XOVER (Section 2.8)

| Requirement | Status | Notes |
|-------------|--------|-------|
| `XOVER range` syntax | **Compliant** | Format: `XOVER start-end` |
| 224 = overview data follows | **Compliant** | Reads multi-line body, parses tab-delimited fields |
| Tab-delimited fields (8+ columns) | **Compliant** | Parses: article_num, subject, from, date, message-id, references, bytes, lines |
| Extra fields beyond standard 8 | **Compliant** | Silently ignored (parsed fields stop at 8) |
| Message-ID angle brackets stripped | **Compliant** | `trim_matches` removes `<>` |
| 412 = no newsgroup selected | **Compliant** | Mapped to `NntpError::NoSuchGroup` |
| 420 = no articles in range | **Compliant** | Returns empty `Vec` (not an error) |
| Malformed lines skipped | **Compliant** | Lines with < 8 tab-separated fields are ignored |
| 423 = no articles in that range (RFC 3977 §8.3.2) | **Compliant** | Returns empty Vec (not an error), e.g. for an expired range |

### XHDR (Section 2.6)

| Requirement | Status | Notes |
|-------------|--------|-------|
| `XHDR header range` syntax | **Compliant** | Supports both `start-end` ranges and message-id |
| 221 = header data follows | **Compliant** | Reads multi-line body |
| Entry format: `artnum value` | **Compliant** | Parsed into `HeaderEntry` |
| 420 = no articles in range | **Compliant** | Returns empty `Vec` |
| 430 = no such article | **Compliant** | When querying by message-id |

### XPAT (Section 2.9)

| Requirement | Status | Notes |
|-------------|--------|-------|
| `XPAT header range pattern [pattern...]` | **Compliant** | Multiple patterns space-separated |
| Wildmat pattern syntax | **Compliant** | Patterns sent as-is to server (server-side matching) |
| 221 = matching headers follow | **Compliant** | Same parser as XHDR |
| Empty patterns rejected | **Compliant** | Returns Protocol error if no patterns provided |

### LIST EXTENSIONS

| Requirement | Status | Notes |
|-------------|--------|-------|
| `LIST EXTENSIONS` command | **Compliant** | Used during compression negotiation |
| 202 = extension list follows | **Compliant** | Reads multi-line body, checks for XFEATURE COMPRESS GZIP |
| Non-202 response handled | **Compliant** | Gracefully continues without compression |

---

## RFC 4642 — TLS with NNTP

| Requirement | Status | Notes |
|-------------|--------|-------|
| Implicit TLS (connect directly over TLS) | **Compliant** | When `ssl: true`, TLS handshake occurs immediately after TCP connect |
| STARTTLS upgrade | **Not implemented** | Implicit TLS on port 563 is the universal standard for Usenet; STARTTLS (port 119 → upgrade) is rarely supported by providers |
| Certificate verification | **Compliant** | Uses webpki-roots CA bundle by default |
| Certificate verification bypass | **Compliant** | `ssl_verify: false` disables verification via custom `ServerCertVerifier` |
| SNI (Server Name Indication) | **Compliant** | Hostname passed as `ServerName` to rustls |
| TLS 1.2 and 1.3 support | **Compliant** | Provided by rustls (TLS 1.2+ only, no legacy protocols) |

---

## De-Facto Extensions (Non-RFC)

### XFEATURE COMPRESS GZIP

This extension is not standardized in any RFC but is widely supported by Usenet providers (Giganews, Eweka, UsenetExpress, etc.).

| Requirement | Status | Notes |
|-------------|--------|-------|
| Feature discovery via LIST EXTENSIONS | **Compliant** | Checks for `XFEATURE COMPRESS GZIP` in extension list |
| `XFEATURE COMPRESS GZIP` command | **Compliant** | Sent after feature discovery |
| 290 = compression enabled | **Compliant** | Sets `compress_enabled` flag |
| Gzip decompression of response bodies | **Compliant** | Detects gzip magic bytes (`1f 8b`), decompresses with flate2 |
| Fallback on decompression failure | **Compliant** | Returns raw data if decompression fails |
| Disable compression at runtime | **Compliant** | `disable_compression()` method available |

### SOCKS5 Proxy Support

| Requirement | Status | Notes |
|-------------|--------|-------|
| `socks5://host:port` URLs | **Compliant** | Parsed by `parse_socks5_url()` |
| Username/password authentication | **Compliant** | `socks5://user:pass@host:port` format |
| Transparent proxying | **Compliant** | TCP stream wrapped before TLS/NNTP negotiation |

---

## Pipelining Compliance

NNTP guarantees responses are returned in the order commands are sent (RFC 3977, Section 3.5). This crate exploits this for performance:

| Requirement | Status | Notes |
|-------------|--------|-------|
| Ordered response guarantee | **Relied upon** | Pipeline correlates responses with FIFO request queue |
| ARTICLE pipelining | **Compliant** | `Pipeline` sends up to `depth` ARTICLE commands before reading |
| STAT pipelining | **Compliant** | `StatPipeline` sends batches of up to 100 STAT commands |
| Fatal error aborts pipeline | **Compliant** | Auth/service/connection errors drain remaining requests as failures |
| Non-fatal errors don't abort | **Compliant** | 430 (not found) continues processing remaining pipeline |

---

## Response Code Coverage

Summary of all NNTP response codes handled by this crate:

| Code | Meaning | Handling |
|------|---------|----------|
| 200 | Service available, posting allowed | Accept welcome |
| 201 | Service available, posting prohibited | Accept welcome |
| 202 | Extension list follows | Read multi-line body (LIST EXTENSIONS) |
| 205 | Closing connection | QUIT acknowledged |
| 211 | Group selected | Parse count/first/last/name |
| 215 | List follows | Parse LIST ACTIVE entries |
| 220 | Article follows | Read multi-line body |
| 221 | Header data follows | Read XHDR/XPAT body |
| 222 | Body follows | Read multi-line body |
| 223 | Article exists | STAT success (single-line) |
| 224 | Overview data follows | Read XOVER body |
| 281 | Authentication successful | Auth complete |
| 290 | Compression enabled | XFEATURE success |
| 381 | Password required | Continue to PASS |
| 411 | No such newsgroup | `NntpError::NoSuchGroup` |
| 412 | No newsgroup selected | `NntpError::NoArticleSelected` or `NntpError::NoSuchGroup` |
| 420 | No article selected / no articles | Empty result or `NntpError::NoArticleSelected` |
| 430 | No such article | `NntpError::ArticleNotFound` |
| 480 | Authentication required | `NntpError::AuthRequired` |
| 481 | Authentication rejected | `NntpError::Auth` |
| 482 | Authentication rejected (provider) | `NntpError::Auth` |
| 502 | Service permanently unavailable | `NntpError::ServiceUnavailable` |

---

## Commands Not Implemented

The following RFC 3977 commands are not implemented, by design (not needed for an article downloading client):

| Command | RFC Section | Reason |
|---------|-------------|--------|
| POST | 6.3.1 | Read-only client |
| IHAVE | 6.3.2 | Server-to-server transfer |
| NEWNEWS | 7.4 | Not used by binary download workflows |
| NEWGROUPS | 7.3 | Not used by binary download workflows |
| HEAD | 6.2.2 | XHDR provides more targeted header access |
| ARTICLE by number | 6.2.1 | Message-ID form is sufficient for NZB-based downloading |
| LISTGROUP | 6.1.2 | Not needed when using XOVER for article enumeration |
| OVER (NNTP v2) | 8.3 | XOVER is the universally supported equivalent |
| HDR (NNTP v2) | 8.5 | XHDR is the universally supported equivalent |
| DATE | 7.1 | Not needed for download workflows |
| HELP | 7.2 | Not needed programmatically |
| CAPABILITIES | 5.2 | LIST EXTENSIONS used instead (broader provider support) |
| MODE READER | 5.3 | Most providers don't require it; implicit in the connection |
| STARTTLS | RFC 4642 | Implicit TLS on port 563 is the universal Usenet standard |

---

## Summary

The `nzb-nntp` crate provides **full compliance** with the subset of NNTP commands required for a high-performance article downloading client. Key strengths:

- **Complete coverage** of article retrieval commands (ARTICLE, BODY, STAT, GROUP, XOVER, XHDR, XPAT, LIST ACTIVE)
- **Robust authentication** per RFC 4643, with pragmatic handling of provider-specific codes (482)
- **Correct multi-line body handling** including dot-unstuffing per RFC 3977
- **Proper state machine** with clear separation between recoverable and fatal errors
- **Standards-based TLS** via rustls with certificate verification and SNI
- **Pipelining** that correctly leverages RFC 3977's ordered-response guarantee

The only notable omissions are STARTTLS (superseded by implicit TLS), SASL authentication (unused by providers), and posting/transfer commands (out of scope for a download client).
