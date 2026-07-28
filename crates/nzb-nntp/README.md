# nzb-nntp

Async NNTP client library for Rust, designed for high-performance Usenet article downloading.

## Features

- **Async/await** — Built on Tokio for non-blocking I/O
- **TLS** — Implicit TLS via rustls with optional certificate verification bypass
- **Authentication** — AUTHINFO USER/PASS (RFC 4643)
- **Pipelining** — Send multiple ARTICLE/STAT commands before reading responses, dramatically improving throughput on high-latency links
- **Connection pooling** — Per-server pools with health checking, idle timeouts, and automatic reconnection
- **Multi-server failover** — Priority-based server selection with automatic retry on next server
- **Penalty system** — Temporarily backs off servers experiencing errors (auth, timeout, connection)
- **GZIP compression** — XFEATURE COMPRESS GZIP negotiation and transparent decompression
- **SOCKS5 proxy** — Route connections through SOCKS5 proxies with optional authentication
- **Bandwidth limiting** — Global bandwidth throttling
- **Pause/resume/shutdown** — Runtime control of download operations

## Quick Start

Add to your `Cargo.toml`:

```toml
[dependencies]
nzb-nntp = "0.1"
tokio = { version = "1", features = ["full"] }
```

### Single Connection

```rust
use nzb_nntp::{NntpConnection, ServerConfig};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = ServerConfig {
        id: "my-server".into(),
        name: "Usenet Provider".into(),
        host: "news.example.com".into(),
        port: 563,
        ssl: true,
        ssl_verify: true,
        username: Some("user".into()),
        password: Some("pass".into()),
        ..ServerConfig::default()
    };

    let mut conn = NntpConnection::new("conn-1".into());
    conn.connect(&config).await?;

    // Fetch an article by message-id
    let response = conn.fetch_article("abc123@example.com").await?;
    println!("Article size: {} bytes", response.data.unwrap().len());

    // Check if an article exists (no data transfer)
    let stat = conn.stat_article("def456@example.com").await?;
    println!("Article exists: code {}", stat.code);

    conn.quit().await?;
    Ok(())
}
```

### Group Browsing

```rust
use nzb_nntp::{NntpConnection, ServerConfig, ArticleRange};

async fn browse(conn: &mut NntpConnection) -> nzb_nntp::NntpResult<()> {
    // Select a newsgroup
    let group = conn.group("alt.binaries.test").await?;
    println!("{}: {} articles ({}-{})", group.name, group.count, group.first, group.last);

    // Get article overview data
    let overview = conn.xover(group.last - 100, group.last).await?;
    for entry in &overview {
        println!("  [{}] {} ({}B)", entry.article_num, entry.subject, entry.bytes);
    }

    // Search by subject pattern
    let matches = conn.xpat(
        "subject",
        ArticleRange::Range(group.first, group.last),
        &["*linux*"],
    ).await?;
    println!("Found {} matching articles", matches.len());

    // Get specific headers
    let subjects = conn.xhdr(
        "subject",
        ArticleRange::Range(group.last - 10, group.last),
    ).await?;
    for h in &subjects {
        println!("  #{}: {}", h.article_num, h.value);
    }

    // List newsgroups
    let groups = conn.list_active(Some("alt.binaries.*")).await?;
    for g in &groups {
        println!("  {} ({}-{}) [{}]", g.name, g.low, g.high, g.status);
    }

    Ok(())
}
```

### Pipelining

Pipelining sends multiple commands before reading responses, reducing round-trip overhead:

```rust
use nzb_nntp::{NntpConnection, Pipeline, StatPipeline};

async fn pipelined_download(conn: &mut NntpConnection) -> nzb_nntp::NntpResult<()> {
    // Pipeline ARTICLE commands (depth = 5 concurrent requests)
    let mut pipeline = Pipeline::new(5);
    pipeline.submit("msg1@example.com".into(), 1);
    pipeline.submit("msg2@example.com".into(), 2);
    pipeline.submit("msg3@example.com".into(), 3);

    let results = pipeline.process_all(conn).await?;
    for r in &results {
        match &r.result {
            Ok(resp) => println!("Tag {}: {} bytes", r.request.tag, resp.data.as_ref().map_or(0, |d| d.len())),
            Err(e) => println!("Tag {}: {}", r.request.tag, e),
        }
    }

    // Batch STAT checks (up to 100 per batch)
    let mut stat_pipe = StatPipeline::new();
    stat_pipe.add("check1@example.com".into());
    stat_pipe.add("check2@example.com".into());

    let stat_results = stat_pipe.execute(conn).await?;
    for s in &stat_results {
        println!("{}: {}", s.message_id, if s.exists { "exists" } else { "missing" });
    }

    Ok(())
}
```

