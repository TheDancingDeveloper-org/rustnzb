# rustnzb

**A modern Usenet downloader written in Rust.**

rustnzb takes an NZB and does the rest — pipelined NNTP downloads, SIMD yEnc
decoding, PAR2 verification and repair, and archive extraction, wrapped in a
web UI with real automation. One static binary, built for self-hosters.

[![Rust](https://img.shields.io/badge/Rust-2024_edition-orange)](https://www.rust-lang.org/)
[![Container](https://img.shields.io/badge/container-GHCR-blue)](https://github.com/TheDancingDeveloper-org/rustnzb/pkgs/container/rustnzbd)
[![License: MIT](https://img.shields.io/badge/License-MIT-green.svg)](LICENSE)

## Try it now — live demo

**[www.rustnzb.dev/demo](https://www.rustnzb.dev/demo/)**

The demo is the complete rustnzb interface running against a simulated backend
in your browser — queue, history, statistics, newsgroup search, RSS rules, the
media library, settings, all of it. Nothing to install, no Usenet provider
required, and nothing leaves the page.

More at [rustnzb.dev](https://rustnzb.dev/) · [GitHub Releases](https://github.com/TheDancingDeveloper-org/rustnzb/releases) · [Discord](https://discord.gg/pu6chSqpnJ)

---

## Features

| Feature | Description |
|---------|-------------|
| **NNTP pipelining** | Multiple ARTICLE commands in flight per connection. Configurable pipeline depth per server eliminates round-trip latency. |
| **Multi-server failover** | Priority-ordered server list over TLS (rustls). Articles missing on one server are retried on the next; fill servers are only used when needed. |
| **yEnc decoding** | SIMD-accelerated yEnc decoder with CRC32 validation, streamed straight into the file assembler. |
| **PAR2 verify & repair** | Automatic verification after download. Damaged files are rebuilt from recovery blocks in pure Rust — no external par2 binary. |
| **Archive extraction** | Automatic extraction of RAR, 7z, and ZIP archives after repair, multi-part sets included, with cleanup. |
| **Web UI** | Queue, history with per-download insights, lifetime statistics, live logs, drag-and-drop NZB upload, and selectable themes. |
| **Automation** | RSS feeds with regex rules, a watch folder, and a newsgroup browser with header search and threaded views. |
| **Media library** | WebDAV library that streams files on demand, straight from Usenet, without downloading first. |
| **REST API** | Full HTTP API with Swagger/OpenAPI documentation, plus a compatibility API for *arr applications. |
| **Desktop app** | Native application for Windows, macOS, and Linux powered by Tauri. System tray with queue count and speed. |
| **OpenTelemetry** | Built-in tracing and metrics export via OTLP. Ship logs and metrics to Grafana, Jaeger, or any OTLP-compatible backend. |

---

## Getting started

### Docker

```bash
docker run -d \
  --name rustnzb \
  -p 9090:9090 \
  -v ./config:/config \
  -v ./data:/data \
  -v /path/to/downloads:/downloads \
  ghcr.io/thedancingdeveloper-org/rustnzbd:latest
```

Open `http://localhost:9090` and add your NNTP servers via the web UI.

### Docker Compose

```bash
git clone https://github.com/TheDancingDeveloper-org/rustnzb.git
cd rustnzb
cp apps/rustnzb/config.example.toml config.toml
docker compose up -d
```

### Binaries

Download the latest release from [GitHub Releases](https://github.com/TheDancingDeveloper-org/rustnzb/releases):

- **Linux** — `tar.gz` (x86_64, aarch64) or `.deb` (amd64, arm64, installs a systemd service)
- **Windows** — NSIS installer
- SHA256 checksums are attached to every release

### From source

```bash
git clone https://github.com/TheDancingDeveloper-org/rustnzb.git
cd rustnzb
cp apps/rustnzb/config.example.toml config.toml
cargo build -p rustnzb --release
./target/release/rustnzb
```

Requirements: Rust 1.88+ (2024 edition), `7z` for archive extraction.

---

## Sonarr & Radarr integration

rustnzb speaks the download-client API that *arr applications already use —
Sonarr, Radarr, Lidarr, Readarr, and Prowlarr connect with their standard
settings.

1. In your *arr app, go to **Settings > Download Clients > Add** and select the **SABnzbd** client type
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
| **nzb-web** | Axum HTTP server, REST API, web UI, queue manager |

### Download pipeline

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

Most settings can be configured through the web UI. See
[`apps/rustnzb/config.example.toml`](apps/rustnzb/config.example.toml) for the
full reference.

### Key environment variables

| Variable | Description |
|----------|-------------|
| `RUSTNZB_CONFIG` | Config file path |
| `RUSTNZB_PORT` | Listen port |
| `RUSTNZB_LOG_LEVEL` | Log level (trace/debug/info/warn/error) |
| `RUSTNZB_DAV_ENABLED` | Enable the Media Library (DAV) at startup |
| `OTEL_ENABLED` | Toggle for OTEL logs + metrics export |
| `OTEL_EXPORTER_OTLP_ENDPOINT` | OTLP gRPC endpoint |
| `OTEL_SERVICE_NAME` | Service name for telemetry |

### Docker volumes

| Path | Purpose |
|------|---------|
| `/config` | Configuration files |
| `/data` | Database, RSS state, credentials |
| `/downloads` | Incomplete and completed downloads |

---

## API

Interactive API documentation is available at `/swagger-ui` when the server is
running.

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

The *arr-compatible API is served at `/sabnzbd/api`.

---

## Development

```bash
cargo build -p rustnzb              # Debug build
cargo build -p rustnzb --release    # Release build
cargo test --workspace              # All tests
cargo test -p nzb-decode            # Single crate
```

For exact CI parity, use the checked-in container task interface:

```bash
./ci/run fmt
./ci/run check
./ci/run test
./ci/run clippy
./ci/run e2e
```

See [docs/DEVELOPMENT.md](docs/DEVELOPMENT.md) for the full task list and
build details. See [CONTRIBUTING.md](CONTRIBUTING.md) for repository workflow
and [docs/KNOWN_ISSUES.md](docs/KNOWN_ISSUES.md) for current user-visible
limitations.

---

## License

MIT

---

## Contributing and security

Contributions are welcome. Please read [CONTRIBUTING.md](CONTRIBUTING.md),
[SECURITY.md](SECURITY.md), and [CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md) before
opening an issue or pull request.
