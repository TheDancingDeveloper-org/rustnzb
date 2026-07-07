# rustnzb

**A Modern Usenet Binary Downloader, Rewritten From the Ground Up in Rust**

Built from scratch with zero legacy dependencies. Fast, efficient, and designed for self-hosters. Full NNTP pipeline with yEnc decoding, PAR2 verification & repair, archive extraction, and a clean web UI. No inherited technical debt -- just modern Rust with async I/O, connection pooling, and NNTP pipelining for maximum throughput.

[![Rust](https://img.shields.io/badge/Rust-2024_edition-orange)](https://www.rust-lang.org/)
[![Docker](https://img.shields.io/badge/Docker-Hub-blue)](https://hub.docker.com/r/ausagentsmith/rustnzb)
[![License: MIT](https://img.shields.io/badge/License-MIT-green.svg)](LICENSE)

---

## Features

| | Feature | Description |
|-|---------|-------------|
| **⚡** | **NNTP Pipelining** | Send multiple ARTICLE commands per connection before reading responses. Configurable pipeline depth per server eliminates round-trip latency. |
| **🔄** | **Multi-Server Failover** | Priority-ordered server list with automatic failover. Articles not found on one server are retried on the next. Optional servers for fill providers. |
| **📦** | **yEnc Decoding** | Fast yEnc decoder with CRC32 validation. Handles multi-part articles, escape sequences, and assembles files from decoded segments. |
| **🔧** | **PAR2 Verify & Repair** | Automatic PAR2 verification after download. Damaged files are repaired using recovery blocks before extraction. |
| **📂** | **Archive Extraction** | Automatic extraction of RAR, 7z, and ZIP archives after download and repair. Supports multi-part RAR with cleanup. |
| **🖥️** | **Clean Web UI** | Responsive single-page interface. Queue management, download history, server configuration, real-time logs, and drag-and-drop NZB upload. |
| **🖱️** | **Desktop App** | Native desktop application for Windows, macOS, and Linux powered by Tauri. System tray with queue count and speed. |
| **🔌** | **REST API** | Full HTTP API with Swagger/OpenAPI documentation. Queue, history, server management, status, and log endpoints. |
| **🔁** | **SABnzbd Compatible** | Drop-in replacement for SABnzbd. Works out of the box with Sonarr, Radarr, Lidarr, and other *arr applications. |
| **📡** | **OpenTelemetry** | Built-in tracing and metrics export via OTLP. Ship logs and metrics to Grafana, Jaeger, or any OTLP-compatible backend. |
| **📥** | **SABnzbd Migration** | Import your SABnzbd configuration with one click. Upload your `sabnzbd.ini` or connect to a live instance. |

---

## Benchmarks

Head-to-head comparison with SABnzbd under identical conditions.

### 1-Hour Stress Test

Sustained load with continuous NZB submission -- 50 NNTP connections, 5 concurrent jobs.

| Metric | rustnzb | SABnzbd |
|--------|---------|---------|
| Avg speed | **5,059 Mbps** | 1,002 Mbps |
| NZBs completed / hour | **401** | 37 |
| Downloaded in 1 hour | **2,287 GB** | 455 GB |
| Memory over time | **Stable** (+2 MB/hr) | +2,412 MB/hr |

### Scenario Comparisons

| Scenario | rustnzb | SABnzbd | Improvement |
|----------|---------|---------|-------------|
| 5 GB raw download | **4.2s** @ 10,316 Mbps | 5.0s @ 8,535 Mbps | +21% faster |
| 10 GB raw download | **10.2s** @ 8,394 Mbps | 14.1s @ 6,098 Mbps | +38% faster |
| 10 GB + 7z extraction | **19.9s** | 29.2s | +47% faster |
| 5 GB + PAR2 repair | 33.5s (4.0s download) | 30.4s (9.4s download) | 2.4x faster download, 60% less memory |

All benchmarks run on the same machine using Docker containers with identical configuration. Data served by a mock NNTP server to eliminate network variability. Full methodology and raw data in `benchnzb/`.

---

## Getting Started

### Docker

```bash
docker run -d \
  --name rustnzb \
  -p 9090:9090 \
  -v ./config:/config \
  -v ./data:/data \
  -v /path/to/downloads:/downloads \
  ausagentsmith/rustnzb:latest
```

Open `http://localhost:9090` and add your NNTP servers via the web UI.

### Docker Compose

```bash
git clone https://github.com/AusAgentSmith-org/rustnzb.git
cd rustnzb
cp config.example.toml config.toml
docker compose up -d
```

### Desktop App

Native application for Windows and Linux powered by Tauri.

Download the latest installer from [GitHub Releases](https://github.com/AusAgentSmith-org/rustnzb/releases):
- **Windows** -- `.exe` (NSIS installer)
- **Linux** -- `.deb` or `.rpm`

### From Source

```bash
git clone https://github.com/AusAgentSmith-org/rustnzb.git
cd rustnzb
cp config.example.toml config.toml
cargo build --release
./target/release/rustnzb
```

Requirements: Rust 1.88+ (2024 edition), `7z` for archive extraction.

---

## Sonarr & Radarr Integration

rustnzb is a drop-in replacement for SABnzbd -- use the **SABnzbd** download client type in your *arr apps.

1. In Sonarr/Radarr, go to **Settings > Download Clients > Add** and select **SABnzbd**
2. Set **Host** and **Port** to your rustnzb instance
3. Enter your `api_key` (if configured in `config.toml`)
4. Set **Category** to match your rustnzb categories (e.g. `tv`, `movies`)

```toml
[general]
api_key = "your-secret-key"

[[categories]]
name = "tv"
output_dir = "tv"

[[categories]]
name = "movies"
output_dir = "movies"
```

Works with Sonarr, Radarr, Lidarr, Readarr, and Prowlarr.

---

## Architecture

A modular Rust workspace with clean separation of concerns.

| Crate | Purpose |
|-------|---------|
| **nzb-core** | NZB parser, config, SQLite database, shared models |
| **nzb-news** | Download orchestration primitives and queue/worker coordination |
| **nzb-dispatch** | Server-aware dispatch engine that feeds article fetch work to `nzb-news` |
| **nzb-nntp** | NNTP protocol, connection pool, TLS (rustls), pipelining, server failover |
| **nzb-decode** | yEnc decoder, CRC32 validation, file assembler |
| **nzb-postproc** | PAR2 verify & repair, RAR/7z/ZIP extraction, cleanup |
| **nzb-web** | Axum HTTP server, REST API, web UI, queue manager, SABnzbd compat |

### Download Pipeline

```
Parse NZB
  -> nzb-web queue manager
  -> nzb-dispatch
  -> nzb-news
  -> Download (nzb-nntp pipelining, multi-server failover)
  -> Decode (yEnc + CRC32)
  -> Verify & Repair (PAR2)
  -> Extract (RAR, 7z, ZIP)
  -> Complete
```

---

## Configuration

rustnzb uses TOML configuration with CLI and environment variable overrides.

**Priority:** CLI args > environment variables > TOML file > defaults

Most settings can be configured through the web UI. See [`config.example.toml`](config.example.toml) for the full reference.

### Key Environment Variables

| Variable | Description |
|----------|-------------|
| `RUSTNZB_CONFIG` | Config file path |
| `RUSTNZB_PORT` | Listen port |
| `RUSTNZB_LOG_LEVEL` | Log level (trace/debug/info/warn/error) |
| `OTEL_ENABLED` | Enable OpenTelemetry |
| `OTEL_EXPORTER_OTLP_ENDPOINT` | OTLP gRPC endpoint |

### Docker Volumes

| Path | Purpose |
|------|---------|
| `/config` | Configuration files |
| `/data` | Database, RSS state, credentials |
| `/downloads` | Incomplete and completed downloads |

---

## API

Interactive API documentation is available at `/swagger-ui` when the server is running.

```bash
# Add NZB by URL
curl -X POST http://localhost:9090/api/queue/add-url \
  -H "Content-Type: application/json" \
  -d '{"url": "https://example.com/file.nzb", "category": "movies"}'

# Upload NZB file
curl -X POST http://localhost:9090/api/queue/add \
  -F "file=@/path/to/file.nzb" -F "category=tv"

# Check status
curl http://localhost:9090/api/status
```

The SABnzbd-compatible API is available at `/sabnzbd/api`.

---

## Development

```bash
cargo build              # Debug build
cargo build --release    # Release build
cargo test --workspace   # All tests
cargo test -p nzb-decode # Single crate
```

---

## License

MIT

---

Also by [AusAgentSmith](https://github.com/AusAgentSmith-org): [Indexarr](https://indexarr.net) | [rustTorrent](https://rusttorrent.dev)