### Multi-Server Downloading

The `Downloader` coordinates article fetching across multiple servers with automatic failover:

```rust
use nzb_nntp::{Article, Downloader, ServerConfig};
use tokio::sync::mpsc;

async fn download_articles() -> nzb_nntp::NntpResult<()> {
    let servers = vec![
        ServerConfig {
            id: "primary".into(),
            name: "Primary Provider".into(),
            host: "news.primary.com".into(),
            port: 563,
            ssl: true,
            priority: 0, // Tried first
            connections: 8,
            pipelining: 5,
            ..ServerConfig::default()
        },
        ServerConfig {
            id: "backup".into(),
            name: "Backup Provider".into(),
            host: "news.backup.com".into(),
            port: 563,
            ssl: true,
            priority: 1, // Tried if primary fails
            connections: 4,
            ..ServerConfig::default()
        },
    ];

    let downloader = Downloader::new(servers, 0); // 0 = no bandwidth limit

    let articles = vec![
        Article {
            message_id: "part1@example.com".into(),
            segment_number: 1,
            bytes: 768000,
            downloaded: false,
            data_begin: None,
            data_size: None,
            crc32: None,
            tried_servers: Vec::new(),
            tries: 0,
        },
        // ... more articles
    ];

    let (tx, mut rx) = mpsc::channel(100);

    // Start download (runs to completion)
    tokio::spawn({
        let downloader_ref = &downloader;
        async move {
            downloader_ref.download(articles, tx).await
        }
    });

    // Process results as they arrive
    while let Some(result) = rx.recv().await {
        match result.result {
            Ok(data) => println!(
                "Segment {}: {} bytes from {}",
                result.article.segment_number,
                data.len(),
                result.server_id.unwrap_or_default()
            ),
            Err(e) => eprintln!(
                "Segment {} failed: {}",
                result.article.segment_number, e
            ),
        }
    }

    Ok(())
}
```

### Connection Pooling

```rust
use nzb_nntp::{ConnectionPool, ServerConfig};
use std::sync::Arc;

async fn pooled_usage() -> nzb_nntp::NntpResult<()> {
    let config = Arc::new(ServerConfig {
        id: "pooled-server".into(),
        host: "news.example.com".into(),
        port: 563,
        ssl: true,
        connections: 4, // Max 4 concurrent connections
        ..ServerConfig::default()
    });

    let pool = ConnectionPool::new(config);

    // Acquire a connection (creates new or reuses idle)
    let mut pooled = pool.acquire().await?;

    // Use the connection
    let resp = pooled.conn.fetch_article("msg@example.com").await?;
    println!("Got {} bytes", resp.data.unwrap().len());

    // Return to pool for reuse
    pool.release(pooled);

    // Pool tracks idle connections
    println!("Idle connections: {}", pool.idle_count());

    // Clean up
    pool.close_idle().await;

    Ok(())
}
```

## Architecture

```
                 ┌──────────────┐
                 │  Downloader   │  Orchestrates multi-server downloads
                 │  (failover)   │  with priority, pause/resume, bandwidth
                 └──────┬───────┘
                        │
              ┌─────────┼─────────┐
              ▼         ▼         ▼
        ┌───────────┐         ┌───────────┐
        │ServerState│   ...   │ServerState│  Per-server health tracking,
        │ + Pool    │         │ + Pool    │  penalties, speed metrics
        └─────┬─────┘         └─────┬─────┘
              │                     │
              ▼                     ▼
     ┌─────────────────┐   ┌─────────────────┐
     │ConnectionPool   │   │ConnectionPool   │  Semaphore-limited pools,
     │ (idle conns)    │   │ (idle conns)    │  health checks, reconnect
     └────────┬────────┘   └────────┬────────┘
              │                     │
              ▼                     ▼
     ┌─────────────────┐   ┌─────────────────┐
     │NntpConnection   │   │NntpConnection   │  TCP/TLS state machine,
     │ (state machine) │   │ (state machine) │  auth, commands, compression
     └────────┬────────┘   └────────┬────────┘
              │                     │
              ▼                     ▼
     ┌──────────────┐      ┌──────────────┐
     │  Transport   │      │  Transport   │    Plain TCP or TLS stream
     │ (TCP / TLS)  │      │ (TCP / TLS)  │    with buffered I/O
     └──────────────┘      └──────────────┘
```

### Module Summary

