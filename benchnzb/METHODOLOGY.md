# Stress Test Methodology

## Overview

The benchnzb stress test framework measures sustained download throughput, memory stability, and degradation over time. Both RustNZB and SABnzbd are tested against the same synthetic NNTP server under identical conditions, with configurable duration, concurrency, and NZB size.

## Test Machine

| Component | Specification |
|-----------|--------------|
| CPU | 48 vCPUs (QEMU Virtual CPU, 1 socket, 48 cores, 1 thread/core) |
| RAM | 252 GB DDR |
| OS Disk | 50 GB virtual SSD |
| Data Disk | 300 GB virtual SSD (`/mnt/data`) |
| OS | Ubuntu Linux (kernel 6.17) |
| Runtime | Docker containers (no resource limits) |

## Test Environment

Both clients run as Docker containers on the same host, connected to a shared synthetic NNTP server over an internal Docker network. The synthetic server generates yEnc-encoded articles on-the-fly from a deterministic seed, eliminating disk I/O as a variable on the server side. All three containers (NNTP server, client under test, and the test orchestrator) share the host's CPU and memory with no resource limits applied.

## How It Works

The stress test runs three concurrent loops for the configured duration:

**Feeder loop** — Continuously submits NZB files to the client under test, maintaining a target queue depth (default: 5 concurrent jobs). Each NZB contains a single large file (default: 5 GB) split into 750 KB articles. This ensures the client always has work available and is never starved for input.

**Metrics loop** — Polls the client's API every 5 seconds, capturing instantaneous download speed, queue depth, active download count, and completed job count. Docker container stats (CPU, memory, network, disk I/O) are collected in parallel and merged by timestamp.

**Cleanup loop** — Periodically clears completed downloads from the client's output directory and history to prevent disk exhaustion during long runs. For SABnzbd, only completed downloads are cleaned; in-progress download directories are left intact to avoid disrupting its internal state management.

## What We Measure

| Metric | Source | Method |
|--------|--------|--------|
| Download speed | Client API | Instantaneous speed reported by the client at each 5-second poll |
| NZBs completed | Client history API | Delta-based counting across cleanup cycles |
| CPU usage | Docker stats | Container CPU percentage (can exceed 100% on multi-core) |
| Memory usage | Docker stats | Container RSS (resident set size) |
| Total bytes | Speed integration | Accumulated from speed samples (approximate) |

The benchmark starts rustnzb through its image-provided service definition;
the compose file must not override it with a second process, because both
instances would contend for the same SQLite state.
Each run stops any previous fixture before replacing bind-mounted configuration
state, preventing an older service from overwriting the next run's server
configuration during shutdown.

## Correctness fixture and fault attribution

The large scenarios are performance/soak workloads, not correctness proof.
Run `./run.sh --scenarios verify` for the small (32 MiB) deterministic
unpack fixture. It uses generated data only, verifies the extracted payload
against the fixture source SHA-256, and records a terminal outcome. A failed
or timed-out job is never reported as complete.

Run `./run.sh --scenarios verify-fault` for the equivalent 32 MiB PAR2
fixture with deterministic injected 430 responses. Its `fixture_metrics`
record proves it is a fault/repair path; do not compare its bytes or duration
with the healthy `verify` result.

For comparison runs, the mock NNTP server records decoded payload bytes,
yEnc-body bytes, article requests (including repeats), successful article
responses, and injected 430 responses.
The JSON result stores these as `fixture_metrics`; a non-zero
`article_not_found` identifies a fault/repair path and must not be compared
as a healthy-path wire-byte result. Peak disk usage is sampled from both
`/downloads/incomplete` and `/downloads/complete` while the job is running,
including post-processing.

## Analysis

Results are analyzed in two ways:

**Windowed statistics** — The test duration is divided into 5-minute windows. Each window reports average speed, CPU, memory, and completed NZBs. This smooths out per-job fluctuations and reveals trends.

**Degradation detection** — Linear regression is applied to windowed speed and memory data (excluding the first window as warmup). Speed declining more than 5% per hour or memory growing more than 100 MB per hour triggers a degradation warning.

## Configuration Parity

Both clients are configured for raw download throughput with post-processing disabled:

| Setting | RustNZB | SABnzbd |
|---------|---------|---------|
| NNTP connections | 50 | 50 |
| Post-processing | Disabled (`post_processing = 0`) | Disabled (`pp = 0`, all extract/repair flags off) |
| Speed limit | None | None |
| NNTP server | synth-nntp (shared) | synth-nntp (shared) |

## Known Differences

The following differences between clients are inherent to their architecture and are not corrected for in the test:

- **Post-processing behavior**: SABnzbd enters a brief post-processing phase after each download completes (even with pp=0), during which it pauses all active downloads. RustNZB skips post-processing entirely when disabled and starts the next download before any cleanup work.

- **Article cache**: RustNZB is configured with a 1 GB article cache. SABnzbd uses its default cache size, which is typically smaller. This may affect throughput under disk I/O pressure.

- **Speed reporting**: RustNZB reports speed in bytes per second from a 1-second rolling window. SABnzbd reports speed as `kbpersec` (converted to bytes/sec for comparison). Both are polled at the same 5-second interval.

## Reproducing the Tests

```bash
cd benchnzb

# RustNZB stress test (1 hour)
./run-stress.sh --client rustnzb --duration 1h

# SABnzbd stress test (1 hour)
./run-stress.sh --client sabnzbd --duration 1h

# Custom configuration
./run-stress.sh --client rustnzb --duration 4h --nzb-size 10gb --concurrency 10 --connections 100
```

Results are written to `results/` as JSON (full timeseries), CSV (windowed summary), a text summary, and SVG charts (timeseries, per-window bars, and dashboard).

## Limitations

- Results are host-dependent: CPU, memory, disk speed, and Docker overhead all affect throughput. Comparative results (RustNZB vs SABnzbd) on the same host are meaningful; absolute numbers will vary across hardware.
- The synthetic NNTP server generates data in memory. Real-world Usenet servers add network latency, TLS overhead, and article availability constraints that are not modeled here.
- Docker network byte counters include protocol/yEnc framing; fixture payload
  counters deliberately measure decoded payload only. Report both rather than
  treating either as an unexplained "extra bytes" figure.
- SABnzbd's `kbpersec` unit semantics (decimal KB vs binary KiB) may introduce a small (~2.4%) measurement discrepancy in reported speed.
