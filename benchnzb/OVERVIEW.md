# benchnzb

Benchmark and stress-test suite for rustnzb. Two modes:

- **v1** — Comparison benchmark: SABnzbd vs rustnzb across fixed scenarios
- **v2** — Stress test: long-duration soak test of rustnzb to detect performance degradation

Both modes run entirely in Docker — a mock/synthetic NNTP server, the download client(s), and an orchestrator that collects metrics and generates reports.

## Quick Start

```bash
# v1: Compare SABnzbd vs rustnzb (5 GB raw download, ~5 min)
./run.sh --scenarios quick

# v2: Stress test rustnzb for 1 hour
./run-stress.sh --duration 1h

# v2: 4-hour soak test with larger NZBs and more connections
./run-stress.sh --duration 4h --nzb-size 10gb --concurrency 10 --connections 100
```

Results land in `results/` — JSON, CSV, summary text, a self-contained HTML
report, and SVG charts. Result artifacts are intentionally local and are not
committed; publish a report only with its complete machine and scenario
metadata.

---

## v1: Comparison Benchmark

Runs SABnzbd and rustnzb sequentially against the same NZB, using a file-backed mock NNTP server. Measures download speed, post-processing time, CPU, memory, network, and disk I/O.

### How it works

1. `run.sh` seeds configs, launches Docker Compose (4 containers)
2. Mock NNTP server starts on port 119, serving yEnc-encoded articles from memory-mapped data files
3. Orchestrator generates random test data (raw binary + par2 + 7z archives + NZB files)
4. For each scenario: runs SABnzbd first, then rustnzb, collecting Docker stats throughout
5. Outputs comparison reports with side-by-side metrics and SVG charts

### Scenarios

| Name | Size | What it tests |
|------|------|---------------|
| sz5gb_raw | 5 GB | Pure NNTP download speed |
| sz10gb_raw | 10 GB | Pure NNTP download speed |
| sz50gb_raw | 50 GB | Pure NNTP download speed |
| sz5gb_par2 | 5 GB | Download + par2 repair (3% missing) |
| sz10gb_par2 | 10 GB | Download + par2 repair (3% missing) |
| sz50gb_par2 | 50 GB | Download + par2 repair (3% missing) |
| sz5gb_unpack | 5 GB | Download + 7z extraction |
| sz10gb_unpack | 10 GB | Download + 7z extraction |
| sz50gb_unpack | 50 GB | Download + 7z extraction |

`verify` is a separate 32 MiB generated archive fixture for regression and
local correctness verification. It checks the extracted payload checksum and
records fixture decoded/yEnc bytes, article requests, injected 430 faults,
terminal outcome, CPU/RSS,
and peak incomplete/complete directory occupancy.

`verify-fault` is the paired 32 MiB PAR2 case with controlled missing
articles. Its fault counters distinguish repair/retry traffic from the
healthy-path result.

Run both correctness legs together with:

```bash
./run.sh --scenarios verify,verify-fault
```

This establishes the fixture and repair contract, not a representative
throughput comparison. Run large scenarios on the intended higher-capacity
benchmark host before publishing a report.

The compose service deliberately uses the image's normal s6 service lifecycle
instead of starting a second rustnzb process, so the fixture exercises the
same database ownership model as the runtime image.

### Scenario Groups

| Flag | Scenarios | Approx Time |
|------|-----------|-------------|
| `quick` | 5 GB raw only | ~5 min |
| `medium` | 5+10 GB, all types | ~30 min |
| `speed` | 5+10+50 GB, raw only | ~60 min |
| `postproc` | 5+10 GB, par2+unpack | ~45 min |
| `full` | All 9 scenarios | ~3-4 hours |

### Docker Stack (v1)

| Container | Role |
|-----------|------|
| `mock-nntp` | File-backed NNTP server (mmap + on-the-fly yEnc) |
| `sabnzbd` | SABnzbd download client |
| `rustnzb` | rustnzb download client |
| `orchestrator` | Data generation, test execution, metrics, reporting |

---

## v2: Stress Test

Runs rustnzb under sustained load for a configurable duration. No SABnzbd, no par2, no comparison — purely focused on finding performance degradation over time: memory leaks, speed drops, connection pool exhaustion, etc.

### How it works

1. `run-stress.sh` seeds config, launches Docker Compose (3 containers)
2. Synthetic NNTP server generates articles on-the-fly from message-IDs — no data files, no disk, unlimited articles
3. Stress runner continuously feeds NZBs to rustnzb, keeping a configurable number always queued
4. Three concurrent background tasks:
   - **Feeder** — generates NZBs in-memory, submits via API, maintains queue depth
   - **Metrics** — samples rustnzb API (speed, queue) + Docker stats (CPU, memory, net, disk)
   - **Cleanup** — deletes completed downloads and clears history to prevent disk/memory growth
5. After the duration expires, generates reports with windowed analysis and degradation detection

