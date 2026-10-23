# Configuration reference

This page lists every rustnzb setting: its TOML key, its default, and the
environment variable or command-line flag that overrides it. The annotated
example file is [`apps/rustnzb/config.example.toml`](../apps/rustnzb/config.example.toml).

Most settings can also be changed in the web UI (**Settings**), which saves
them to the same `config.toml`.

## Where the config lives

| Install | Config file |
|---------|-------------|
| Container image | `/config/config.toml`, created empty on first start (an empty file means "all defaults") |
| Binary, no `--config` | `$XDG_CONFIG_HOME/rustnzb/config.toml`, or `~/.config/rustnzb/config.toml`, created on first start |
| Binary with `--config <path>` / `RUSTNZB_CONFIG` | That path, which must already exist |

rustnzb writes the file with owner-only permissions (0600), because it holds
server passwords and API keys.

## Precedence

Highest first:

1. Command-line flag
2. Environment variable
3. `config.toml`
4. Built-in default

A value from a flag or an environment variable applies only while the process
runs, and is never written to `config.toml`. The web UI shows such a setting
as locked and names its source (`cli`, `env:NAME`). Remove the flag or
variable to edit the setting again.

Unknown keys are an error. The message names the file and the key. To check a
file without starting the server:

```bash
rustnzb --config /path/to/config.toml --check-config
```

It prints the result and exits 0 if the file is usable, 1 if not.

## Command-line flags and environment variables

| Flag | Environment variable | Overrides | Notes |
|------|----------------------|-----------|-------|
| `-c`, `--config <path>` | `RUSTNZB_CONFIG` | — | Config file path. A path given here must exist. |
| `--check-config` | — | — | Validate the config, print the result and exit. |
| `--listen-addr <addr>` | `RUSTNZB_LISTEN_ADDR` | `general.listen_addr` | |
| `-p`, `--port <port>` | `RUSTNZB_PORT` | `general.port` | In the container, also remap the host port (see the README). |
| `--data-dir <path>` | `RUSTNZB_DATA_DIR` | `general.data_dir` | The container sets `--data-dir /data`. |
| `--log-level <level>` | `RUSTNZB_LOG_LEVEL` | `general.log_level` | `RUST_LOG`, when set, overrides all three with a full filter. |
| `--log-file <path>` | `RUSTNZB_LOG_FILE` | `general.log_file` | Daily rotation, newest 7 files kept. |
| `--log-format <text\|json>` | `RUSTNZB_LOG_FORMAT` | — | Default `text`. `json` writes one JSON object per line. |
| `--smoke-test` | — | — | Check that `7z` (and `unrar`, if present) work, then exit. |
| — | `OTEL_ENABLED` | `otel.enabled` | Boolean (see below). |
| — | `OTEL_EXPORTER_OTLP_ENDPOINT` | `otel.endpoint` | |
| — | `OTEL_LOGS_ENABLED` | `otel.logs_enabled` | Boolean. |
| — | `OTEL_EXPORTER_OTLP_LOGS_ENDPOINT` | `otel.logs_endpoint` | |
| — | `OTEL_METRICS_ENABLED` | `otel.metrics_enabled` | Boolean. |
| — | `OTEL_EXPORTER_OTLP_METRICS_ENDPOINT` | `otel.metrics_endpoint` | |
| — | `OTEL_SERVICE_NAME` | `otel.service_name` | |
| — | `RUSTNZB_DAV_ENABLED` (alias `ENABLE_DAV`) | `dav.enabled` | Can only turn DAV **on**. A false value leaves the file's setting alone. |

Boolean variables accept `1`, `true`, `yes` or `on`, in any case. Any other
value is false.

### Container-only variables

The container image (built on the linuxserver.io Alpine base) also reads:

| Variable | Default | Meaning |
|----------|---------|---------|
| `PUID` | `1000` in the example compose file, `911` in the base image | User ID that owns `/config`, `/data` and the download directories, and runs rustnzb |
| `PGID` | `1000` in the example compose file, `911` in the base image | Group ID for the same |
| `TZ` | `UTC` | Timezone for log timestamps and schedules |