| Module | Purpose |
|--------|---------|
| `config` | `ServerConfig`, `Article`, `ListActiveEntry` — serializable configuration types |
| `error` | `NntpError` enum with NNTP-specific variants and response code mapping |
| `connection` | RFC 3977 state machine: connect, auth, TLS, compression, all NNTP commands |
| `pipeline` | `Pipeline` (ARTICLE) and `StatPipeline` (STAT) for request pipelining |
| `pool` | Per-server connection pool with semaphore limiting and health checks |
| `server` | `ServerState` with penalty system, speed tracking, failure ratios |
| `downloader` | `Downloader` orchestrator: priority failover, pause/resume, bandwidth limiting |

## Configuration

`ServerConfig` supports serde serialization for easy config file integration:

```json
{
  "id": "news-provider",
  "name": "My Usenet Provider",
  "host": "news.example.com",
  "port": 563,
  "ssl": true,
  "ssl_verify": true,
  "username": "myuser",
  "password": "mypass",
  "connections": 8,
  "priority": 0,
  "enabled": true,
  "retention": 3600,
  "pipelining": 5,
  "optional": false,
  "compress": true,
  "proxy_url": null
}
```

| Field | Default | Description |
|-------|---------|-------------|
| `id` | — | Unique identifier for the server |
| `name` | — | Human-readable display name |
| `host` | — | NNTP server hostname |
| `port` | `563` | Server port (563 = NNTPS) |
| `ssl` | `true` | Enable TLS |
| `ssl_verify` | `true` | Verify TLS certificates |
| `username` | `None` | Auth username |
| `password` | `None` | Auth password |
| `connections` | `4` | Max concurrent connections to this server |
| `priority` | `0` | Server priority (0 = highest, tried first) |
| `enabled` | `true` | Include this server in downloads |
| `retention` | `0` | Article retention in days (0 = unlimited) |
| `pipelining` | `4` | Pipeline depth (1 = no pipelining) |
| `optional` | `false` | If true, failures don't block the download |
| `compress` | `false` | Negotiate XFEATURE COMPRESS GZIP |
| `proxy_url` | `None` | SOCKS5 proxy: `socks5://[user:pass@]host:port` |

## Error Handling

All operations return `NntpResult<T>` (alias for `Result<T, NntpError>`). Errors are structured by cause:

| Error | Trigger | Recoverable? |
|-------|---------|--------------|
| `ArticleNotFound` | 430 response | Yes — try another server |
| `NoSuchGroup` | 411 response | Yes — check group name |
| `NoArticleSelected` | 412, 420 responses | Yes — select group first |
| `AuthRequired` | 480 response | No — needs reconnection with credentials |
| `Auth` | 481, 482 responses | No — check credentials |
| `ServiceUnavailable` | 502 response | No — server is down |
| `Connection` | TCP/socket errors | No — reconnect required |
| `Tls` | TLS handshake failure | No — check TLS config |
| `Protocol` | Unexpected response codes | No — possible server bug |
| `Io` | Underlying I/O errors | No — reconnect required |
| `Timeout` | Pool acquire timeout | Retry after delay |
| `AllServersExhausted` | Article not on any server | No — article is unavailable |
| `Shutdown` | Downloader shut down | No — intentional |

Fatal errors (Auth, ServiceUnavailable, Connection, I/O) transition the connection to `Error` state, requiring a new connection. Non-fatal errors (ArticleNotFound, NoSuchGroup) keep the connection in `Ready` state.

## NNTP Commands Supported

| Command | Method | RFC |
|---------|--------|-----|
| ARTICLE | `fetch_article()` | RFC 3977 §6.2.1 |
| BODY | `fetch_body()` | RFC 3977 §6.2.3 |
| STAT | `stat_article()` | RFC 3977 §6.2.4 |
| GROUP | `group()` | RFC 3977 §6.1.1 |
| XOVER | `xover()` | RFC 2980 §2.8 |
| XHDR | `xhdr()` | RFC 2980 §2.6 |
| XPAT | `xpat()` | RFC 2980 §2.9 |
| LIST ACTIVE | `list_active()` | RFC 3977 §7.6.3 |
| AUTHINFO USER/PASS | `connect()` (automatic) | RFC 4643 §2.3 |
| XFEATURE COMPRESS GZIP | `connect()` (automatic) | De-facto standard |
| QUIT | `quit()` | RFC 3977 §5.4 |

For detailed spec compliance analysis, see [NNTP_COMPLIANCE.md](NNTP_COMPLIANCE.md).

## Testing

```bash
cargo test
```

The test suite uses an in-process mock NNTP server (`testutil::MockNntpServer`) that supports all implemented commands with configurable responses and error injection. No external NNTP server is required.

## License

MIT