### Synthetic NNTP Server

Unlike v1's file-backed mock, the v2 synthetic server needs zero pre-generated data:

- Accepts any message-ID matching `stress-{size}-f{idx}-p{part}@benchnzb`
- Generates deterministic data from the message-ID using a fast xorshift fill
- yEnc encodes on-the-fly with correct headers, CRC32, and segment boundaries
- Serves unlimited articles with zero disk usage

### Degradation Analysis

The run is divided into 5-minute windows. For each window, the runner computes average speed, CPU, and memory. A linear regression over all windows determines:

- **Speed trend** (%/hour) — negative means slowing down
- **Memory trend** (MB/hour) — positive means growing (possible leak)

Degradation is flagged if speed drops >5%/hour or memory grows >100MB/hour.

### Options

| Flag | Default | Description |
|------|---------|-------------|
| `--duration` | `1h` | How long to run (e.g. `30m`, `4h`, `8h`) |
| `--nzb-size` | `5gb` | Size of each NZB download (e.g. `500mb`, `10gb`) |
| `--concurrency` | `5` | Number of NZBs to keep queued |
| `--connections` | `50` | NNTP connections for rustnzb |
| `--no-cleanup` | — | Keep Docker containers after run |

### Docker Stack (v2)

| Container | Role |
|-----------|------|
| `synth-nntp` | Synthetic NNTP server (stateless, generates data on-the-fly) |
| `rustnzb` | rustnzb download client (stress-optimized config) |
| `stress-runner` | Feeder, metrics collector, cleanup, reporting |

---

## Output

Both modes write to `results/`:

### v1 Output
```
results/
  benchmark_YYYYMMDD_HHMMSS.json     # Full metrics + timeseries
  benchmark_YYYYMMDD_HHMMSS.csv      # Tabular comparison
  benchmark_YYYYMMDD_HHMMSS.html     # Self-contained measured report
  summary_YYYYMMDD_HHMMSS.txt        # Human-readable side-by-side
  charts_YYYYMMDD_HHMMSS/            # SVG charts (comparison bars, timeseries, dashboard)
  logs_YYYYMMDD_HHMMSS/              # Per-scenario container logs
```

### v2 Output
```
results/
  stress_YYYYMMDD_HHMMSS.json        # Full metrics + timeseries + degradation analysis
  stress_YYYYMMDD_HHMMSS.csv         # Per-window stats (one row per 5-minute window)
  stress_summary_YYYYMMDD_HHMMSS.txt # Human-readable summary with verdict
  stress_charts_YYYYMMDD_HHMMSS/     # SVG charts (timeseries with trends, window bars, dashboard)
  stress_rustnzb_YYYYMMDD_HHMMSS.log # rustnzb container logs
```

---

## Project Structure

```
benchnzb/
├── Cargo.toml
├── Dockerfile                  # Rust build + par2/p7zip runtime
├── docker-compose.yml          # v1: mock-nntp, sabnzbd, rustnzb, orchestrator
├── docker-compose.stress.yml   # v2: synth-nntp, rustnzb, stress-runner
├── run.sh                      # v1 entry point
├── run-stress.sh               # v2 entry point
├── configs/
│   ├── sabnzbd.ini             # Pre-seeded SABnzbd config
│   ├── rustnzb.toml            # Pre-seeded rustnzb config (v1, 20 connections)
│   └── rustnzb-stress.toml     # Stress-optimized rustnzb config (v2, 50 connections, no post-proc)
└── src/
    ├── main.rs                 # CLI: run | mock-nntp | regen-charts | synth-nntp | stress
    ├── config.rs               # Scenarios, size/duration parsing
    ├── yenc.rs                 # yEnc encoder (CRC32, escaping, line wrapping)
    ├── nzb.rs                  # NZB XML generator
    ├── mock_nntp.rs            # v1: file-backed mock NNTP server (mmap + yEnc)
    ├── synth_nntp.rs           # v2: synthetic NNTP server (stateless, on-the-fly data)
    ├── datagen.rs              # v1: generates test data, par2, 7z, NZBs, article index
    ├── runner.rs               # v1: orchestrator (sequential SABnzbd/rustnzb runs)
    ├── stress.rs               # v2: stress orchestrator (feeder, metrics, cleanup, analysis)
    ├── metrics.rs              # Docker stats collection (CPU/mem/net/disk timeseries)
    ├── docker.rs               # Docker API helpers (container lookup, exec, logs)
    ├── report.rs               # v1: JSON/CSV/summary output
    ├── stress_report.rs        # v2: JSON/CSV/summary with degradation analysis
    ├── charts.rs               # v1: SVG comparison charts
    ├── stress_charts.rs        # v2: SVG timeseries with trend lines
    └── clients/
        ├── mod.rs
        ├── sabnzbd.rs          # SABnzbd API client
        └── rustnzb.rs          # rustnzb API client
```
