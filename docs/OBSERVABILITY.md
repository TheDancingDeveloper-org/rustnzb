# Observability

rustnzb exports three OpenTelemetry signals over OTLP/gRPC when `[otel]` is
configured: logs, metrics and traces. The endpoint defaults to
`http://localhost:4317`.

```toml
[otel]
enabled = true
endpoint = "http://localhost:4317"
service_name = "rustnzb"
# Optional per-signal overrides:
# logs_enabled = true
# logs_endpoint = "http://collector:4317"
# metrics_enabled = true
# metrics_endpoint = "http://collector:4317"
```

Traces follow the logs switch and use the logs endpoint, so a download's spans
and its log lines go to the same place. The resource carries `service.name`
only. There is no environment-variable override for these settings.

## Log file

`--log-file <path>` (or `RUSTNZB_LOG_FILE`, or `general.log_file` in
`config.toml`) writes logs to a file as well as stdout. The flag or variable
wins over the config value. Nothing is removed from stdout.

- The file rotates daily. The name is the path's file name with the date
  appended: `--log-file data/rustnzb.log` writes `data/rustnzb.log.2026-10-17`.
  The newest 7 daily files are kept and older ones are deleted when a new day
  starts.
- A relative path is resolved against the working directory. The parent
  directory is created if it is missing.
- Lines in the file have no ANSI colour codes.
- A path with no file name (such as `/`) or a directory that cannot be opened
  stops startup with an error.

## Debug logging toggle

Settings → General → **Debug logging**. It takes effect at once (the tracing
filter is a reload layer, so there is no restart) and is saved in
`config.toml`, so it survives a restart.

- **On** installs a filter scoped to rustnzb's own crates:
  `info,nzb_core=debug,nzb_news=debug,nzb_dispatch=debug,nzb_nntp=debug,nzb_decode=debug,nzb_postproc=debug,nzb_web=debug,rustnzb=debug`.
  Transport and exporter crates (`h2`, `hyper`, `tonic`, `reqwest`,
  `opentelemetry*`) stay at `info`, so the OTLP exporter does not log its own
  traffic into Loki. The level it replaced is saved as
  `general.log_level_before_debug`.
- **Off** restores that saved level. A custom level such as
  `warn,nzb_nntp=info` comes back as it was; it is not forced to `info`.
- The page shows the **effective level**. When `RUST_LOG`, `--log-level` or
  `RUSTNZB_LOG_LEVEL` is set, that source wins over the saved level. The
  switch is then disabled, and the page shows the override and the filter
  the process is actually running. Unset the override and restart to let the
  switch take effect.

API: `GET /api/config/debug-logging` returns
`{enabled, effective, overridden}`. `PUT` with `{"enabled": true|false}`
applies the change and returns the same fields. Both require authentication.

When the toggle is off, the extra `debug!` callsites cost nothing beyond the
filter's cached "never" decision. The spans and metrics below are emitted at
`info` whatever the toggle says. They are per job, per connection or per
10-second worker window, never per article, so their cost is negligible.

## Metrics

Meter `rustnzb`. The collector flattens dots to underscores and appends unit
suffixes, so `rustnzb.stage.duration` (unit `s`) lands in Prometheus as
`rustnzb_stage_duration_seconds_{bucket,sum,count}`.

| OTLP name | Kind | Attributes | Meaning |
|---|---|---|---|
| `download.speed_bps` | gauge | none | current download speed, bytes/s |
| `queue.depth` | gauge | none | jobs in the queue |
| `rustnzb.stage.duration` | histogram, s | `stage`, plus `server` for `connect` | time per stage, see below |
| `rustnzb.nntp.bytes` | counter, bytes | `server` | decoded article bytes received |
| `rustnzb.nntp.articles` | counter | `server` | articles received and decoded |
| `rustnzb.nntp.response_wait` | counter, s | `server` | time workers spent waiting for article responses |

`stage` values:

| stage | measures | granularity |
|---|---|---|
| `parse` | NZB parse | per job |
| `queue_wait` | time from add to dispatch (a job resumed after a restart counts from its original add time) | per job |
| `connect` | NNTP connect, TLS and authentication, including retries (the connection wait) | per worker connection, with `server` |
| `download` | whole download phase, wall time | per job |
| `decode` | yEnc decode, summed across workers | per job |
| `assemble` | writing decoded data to the files, summed across workers | per job |
| `par2_verify` | PAR2 verify and repair | per job |
| `extract` | archive extraction | per job |
| `cleanup` | removal of archives and par2 files | per job |

`decode` and `assemble` are sums across workers, so they can exceed the
download's wall time.

The NNTP counters are added from each worker's 10-second perf summary. A
window still open when a worker stops is not exported.

## Traces

Spans come from `tracing` and are exported through `tracing-opentelemetry` to
the same endpoint as the logs.

| span | root? | fields |
|---|---|---|
| `nzb_parse` | yes | `name`, `bytes` |
| `download` | yes, opened when the job is dispatched, closed when it finishes or aborts | `job_id`, `job_name`, then `articles_decoded`, `total_bytes`, `decode_secs`, `assemble_secs`, `active_secs` |
| `nntp_connect` | yes | `server`, `worker` |
| `postproc` | yes | `job_id`, `pp_level` |
| `par2_verify` | child of `postproc` | `recovery_sets` |
| `extract` | child of `postproc` | |
| `cleanup` | child of `postproc` | |

Log events emitted inside a span carry its trace and span IDs. The workers
share a connection pool across jobs, so `nntp_connect` is not a child of any
job's `download` span. Search by `job_id` to tie a job's spans together.

## Where it lands in Grafana (Node B `grafanaloki` stack)

The `otel-collector` listens on `:14317` (gRPC) and `:14318` (HTTP).

- **Logs → Loki.** `{service_name="rustnzb"}`. `scope_name` is the tracing
  target, e.g. `nzb_web::queue_manager`. Filter one job with
  `{service_name="rustnzb"} | job_id="<id>"`.
- **Metrics → Prometheus**, through the collector's remote write. They are
  not scraped. Examples:
  - Mean stage time:
    `sum by (stage) (rate(rustnzb_stage_duration_seconds_sum[15m])) / sum by (stage) (rate(rustnzb_stage_duration_seconds_count[15m]))`
  - Per-server throughput: `sum by (server) (rate(rustnzb_nntp_bytes_total[5m]))`
  - Per-server mean response latency:
    `rate(rustnzb_nntp_response_wait_seconds_total[5m]) / rate(rustnzb_nntp_articles_total[5m])`
  - Connection wait p95:
    `histogram_quantile(0.95, sum by (le, server) (rate(rustnzb_stage_duration_seconds_bucket{stage="connect"}[1h])))`
- **Traces → Tempo.** `{resource.service.name="rustnzb"}`, or
  `{name="download" && span.job_id="<id>"}`. This needs the collector to
  have a traces pipeline that exports to Tempo. Before this change rustnzb
  sent no traces, so that pipeline has never been exercised. Check it after
  deploying.

The exact Prometheus names depend on the collector's naming settings. Check
them with `{__name__=~"rustnzb_.*"}` after the first job runs.