## `[general]`

| Key | Default | Description |
|-----|---------|-------------|
| `listen_addr` | `"0.0.0.0"` | HTTP listen address. |
| `port` | `9090` | HTTP port. |
| `api_key` | unset | API key. The SABnzbd-compatible API (`/api`, `/sabnzbd/api`) takes it as `apikey` in the query string, the body, or an `X-Api-Key` header; the header keeps it out of request URLs and therefore out of reverse-proxy access logs, so prefer it. The native API accepts the same `X-Api-Key` header. Settings → API & SABnzbd compat can show and rotate it. |
| `incomplete_dir` | `"/downloads/incomplete"` | Where jobs download. |
| `complete_dir` | `"/downloads/complete"` | Where finished jobs go. Category `output_dir` is relative to this. |
| `data_dir` | `"/data"` | Database, RSS state, credentials. |
| `speed_limit_bps` | `0` | Download limit in bytes per second. `0` is unlimited. |
| `cache_size` | `524288000` (500 MB) | Article cache in bytes. The API accepts 16 MiB to 64 GiB. |
| `log_level` | `"info"` | `trace`, `debug`, `info`, `warn`, `error`, `off`, or a full filter such as `info,nzb_nntp=debug`. Applied at start. |
| `log_file` | unset | Also log to this file. Daily rotation, newest 7 kept. Applied at start. |
| `history_retention` | unset (keep all) | Number of history entries to keep. `0` also keeps all. |
| `keep_history_on_arr_delete` | `true` | When Sonarr, Radarr or another *arr removes a job through the SABnzbd API, delete its files as asked but keep the History row, marked "removed by <client>". The *arrs no longer see it. `false` removes the row too, as SABnzbd does. |
| `max_active_downloads` | `1` | Jobs downloading at once. `0` is unlimited. The API accepts at most 100. |
| `max_post_processing_jobs` | `2` | Jobs in post-processing at once. Applied at start. |
| `max_repair_workers` | `1` | Concurrent PAR2 verify/repair workers. Applied at start. |
| `max_extract_workers` | `1` | Concurrent extraction workers. Applied at start. |
| `min_free_space_bytes` | `1073741824` (1 GB) | Pause downloads when free space drops below this. |
| `watch_dir` | unset | Folder watched for `.nzb` files to queue. |
| `rss_history_limit` | `500` | RSS feed items kept. Unset keeps all. |
| `rss_downloaded_item_expiry_days` | unset | Remove downloaded RSS records after this many days. Values above 36500 (100 years) are clamped. |
| `direct_unpack` | `true` | Extract RAR volumes during the download. Needs `unrar`. |
| `max_nested_archive_depth` | `5` | Archive layers extracted after the outer one. `0` extracts only the outer archive. The API accepts 0 to 10. |
| `abort_hopeless` | `true` | Abort jobs that cannot complete because too many articles are missing. Applied at start. |
| `early_failure_check` | `true` | Abort an NZB early if most of its first articles are missing. Needs `abort_hopeless`. Applied at start. |
| `required_completion_pct` | `100.2` | Minimum completion, counting PAR2 recovery, to keep downloading. Values above 100 keep a repair margin. Range 100.0 to 200.0. Applied at start. |
| `article_timeout_secs` | `30` | Wait for one article response before reconnecting. `0` disables the timeout. The API accepts at most 3600. Applied at start. |
| `auto_sort_remaining_pct` | `false` | Keep queued jobs ordered by remaining work. |
| `duplicate_policy` | `"warn"` | When an NZB matches a queued or history job by name or content: `allow`, `warn` (log it) or `skip` (refuse it). |
| `local_import_dir` | unset | The only directory SABnzbd `addlocalfile` may read. Unset refuses the mode. It also needs an API key. |
| `queue_complete_pers` | `false` | Keep the SABnzbd `change_complete_action` after it runs once. SABnzbd's default is to reset it. |
| `scripts_dir` | unset | Directory of post-processing scripts. |
| `script_success` | unset | Script run after a successful job. |
| `script_failure` | unset | Script run after a failed job. |
| `script_timeout_secs` | `300` | Maximum script run time. |
| `script_max_output_bytes` | `1048576` | Script output kept. |
| `fetch_allow_private` | `false` | Let URL fetches (add-url, SABnzbd `addurl`, RSS, SABnzbd import) reach private LAN addresses. Loopback and link-local addresses stay blocked. |
| `fetch_allowed_hosts` | `[]` | Hosts that URL fetches may always reach: IP addresses, CIDR blocks or exact hostnames, such as `["prowlarr", "192.168.1.0/24"]`. |
| `trusted_proxies` | `[]` | Reverse proxies whose `X-Forwarded-For` header is believed: IP addresses or CIDR blocks. Wrong web-login passwords are delayed per client address (see [Login brute-force protection](#login-brute-force-protection)). Behind a reverse proxy every request arrives from the proxy, so all clients share one failure count until the proxy is listed here. For a reverse-proxied install (for example rustnzb behind Caddy), set it to the proxy's address or its Docker network, such as `["172.18.0.0/16"]`. The client is the rightmost `X-Forwarded-For` address that is not itself a trusted proxy. Leave it empty when rustnzb is reached directly; a listed address can then claim any client address. Read on every login, so a change needs no restart. |
| `group_max_headers` | `100000` | Headers kept per newsgroup, oldest pruned first. `0` keeps all. |
| `shutdown_timeout_secs` | `30` | Wait for connections and downloads on SIGINT/SIGTERM before exiting. `0` waits forever. |

Scripts run without a shell, with SABnzbd's positional arguments (final
directory, NZB name, job name, report number, category, group, status 0-3,
URL) and `SAB_*` / `NZBPP_*` environment variables, described below.

### Login brute-force protection

A wrong web-login password (on `/api/auth/login`, HTTP Basic auth or
`/api/auth/change_credentials`) is answered after a delay: 0.5 s for a
client's first wrong password, doubling with each further one up to 30 s. A
client is an IPv4 address or an IPv6 /64. The count is forgotten after 15
minutes without a failure, or on a correct password. A correct password is
never refused or delayed; it waits only for one of the three password-hashing
slots, first come, first served. Each failure is logged at `warn` as
`Failed web login from <address>`.

This slows guessing from one address. It does not stop guessing spread over
many addresses, and it does not cap concurrent connections. Stop those at the
edge. Behind Caddy, set `trusted_proxies` and rate-limit the auth endpoints
with the [caddy-ratelimit](https://github.com/mholt/caddy-ratelimit) module,
for example:

```caddyfile
rate_limit {
	zone rustnzb_login {
		match {
			path /api/auth/*
		}
		key {remote_host}
		events 10
		window 1m
	}
}
```

Or ban repeat offenders with fail2ban, reading the rustnzb log:

```ini
# /etc/fail2ban/filter.d/rustnzb.conf
[Definition]
failregex = Failed web login from <HOST>
```

With HTTP Basic auth, an *arr or script that has a stale password also logs
failures, so set the ban threshold above what a misconfigured client produces
before someone notices it.

### Script environment

Two contracts are sent at once, so a script written for either downloader
runs unchanged. Every name SABnzbd attaches to the job is always present
here, empty when rustnzb has no truthful value, because a missing key raises
`KeyError` in a Python script.

Four of SABnzbd's global names are deliberately not set, because rustnzb has
nothing truthful to put in them: `SAB_VERSION`, `SAB_PROGRAM_DIR`,
`SAB_API_URL` and `SAB_API_KEY`. Read them with a default
(`os.environ.get("SAB_VERSION")`) rather than by index. The API key is left
out on purpose: a script's environment is visible in `/proc`, and a script has
no need for the key to work on the files it was given.

SABnzbd's names:

| Variable | Value |
|----------|-------|
| `SAB_SCRIPT` | File name of the script being run. |
| `SAB_NZO_ID` | Job id, the same id the API uses. |
| `SAB_FINAL_NAME`, `SAB_FILENAME`, `SAB_JOB` | The job's name. |
| `SAB_COMPLETE_DIR`, `SAB_COMPLETE` | The job's final directory. |
| `SAB_CAT` | Category, empty when the job has none. |
| `SAB_STATUS` | `Completed` or `Failed`, the job's outcome. (SABnzbd sends `Running` while the script itself runs; rustnzb keeps its existing outcome value.) |
| `SAB_FAIL_MSG` | The failure reason, empty on success. |
| `SAB_BYTES` | Total NZB size in bytes. |
| `SAB_BYTES_DOWNLOADED` | Bytes downloaded successfully. |
| `SAB_BYTES_TRIED` | Bytes attempted; rustnzb attempts the whole NZB, so this is the total size. |
| `SAB_PP_STATUS` | SABnzbd's code: 0 verified and unpacked, 1 verify/repair failed, 2 unpack failed, 3 both. |
| `SAB_PP` | Post-processing level: 0 none, 1 repair, 2 unpack, 3 both. |
| `SAB_REPAIR`, `SAB_UNPACK` | `1` when that level includes the step, else `0`. |
| `SAB_PRIORITY` | SABnzbd's numbering: Low `-1`, Normal `0`, High `1`, Force `2`. |
| `SAB_GROUP` | First newsgroup of the first file, empty when unknown. |
| `SAB_PASSWORD` | The job's password, empty when it has none. |
| `SAB_CORRECT_PASSWORD` | The password when the job succeeded, else empty. |
| `SAB_FILES` | JSON array of files produced, relative to `SAB_COMPLETE_DIR`. |
| `SAB_PAR2_COMMAND` | Empty: rustnzb verifies and repairs PAR2 itself. |
| `SAB_RAR_COMMAND`, `SAB_7ZIP_COMMAND` | `unrar` and 7-Zip paths, empty when not installed. |

The remaining SABnzbd names are declared empty (or `0` for the flags) so a
script that reads them does not crash: `SAB_URL`, `SAB_FAILURE_URL`,
`SAB_AGE`, `SAB_AVG_BPS`, `SAB_DOWNLOAD_TIME`, `SAB_ORIG_NZB_GZ`,
`SAB_DUPLICATE`, `SAB_DUPLICATE_KEY`, `SAB_UNWANTED_EXT`, `SAB_OVERSIZED`
and `SAB_ENCRYPTED`.

NZBGet's names, including its numeric status codes:

| Variable | Value |
|----------|-------|
| `NZBPP_NZBID` | Job id. |
| `NZBPP_NZBNAME`, `NZBPP_NZBFILENAME` | The job's name. |
| `NZBPP_DIRECTORY`, `NZBPP_FINALDIR` | The job's final directory. |
| `NZBPP_CATEGORY` | Category, empty when the job has none. |
| `NZBPP_TOTALSTATUS` | `SUCCESS` or `FAILURE`. |
| `NZBPP_STATUS` | `<TOTALSTATUS>/<detail>`, where detail is `ALL` on success, `PAR` for a verify or repair failure, `UNPACK` for an extraction failure, and `HEALTH` otherwise. |
| `NZBPP_SCRIPTSTATUS` | `NONE`: the script is running, so there is no result yet. |
| `NZBPP_TOTALSIZE` | Total NZB size in bytes. |
| `NZBPP_PARSTATUS` | 0 not checked, 1 checked and failed, 2 checked successfully. |
| `NZBPP_UNPACKSTATUS` | 0 not run, 1 failed, 2 succeeded. NZBGet's 3 (write error) and 4 (wrong password) are not distinguishable, so a failure is always 1. |

`RUSTNZB_STATUS` repeats `SAB_STATUS` for scripts already using it. A
SABnzbd script that needs a name rustnzb does not track reads an empty
string rather than failing.

## `[[servers]]`

One table per NNTP server. Only `host` is required.

| Key | Default | Description |
|-----|---------|-------------|
| `id` | derived from host and port | Stable identifier. |
| `name` | `""` | Display name. |
| `host` | — | Server hostname. |
| `port` | `563` | Server port. |
| `ssl` | `true` | Use TLS. |
| `ssl_verify` | `true` | Verify the TLS certificate. |
| `trusted_fingerprint` | unset | SHA-256 fingerprint of the server certificate. When set, only this fingerprint is checked, and `ssl_verify` is ignored. Use it for self-signed certificates. |
| `username`, `password` | unset | Credentials. |
| `connections` | `8` | Maximum connections. |
| `priority` | `0` | `0` is tried first. Higher numbers fill in missing articles. |
| `enabled` | `true` | |
| `optional` | `false` | If true, a failure on this server is not fatal. |
| `retention` | `0` | Retention in days. `0` is unlimited. |
| `pipelining` | `4` | Article requests in flight per connection. |
| `compress` | `false` | Negotiate `XFEATURE COMPRESS GZIP`. |
| `ramp_up_delay_ms` | `50` | Delay between opening connections, to avoid rate limits. |
| `recv_buffer_size` | `2097152` (2 MiB) | TCP receive buffer. `0` uses the OS default. |
| `connect_timeout_secs` | `30` | Time allowed to connect, negotiate TLS, read the banner and log in. |
| `proxy_url` | unset | SOCKS5 proxy: `socks5://[user:pass@]host:port`. |

## `[[categories]]`

| Key | Default | Description |
|-----|---------|-------------|
| `name` | — | Category name. One path component. |
| `output_dir` | unset | Output directory: absolute, or relative to `complete_dir`. Unset uses `complete_dir`. |
| `post_processing` | `3` | `0` none, `1` repair, `2` unpack, `3` repair and unpack. |
| `cleanup_patterns` | `[]` | Globs removed after unpacking, such as `["*.nfo", "sample/*"]`. |
| `unwanted_extensions` | `[]` | Extensions removed after unpacking, with or without the dot. |

A fresh config has one category, `Default`.

## `[[rss_feeds]]`

| Key | Default | Description |
|-----|---------|-------------|
| `name` | — | Display name. |
| `url` | — | RSS 2.0 or Atom feed URL. |
| `poll_interval_secs` | `900` | Poll interval. Minimum 60. |
| `category` | unset | Category for NZBs from this feed. |
| `filter_regex` | unset | Rust `regex` matched against titles, such as `(?i)ubuntu\|debian`. |
| `enabled` | `true` | |
| `auto_download` | `false` | Queue every item that passes `filter_regex`, without a rule. |
| `max_age_days` | unset | Ignore items older than this. |

## `[schedule]`

Time-of-day rules for pausing downloads, capping the speed and taking news
servers out of rotation. Edit it in Settings → Scheduler or write it here.

| Key | Default | Description |
|-----|---------|-------------|
| `enabled` | `false` | Evaluate the rules. Turning it off lifts only a pause the schedule applied. |
| `rules` | `[]` | One table per window. |

Each `[[schedule.rules]]` entry:

| Key | Default | Description |
|-----|---------|-------------|
| `id` | assigned by the API | Stable identifier. The UI leaves it blank on a new rule and the API fills it in. |
| `name` | `""` | Label shown in the settings page and the queue. |
| `days` | — | Weekdays the window **starts** on: `mon`..`sun`, or full names. Required. |
| `start`, `end` | `"00:00"`, `"24:00"` | `HH:MM`, 24-hour clock. `end` earlier than `start` runs overnight; `24:00` ends the day. |
| `action` | `"pause"` | `pause`, `speed_limit` or `disable_servers`. |
| `speed_limit_bps` | `0` | Cap for `speed_limit`, in bytes/sec. `0` means unlimited and overrides a lower cap from another active rule. |
| `servers` | `[]` | Server ids taken out of rotation for `disable_servers`. |

Rules are evaluated against the process's **local** wall clock: inside a
container, set `TZ` to have windows follow your timezone.

The schedule is an overlay. It never rewrites `general.speed_limit_bps` or a
server's `enabled` flag — the configured values come back when the window
ends, and the servers page keeps showing what you configured. The same is true
of pauses: the schedule only resumes a pause it applied, so a manual pause, a
timed pause or a low-disk hold is never lifted by a window ending. Overlapping
caps use the lowest non-zero value, and an active `0` cap means unlimited,
overriding both `general.speed_limit_bps` and any lower cap from another rule.

A `pause` rule is **edge-triggered**: it pauses when its window opens, not on
every minute while the window is open. Resuming downloads by hand inside a
window therefore holds until that window closes, and the next window opening
pauses again — the schedule does not fight you back within a minute. The
resume also survives a restart: the open window is remembered, so the first
tick after the process starts does not treat it as a newly opened window.

Pause reasons are **additive**. The queue is paused while any of a user pause,
a low-disk hold or a schedule window is in force, and each lifts only its own:

- A window that opens while the low-disk guard is holding downloads still
  takes effect. When space frees up the guard drops its hold, and downloads
  stay paused until the window ends instead of running straight through it.
- A `Pause All` pressed inside a window takes precedence, so the end of the
  window will not resume it.
- The end of a window never lifts a user pause or a disk hold.
- A **pause for a duration** covers any window already open when it starts, so
  its expiry lifts only the user's pause: the window takes effect then rather
  than the queue resuming straight into it. A disk hold is likewise left in
  place. (An indefinite `Pause All` is different: it stays in force until it is
  lifted, and the windows it covers do not pause again after it is.)
- A manual resume inside a window lasts for that window even when **two windows
  overlap** and one of them closes first: only a window that newly opens
  pauses again. A rule's `id` is part of its window's identity, so a rule the
  settings API saved back under a newly generated `id` is a different rule.

## `[par2]`

| Key | Default | Description |
|-----|---------|-------------|
| `defer_volumes` | `true` | Hold PAR2 recovery volumes until damage shows up, then fetch only as many as needed. `false` fetches all of them up front. |

## `[dav]`

The WebDAV media library, which streams files from Usenet on demand.

| Key | Default | Description |
|-----|---------|-------------|
| `enabled` | `false` | Serve `/dav`. |
| `auto_send_all` | `false` | Send every completed download to the library. |
| `category_rules` | `[]` | Categories sent to the library when `auto_send_all` is false. |
| `username`, `password` | unset | Basic auth for `/dav`. |
| `api_key` | unset | `X-Api-Key` header accepted instead of Basic auth. |

If `username`, `password` and `api_key` are all unset, `/dav` needs no
authentication.

## `[notifications]`

Each target is off until its URL or topic is set. Changes apply after a
restart.

| Key | Default | Description |
|-----|---------|-------------|
| `webhook_url` | `""` | POST a JSON document for each event. |
| `apprise_api_url` | `""` | Apprise API notify endpoint, such as `http://apprise:8000/notify`. |
| `apprise_urls` | `""` | Comma-separated Apprise URLs. Leave empty if the endpoint has a config key. |
| `ntfy_server` | `"https://ntfy.sh"` | ntfy server. |
| `ntfy_topic` | `""` | ntfy topic. |
| `ntfy_token` | `""` | ntfy access token. |

See [NOTIFICATIONS.md](NOTIFICATIONS.md) for events and browser alerts.

## `[otel]`

OpenTelemetry export over OTLP gRPC. See [OBSERVABILITY.md](OBSERVABILITY.md).

| Key | Default | Description |
|-----|---------|-------------|
| `enabled` | `false` | Fallback switch for logs and metrics. |
| `endpoint` | `"http://localhost:4317"` | Fallback endpoint for logs and metrics. |
| `logs_enabled` | unset (uses `enabled`) | Export logs. |
| `logs_endpoint` | unset (uses `endpoint`) | Log endpoint. |
| `metrics_enabled` | unset (uses `enabled`) | Export metrics. |
| `metrics_endpoint` | unset (uses `endpoint`) | Metrics endpoint. |
| `service_name` | `"rustnzb"` | Service name reported to the collector. |

## Other keys

`config_version` at the top of the file is the schema version. rustnzb writes
it on every save and migrates older files on load. Don't edit it.

## Backup and restore

Settings can be exported to one TOML file and restored onto another install,
from **Settings → Backup & restore** or the API. A backup carries the settings
only: jobs, history, the database and the download directories are not part of
it.

| Route | Method | What it does |
|-------|--------|--------------|
| `/api/config/backup` | `GET` | Download the configuration as `application/toml` |
| `/api/config/backup?secrets=true` | `GET` | The same file with real credentials |
| `/api/config/backup/preview` | `POST` | Validate a backup and report what a restore would do; writes nothing |
| `/api/config/backup/restore` | `POST` | Validate, save and apply a backup |

All four are protected by the native API authentication and return
`Cache-Control: no-store`. The file is capped at 4 MiB.

### The file

```toml
[backup]
backup_version = 1
created_at = "2026-10-20T09:00:00Z"
app_version = "1.7.0"
secrets_included = false

[config]
config_version = 1

[config.general]
...
```

`backup_version` is the envelope schema and is independent of the config's own
`config_version`, and it is required — a file without it is not treated as
version 1. A file from a newer build is refused, and so is `config_version`
newer than this build understands. Unknown keys are refused rather than
dropped, exactly as they are for `config.toml`, and that includes the `[backup]`
header. A TOML syntax error is reported with its line and column but without
echoing the offending source line, so a secrets-included file cannot leak a
credential through the error response.

### Secrets

The default export **masks** every stored secret as `********` and writes
`secrets_included = false`; that file is safe to share. `?secrets=true` writes
the real values and sets `secrets_included = true`; the UI warns before it does.

On restore the mask is a *keep token*: a masked secret keeps the value the
target install already holds, and `secrets_kept` in the response counts them.
A mask with nothing to keep — restoring a masked backup onto a fresh install —
leaves that secret unset and is reported in `warnings`; the literal `********`
is never written as a value. A secret **absent** from a backup is cleared,
because a restore replaces the file layer.

The secrets covered are the SABnzbd API key, the DAV password and API key,
news-server passwords, the `user:pass@` part of server proxy URLs, the
notification webhook, Apprise and ntfy values (including the ntfy topic, which
is a bearer secret on a public server), RSS feed URL query strings (indexers
carry the account key there, for example `?t=2000&apikey=…`), and the
`user:pass@` part of OTLP endpoints. A masked feed URL keeps its scheme, host
and path (`https://indexer.example/rss?********`) so a restore can still match
it to the right feed.

A masked URL is kept only when the target has the same feed (by name) or
endpoint. With nothing to keep, the feed is dropped and the endpoint returns to
its default, with a warning; a document that still carries a mask after the
merge is refused instead of saving `********` as a value.

Because the whole query string is replaced, a masked backup carries no query
parameters beyond the mask. Restoring one onto an install that does not already
have the same feed therefore drops that feed; add it again with its URL. A key
embedded in a feed's *path* rather than its query (for example
`/rss/<key>/feed.xml`) is not detected — check such a URL before sharing a
masked export.

### What a restore does

- The whole saved configuration is replaced: a setting missing from the file
  returns to its default.
- Nothing is written unless the document parses, normalizes and validates;
  a refusal returns `400` with the reasons and leaves the config and the file
  untouched.
- Live settings — servers, categories, speed limit, disk guards, retention,
  concurrency, notifications and post-processing scripts — are applied at
  once.
- Settings read only at start-up are listed in `restart_required`:
  `general.listen_addr`, `general.port`, the data, incomplete, complete and
  watch directories, `general.log_level`, the post-processing and repair
  worker counts, `direct_unpack`, `early_failure_check`,
  `required_completion_pct`, `article_timeout_secs`,
  `max_nested_archive_depth`, `local_import_dir`, `fetch_allow_private`,
  `fetch_allowed_hosts`, `group_max_headers`, `shutdown_timeout_secs` and the
  `[otel]` settings.
- `general.log_level_before_debug` is process-local and is not exported.

Settings supplied by a CLI flag or environment variable are not replaced by a
restore: the override still wins until it is removed, and the file keeps the
value the backup wrote underneath it.
