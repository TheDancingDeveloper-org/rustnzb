# GUI-managed configuration — design review

Date: 2026-10-16 · Status: proposal, for operator review · Work item: WI-1701

Goal: make all of rustnzb's configuration manageable from the web GUI. A user
who installs it as a plain binary (or a Docker container they never edit) should
get a SABnzbd-like experience. Docker/GitOps operators who drive it from the
environment or a checked-in `config.toml` must keep working with no change in
behaviour unless they opt in.

This document is the deliverable of a design review. It changes no code.

---

## 1. Summary

The brief's premise is half right. rustnzb is **not** env-only:

- The bulk of the configuration lives in a TOML file (`config.toml`,
  `--config` / `RUSTNZB_CONFIG`). The GUI already writes it through
  `AppState::update_config_with`, which persists every change.
- Only **six settings plus the OTEL block** can come from the environment or the
  CLI (§2.1). Everything else is file-only or GUI-editable.
- What hurts users is a mix of gaps and sharp edges, not missing machinery:
  1. **The GUI exposes much less than the API.** `PUT /api/config/general`
     accepts 23 fields, but the Settings page calls none of it. The *Paths &
     disk*, *Post-processing* and *Logging & telemetry* tabs are read-only
     text telling the user to edit TOML or set env vars.
  2. **Overrides leak into the file.** `commit_config` serialises the
     *effective* config. That includes `--port`/`RUSTNZB_PORT`,
     `--listen-addr`, `--data-dir` (which the Docker image always passes), the
     desktop shell's forced `127.0.0.1`, and every `OTEL_*` variable. So the
     first GUI save of *any* setting copies env-supplied values into
     `config.toml`, where they outlive the env var. `RUSTNZB_DAV_ENABLED` goes
     further: `main.rs` writes `dav.enabled = true` to the file at startup, a
     one-way latch.
  3. **No source attribution.** The GUI cannot tell the user that a field is
     overridden. A saved value can silently have no effect: `data_dir` in
     Docker, or the log level under `RUST_LOG`. Only the debug-logging toggle
     handles this today (`overridden: true`).
  4. **Dead settings.** `general.cache_size` is validated and saved but nothing
     reads it. `general.log_file` and `--log-file`/`RUSTNZB_LOG_FILE` are
     parsed and never used.
  5. **Non-Docker first run is broken by Docker-shaped defaults.**
     `/downloads/...` and `/data` are created eagerly at startup, so a
     non-root bare-metal start fails before the GUI is reachable. On Windows
     the config path is relative to the working directory. The systemd unit
     points at `/etc/rustnzb/config.toml`, which the `rustnzb` user cannot
     write.
  6. **Secrets are exposed in several places.** `GET /api/config` masks server
     passwords but returns `general.api_key`, `dav.password` and `dav.api_key`
     in clear. `GET /api/config/dav` returns the DAV password and key, and the
     PUT replaces the block wholesale. The web-login password sits in
     `credentials.json` as plaintext (0600). With no `api_key` configured (the
     default), the SABnzbd-compatible `/api` accepts every request without a
     key.
  7. **Writes are not atomic.** `AppConfig::save` is a bare `std::fs::write`,
     with no temp file, rename, backup, or check for hand edits made since load.

**Recommendation in one paragraph:** keep `config.toml` as the single persisted
store, which mirrors `sabnzbd.ini`. Treat env and CLI values as **locked
overrides** layered above it, never written back, and shown in the GUI as
"set by `RUSTNZB_…`", read-only. Drive validation, the API, the GUI and the env
mapping from one **settings registry**, so every field carries its type, bounds,
apply mode (live / restart), secrecy and env name. Make writes atomic with
rolling backups. Add an opt-in **managed mode** that makes the GUI fully
read-only for GitOps. Ship it in ten small PRs (§12). The first two are safety
fixes worth landing on their own.

---

## 2. Current-state inventory

### 2.1 How a value is resolved today

Precedence, highest first (README, `AGENTS.md`, `main.rs`, `startup.rs`):

```
CLI flag  >  environment  >  config.toml  >  serde defaults (GeneralConfig::default etc.)
```

| Source | Mechanism | Settings it can set |
|---|---|---|
| CLI / env via clap | `Args` in `apps/rustnzb/src/main.rs` (`#[arg(env = …)]`, so the flag beats the env var) | `--config`/`RUSTNZB_CONFIG`, `--listen-addr`/`RUSTNZB_LISTEN_ADDR`, `--port`/`RUSTNZB_PORT`, `--data-dir`/`RUSTNZB_DATA_DIR`, `--log-level`/`RUSTNZB_LOG_LEVEL`, `--log-file`/`RUSTNZB_LOG_FILE` (**unused**) |
| Raw env | `main.rs` | `RUST_LOG` (beats all log-level sources); `RUSTNZB_DAV_ENABLED` / `ENABLE_DAV` (latched into the file) |
| Raw env | `crates/nzb-web/src/startup.rs` | `OTEL_ENABLED`, `OTEL_EXPORTER_OTLP_ENDPOINT`, `OTEL_LOGS_ENABLED`, `OTEL_EXPORTER_OTLP_LOGS_ENDPOINT`, `OTEL_METRICS_ENABLED`, `OTEL_EXPORTER_OTLP_METRICS_ENDPOINT`, `OTEL_SERVICE_NAME` |
| Container | s6 `svc-rustnzb/run` | always `--config /config/config.toml --data-dir /data` (so `data_dir` is CLI-locked in Docker); `PUID`/`PGID`/`TZ` are image-level |
| Desktop shell | `desktop/src-tauri/src/main.rs` | forces `listen_addr = 127.0.0.1`, `data_dir` = platform data dir, config in platform config dir |
| TOML file | `AppConfig::load` (creates a default file if missing) | everything in `AppConfig` |
| SQLite `settings` table | `Database::{get,set}_setting` | runtime **state** only (global pause, paused-job set, disk-space hold), not configuration |
| SQLite `rss_rules` | `/api/rss/rules` | RSS download rules (GUI-managed already; data, not config) |
| `credentials.json` in `data_dir` | `CredentialStore` | web-UI username + password (plaintext, 0600) |

### 2.2 Every setting

Legend. **Src**: where it can come from (F = file, E = env, C = CLI).
**GUI**: what the Settings page does today (RW = editable, RO = shown read-only,
— = absent). **API**: native endpoint that writes it. **Apply**: when a change
takes effect today (L = live, R = next restart, ✗ = never, the value is unused).
**SAB** gives the closest SABnzbd key (`[misc]` unless another section is named).
"≈" means the semantics differ.

#### `[general]`

| Key | Default | Src | GUI | API | Apply | SAB equivalent |
|---|---|---|---|---|---|---|
| `listen_addr` | `0.0.0.0` | F E C | — | — | R | `host` |
| `port` | `9090` | F E C | — | — | R | `port` (+ `https_port`) |
| `api_key` | none (**SAB API open**) | F | reveal + rotate | `/config/sabnzbd-api-key/rotate` | L | `api_key` (SAB generates one on first run) |
| `incomplete_dir` | `/downloads/incomplete` | F | RO | `/config/general` | R (QM setter exists, unused) | `download_dir` |
| `complete_dir` | `/downloads/complete` | F | RO | `/config/general` | R (QM setter exists, unused) | `complete_dir` |
| `data_dir` | `/data` | F E C | RO | `/config/general` (ineffective in Docker) | R | `admin_dir` (≈) |
| `speed_limit_bps` | `0` | F | RW | `/config/speed-limit`, SAB `config&name=speedlimit` | L | `bandwidth_max` + `bandwidth_perc` (≈) |
| `cache_size` | 500 MB | F | — | `/config/general` | ✗ unused | `cache_limit` |
| `log_level` | `info` | F E C (`RUST_LOG`) | debug toggle (shows overridden) | `/config/debug-logging` | L | `log_level` (≈ −1/0/1) |
| `log_level_before_debug` | none | F (internal) | — | via toggle | — | — |
| `log_file` | none | F E C | — | — | ✗ unused | `log_dir` (≈) |
| `history_retention` | keep all | F | RW | `/config/history-retention`, `/config/general` | L | `history_retention_option`/`_number` (≈) |
| `max_active_downloads` | `1` | F | RW | `/config/max-active-downloads`, `/config/general` | L | — (SAB downloads one job at a time) |
| `max_post_processing_jobs` | `2` | F | — | `/config/general` | R | — |
| `max_repair_workers` | `1` | F | — | `/config/general` | R | — |
| `max_extract_workers` | `1` | F | — | `/config/general` | R | — |
| `min_free_space_bytes` | 1 GiB | F | RW | `/config/disk-guards` | L | `download_free` |
| `watch_dir` | none | F | RO | `/config/general` | R (watcher spawned once) | `dirscan_dir` (+ `dirscan_speed`) |
| `rss_history_limit` | `500` | F | — | `/config/general` | L | — |
| `direct_unpack` | `true` | F | — | `/config/general` | R | `direct_unpack` |
| `max_nested_archive_depth` | `5` | F | — | `/config/general` | R | — (no confirmed equivalent) |
| `abort_hopeless` | `true` | F | RW | `/config/disk-guards` | R (persisted only; QM reads at construction) | `fail_hopeless_jobs` |
| `early_failure_check` | `true` | F | — | `/config/general` | R | ≈ `fail_hopeless_jobs` fast-path |
| `required_completion_pct` | `100.2` | F | — | `/config/general` | R | `req_completion_rate` |
| `article_timeout_secs` | `30` | F | — | `/config/general` | R | `[servers]` `timeout` (≈ per server) |
| `auto_sort_remaining_pct` | `false` | F | — | `/config/general` | L | `auto_sort` (≈) |
| `rss_downloaded_item_expiry_days` | none | F | — | `/config/general` | L | — |
| `scripts_dir` | none | F | — | `/config/general` | L | `script_dir` |
| `script_success` / `script_failure` | none | F | — | `/config/general` | L | per-category `script` (≈) |
| `script_timeout_secs` | `300` | F | — | `/config/general` | L | — |
| `script_max_output_bytes` | 1 MiB | F | — | `/config/general` | L | — |
| `fetch_allow_private` | `false` | F | — | — | L (read per fetch) | — (SAB does not filter) |
| `fetch_allowed_hosts` | `[]` | F | — | — | L | — |
| `group_max_headers` | `100000` | F | — | — | L | — |

#### `[[servers]]` (`nzb_nntp::ServerConfig`) — GUI RW, API `/config/servers`, applied live via `QueueManager::update_servers`

| Key | Default | SAB `[servers][[name]]` |
|---|---|---|
| `id` | derived `host-port` | section name |
| `name` | `""` | `displayname` |
| `host` / `port` | — / `563` | `host` / `port` |
| `ssl` / `ssl_verify` | `true` / `true` | `ssl` / `ssl_verify` (SAB has 0/1/2 levels) |
| `username` / `password` | none (password masked `********`) | `username` / `password` |
| `connections` | `8` | `connections` |
| `priority` | `0` | `priority` |
| `enabled` / `optional` | `true` / `false` | `enable` / `optional` |
| `retention` | `0` | `retention` |
| `pipelining` | `4` | — (SAB key not confirmed in source) |
| `compress` | `false` | — |
| `ramp_up_delay_ms` | — | — |
| `recv_buffer_size` | `0` | — (not in GUI) |
| `proxy_url` | none (may embed credentials) | ≈ global `misc.socks5_proxy_url` (restart-guarded in SAB) |
| `trusted_fingerprint` | none | — (not in GUI) |
| `connect_timeout_secs` | — | `timeout` |

#### `[[categories]]` — GUI RW (name, output_dir, post_processing), API `/config/categories`, live via `set_categories`

| Key | SAB `[categories]` |
|---|---|
| `name` | `name` |
| `output_dir` | `dir` |
| `post_processing` (0–3) | `pp` |
| `cleanup_patterns` | global `cleanup_list` (≈); **not in GUI** |
| `unwanted_extensions` | global `unwanted_extensions` (≈); **not in GUI** |
| — | `script`, `priority`, `order` (no rustnzb equivalent) |

#### `[[rss_feeds]]` — GUI RW on the RSS page, API `/config/rss-feeds`, live (monitor woken on commit)

`name`, `url`, `poll_interval_secs` (SAB: global `rss_rate`), `category` (`cat`),
`filter_regex` (SAB: ordered filter list per feed), `enabled` (`enable`),
`auto_download`, `max_age_days`. SAB equivalent: `[rss][[feed]]`.

#### `[otel]` — env-overridable, GUI shows env-var help text only, restart

`enabled`, `endpoint`, `logs_enabled`, `logs_endpoint`, `metrics_enabled`,
`metrics_endpoint`, `service_name`. No SABnzbd equivalent.

#### `[dav]` (feature `webdav`) — GUI RW, API `/config/dav`

| Key | Apply | Note |
|---|---|---|
| `enabled` | R (handle built once in `main`) | also `RUSTNZB_DAV_ENABLED` / `ENABLE_DAV` (latched) |
| `auto_send_all`, `category_rules` | L (read per add event) | |
| `username`, `password`, `api_key` | L (read per request) | **returned in clear by GET** |

#### Outside `AppConfig`

| Setting | Where | GUI | SAB equivalent |
|---|---|---|---|
| Web login user/password | `data_dir/credentials.json` | first-boot setup + change credentials | `username` / `password` in ini |
| Theme | browser-local | RW | `web_color` / `interface` (≈) |
| RSS rules | SQLite `rss_rules` | RW | `[rss]` filters |

**SABnzbd settings with no rustnzb equivalent** (features, not config):
`https_port`/`enable_https`/certificates, `url_base`, `nzb_key`, `inet_exposure`,
`local_ranges`, `host_whitelist`, scheduler (`[scheduler]`), notifications
(email/apprise sections), sorting (`[sorters]`), `pre_script`, per-category
priority/script, `ipv6_hosting`, `language`, `dirscan_speed`,
`complete_free`, quota (`quota_size`/`quota_day`), `top_only`, `pre_check`. Those
are feature gaps, outside this design. The registry (§5) gives them an obvious
home when they land.

---

## 3. How SABnzbd does it

Sources: SABnzbd `develop` (`version.py` = 5.2.0Beta2): `sabnzbd/config.py`,
`cfg.py`, `interface.py`, `api.py`, `security.py`, `__init__.py`,
`SABnzbd.py`, `misc.py`, `downloader.py`; the sabnzbd.org wiki (configuration
5.1 api/general, 4.5 special, hostname-check, command-line parameters,
directory setup); linuxserver/docker-sabnzbd. Two notes: develop has moved
from CherryPy to Starlette/uvicorn, and there is no `wizard.py` (the wizard
lives in `interface.py`).

### 3.1 Storage

- **One file, `sabnzbd.ini`**, parsed with configobj (`config.CONFIG`, a
  `SABnzbdConfig`). It holds flat sections (`[misc]` with about 170 options and
  the scheduler as `misc.schedlines`, `[logging]`, notification sections) and
  `[[name]]` subsections for `[servers]`, `[categories]`, `[rss]`, `[sorters]`
  and `[indexers]`. The file carries `__version__ = 19` and
  `__encoding__ = utf-8`. A newer version is refused.
- **Location:** `%LOCALAPPDATA%\sabnzbd` on Windows,
  `~/Library/Application Support/SABnzbd` on macOS, `~/.sabnzbd` on Linux, or
  `-f/--config-file` (a file or a directory). Relative admin/log dirs resolve
  against the ini's directory.
- **Creation:** under `umask 0o77`, so the file is 0600. A missing or
  unparseable ini falls back to `sabnzbd.ini.bak`.
- **Writes are not atomic.** `save_config` copies the ini to `.bak`, rewrites
  the ini in place, restores the mode, and on exception deletes the ini and
  renames `.bak` back. There is no temp-file rename and no fsync. It saves only
  when `CONFIG.modified` is set, and the main loop also saves about every 30 s.
- **Backup and restore** are GUI features. *Create backup* writes a zip of the
  ini, the history DB (SQLite online backup) and the HTTPS material. *Restore*
  validates the zip, holds it in memory, and applies it **during a restart**,
  stripping `protect=True` options first.

### 3.2 Types, validation, callbacks

- `Option(section, keyword, default, add, public, protect)`. `public=False`
  hides an option from `get_config`. `protect=True` blocks `set_config` and
  backup restore, but **not** the GUI forms, which call `.set()` directly.
- Typed options: `OptionNumber` **clamps** to min/max rather than rejecting.
  `OptionBool` stores 0/1. `OptionStr` has KMGT parsing. `OptionDir` creates,
  checks writability and resolves relative paths. Also `OptionList`,
  `OptionPassword` and `OptionFilters`.
- Validators return `(error, value)`. `validate_host`: an IP or a resolvable
  name. `validate_safedir`: refuses moving the admin/download dir while the
  queue or post-processing is busy. `validate_download_vs_complete_dir`: not
  equal, not nested, not inside the program dir. Others include
  `validate_scriptdir_not_appdir`, `validate_url_base`, and argparse
  allow-lists for nice/ionice/unrar parameters (exploit guards).
- **The weak spot:** on the API path `Option.set_dict` **ignores the validator's
  error**, so `set_config` reports success while the value stays unchanged. The
  GUI forms stop at the first error. rustnzb should not copy this; §7 returns
  field errors on every path.
- **Live-apply** is done with single per-option callbacks, wired in
  `sabnzbd.initialize()`: `cache_limit`, `direct_write`, `download_dir`,
  `top_only`, quota, scheduler, language, and bandwidth → `Downloader.speed_set`.
  Servers re-init through `Downloader.update_server`, or are marked
  `server.restart` when in use.

### 3.3 API and access levels

- `api._api_table` maps `(mode, name)` to `(handler, access_level, config_locked)`.
  The levels are 1 add NZB, 2 API without config, 3 full API, 4 web UI.
  **`nzb_key` is accepted only for level 1.** `api_key`, or a session cookie
  plus a CSRF header, covers everything.
- `get_config` (level 3) returns sections with `OptionPassword` values masked,
  but returns `api_key`, `nzb_key` and notification tokens **in clear**.
- `set_config` (level 3) upserts `misc` keys and `[[items]]`. Server changes
  call `Downloader.update_server`. The call returns the new config.
  `set_config_default` and `del_config` also exist. Through `mode=config` the
  API offers `speedlimit` (runtime only, not persisted), `set_pause`
  (level 2), and `set_apikey`, `set_nzbkey`, `test_server`, `create_backup`
  (level 3).
- The web UI posts per-page whitelists (`GENERAL_LIST`, `LIST_DIRPAGE`,
  `SWITCH_LIST`, `SPECIAL_*_LIST`) with CSRF and login.

### 3.4 Restart versus live

- `cfg.guard_restart` sets the global `RESTART_REQ`. It is attached to `host`,
  `port`, `https_*`, `username`, `password`, `log_dir`, `local_ranges`,
  `socks5_proxy_url`, `web_color` and XFF settings. Only
  `/config/general/save` reports `restart_req` back. `url_base` restarts
  immediately. The GUI has a Restart button (`mode=restart`).
- Most other settings take effect where they are used, at read time.

### 3.5 Secrets and exposure

- Server passwords and `misc.password` are `OptionPassword`, **stored in
  plaintext**. A `!!!encoded!!!` hex form is used only for characters that are
  unsafe in an ini. `api_key` and `nzb_key` are plain strings
  (`uuid4().hex`), generated on first run.
- **Masking:** `get_stars()` (10 asterisks). A posted value made only of `*`
  is ignored, and `""` clears the password. `test_server` substitutes the
  stored password for a starred one. rustnzb's `********` round-trip follows
  the same pattern.
- **`inet_exposure` (0–5)** caps what non-local clients may do: local only,
  add-only, API, full API, web UI, or web UI with login. "Local" means loopback
  or `local_ranges` (default: RFC 1918, link-local, ULA).
- **Login:** sessions are random tokens stored as SHA-256. Cookies are
  httponly and SameSite=strict. Comparisons are constant-time, with lockout on
  failures. Level 5 lets local clients bypass the login.
- **`host_whitelist`:** a DNS-rebinding guard. It is skipped when a login is
  set. Otherwise it allows only `localhost`, IP literals, `*.local` and listed
  names, seeded with `gethostname()`. In Docker that seed is the container's
  hostname, so a reverse-proxy name is refused until the user adds it, a
  well-known SAB-in-Docker papercut (inferred from source).

### 3.6 First run

`/` redirects to `/wizard` while no server is configured (`?skip_wizard`
bypasses it). The steps are language, then a news-server form with test, then
the access URLs and folders. The wizard does **not** change host or port; it
only shows the resulting URLs.

### 3.7 Docker and overrides

- **SABnzbd has no environment-variable configuration at all.** The source
  reads only `HOME`, `APPDATA`, `PATH` and similar. linuxserver adds
  `PUID/PGID/TZ/UMASK` and `FILE__` secrets for its own scripts, mounts
  `/config`, and runs `SABnzbd.py --config-file /config --server ::`.
- **CLI values are persisted.** `--server`, `--https`, `--inet_exposure`,
  `--no-login`, `-l` and others go through `.set()` and are written on the next
  save. So in the linuxserver image `misc.host` is reset to `::` on every
  start. The port comes from the ini, and users change it by remapping in
  Docker (or editing it and remapping).
- A pre-seeded ini is read normally; missing keys get defaults on the next save.

### 3.8 Locked and ini-only settings

- **`misc.config_lock`**: when set, every `/config*` and `/wizard` route
  returns 403, the `config_locked` API calls are refused, and `save_config`
  refuses to write. No GUI page offers it; you edit it with SABnzbd stopped.
  It is not `protect`ed, so the API could turn it on and then never off
  (inferred). This is SABnzbd's version of §5.2 managed mode.
- **Ini-only:** `email_dir` (protected because templates execute) and several
  logging and queue tunables that appear on no form.

### 3.9 What rustnzb takes and what it does better

| Aspect | SABnzbd | rustnzb proposal |
|---|---|---|
| Store | one ini, GUI-written | one TOML, GUI-written (same idea) |
| Atomic write | no (`.bak` + in-place) | temp + fsync + rename, rolling backups |
| Overrides | none from env; CLI values **persisted** | env/CLI locked, **never** persisted, shown as such |
| Validation on API path | errors silently dropped | the same validators on every path, field-level 400s |
| Restart signalling | one global flag, reported by one page | per-key `apply`, `pending_restart` set, banner + restart |
| Secret masking | passwords masked, keys in clear | passwords and keys masked; keys revealed by an explicit route |
| Key scopes | `nzb_key` (add) / `api_key` (all) | `X-Api-Key` = Normal only; Sensitive needs session + step-up |
| Exposure guard | `inet_exposure`, `local_ranges`, `host_whitelist` | `Host` allowlist while keyless, setup code, loopback default for bare metal |
| Lock | `config_lock` (ini edit only) | `RUSTNZB_CONFIG_MANAGED` (env only) or a read-only file |
| Wizard | redirect while no server; server + folders | redirect while no server (exists); login, network, server/import, folders, categories, keys |

---

## 4. Requirements

1. A non-Docker user can install, start, finish setup and change any setting
   without opening a text editor or setting a variable.
2. A Docker/GitOps deployment that sets env vars or ships a `config.toml` gets
   **byte-for-byte the same effective configuration** after upgrade, and its
   files are not rewritten unless a person saves something in the GUI.
3. Env/CLI values are never written to disk, and the GUI never offers a control
   that would have no effect.
4. A save is all-or-nothing, cannot corrupt the file, and can be undone.
5. Secrets are write-only through the API by default.
6. The GUI cannot be used to widen its own blast radius beyond bounds the
   operator set outside it (§9).
7. The SABnzbd compatibility contract (`/api`, `/sabnzbd/api`) does not regress.

---

## 5. Proposed design

### 5.1 Precedence model

**Recommended: env/CLI as locked overrides above GUI-managed file values.**

```
            ┌───────────────────────────────┐
  highest   │ CLI flags                     │  locked, never persisted
            │ environment (RUSTNZB_*, OTEL_*,│  locked, never persisted
            │   RUST_LOG, *_FILE secrets)   │
            ├───────────────────────────────┤
            │ config.toml  ← GUI writes here│  "file layer"
            ├───────────────────────────────┤
  lowest    │ built-in defaults (registry)  │
            └───────────────────────────────┘
```

This keeps today's order, so Requirement 2 holds automatically. The change is
in what is **stored and shown**:

- `AppState` holds two configs: `file: AppConfig`, exactly what is on disk, and
  `effective: AppConfig`, which is `file` with the overrides applied. Handlers
  mutate `file`. `effective` is rebuilt as `overlay(file, overrides)` and
  published through the existing `ArcSwap`. `commit_config` saves `file`
  only. This alone fixes the leak in §1, item 2.
- `overrides` is computed once at startup into an `OverrideSet` (key → value,
  source `env:RUSTNZB_PORT` / `cli:--port`). It is immutable for the life of the
  process, since env cannot change under a running process.
- Every API read returns, per key, `{value, source, locked, env}`. `source` is
  one of `default | file | env | cli`.
- A write to a locked key is refused with `409 {"error":"locked","key":"general.port","source":"env:RUSTNZB_PORT"}`.
  It is never silently accepted.
- The GUI renders a locked field disabled, with a padlock and "Set by
  `RUSTNZB_PORT`. Remove the variable to manage this here." The value it
  shows is the effective one, and the file value is shown greyed underneath
  when it differs.

Rejected alternative, **env > file > GUI as a three-tier store** (GUI values
in the DB, file above them): a third place to look adds no capability. "Why is
my edit ignored?" gets worse, and backup/restore has to span two stores.

**Generalised env mapping (Phase 6, opt-in by use):** every scalar registry key
gets a canonical variable `RUSTNZB__<SECTION>__<KEY>`, for example
`RUSTNZB__GENERAL__SPEED_LIMIT_BPS`. The existing short names (`RUSTNZB_PORT`,
`RUSTNZB_LISTEN_ADDR`, `RUSTNZB_DATA_DIR`, `RUSTNZB_LOG_LEVEL`, `OTEL_*`,
`RUSTNZB_DAV_ENABLED`, `ENABLE_DAV`) remain as aliases forever. Secrets also
accept `…_FILE` (Docker/Kubernetes secrets), for example
`RUSTNZB__GENERAL__API_KEY_FILE=/run/secrets/rustnzb_api_key`. List-valued
sections (`servers`, `categories`, `rss_feeds`) are **not** env-mappable;
GitOps users manage them in `config.toml`, which suits them better (decision
D4).

### 5.2 Managed (read-only) mode

GitOps operators who want the file to be the only source of truth get an
explicit switch: `RUSTNZB_CONFIG_MANAGED=1` (env only, by design), or a
read-only `config.toml`.

- In managed mode, every config write returns `409 {"error":"managed"}`. The
  GUI shows a banner ("Configuration is managed outside rustnzb") with every
  field read-only, and the server/category/feed editors are hidden.
- Runtime controls are **not** configuration and keep working: pause/resume,
  the speed limit (see D6), queue actions.
- With a read-only file and no switch, rustnzb starts normally and treats the
  file as managed, instead of failing the first save with a 500 as it does today.

### 5.3 Settings registry

A single static table in `nzb-core::config::registry`, one entry per leaf key:

```rust
pub struct Setting {
    pub key: &'static str,            // "general.speed_limit_bps"
    pub kind: Kind,                   // Bool | U64{min,max} | F64{..} | Path{..} | Enum(&[..]) | String{..} | Secret | List(..)
    pub default: fn() -> toml::Value,
    pub apply: Apply,                 // Live | Restart | Subsystem("rss") ...
    pub class: Class,                 // Normal | Sensitive | Bounding (see §9)
    pub env: &'static [&'static str], // ["RUSTNZB__GENERAL__SPEED_LIMIT_BPS"] + legacy aliases
    pub cli: Option<&'static str>,
    pub sab: Option<&'static str>,    // "misc.bandwidth_max" — feeds get_config/set_config
    pub group: &'static str,          // GUI tab/section
    pub label: &'static str,
    pub help: &'static str,
}
```

It drives the following:

- **Validation**: one `validate(key, value)` per kind, plus whole-config checks
  (§7).
- **The API**: `GET /api/config/schema` serialises it, so the frontend renders
  generic fields for most settings. Servers, categories and feeds keep their
  bespoke editors.
- **Override resolution**: from the `env` and `cli` columns.
- **SAB compatibility**: `get_config`/`set_config` map through `sab`.
- **Docs**: a test regenerates the settings table in `config.example.toml`.

A unit test walks `toml::to_string(&AppConfig::default())` and fails if any leaf
key lacks a registry entry, or an entry names a key that does not exist. That
stops the drift that produced the dead `cache_size` and `log_file`.

The registry *describes* `AppConfig`; it does not replace it. The serde structs
stay the typed source of truth, and the engine keeps reading typed fields.

### 5.4 Persisted store

**Recommended: keep `config.toml`** (decision D1).

- It already exists and every install has one. `sabnzbd.ini` is SAB's equivalent.
- It is hand-editable and diffable, so it suits GitOps and backups well.
- The DB holds runtime state. Moving config there would break every
  file-based deployment and make "what is my config" a SQL question.

**Write path** (`ConfigStore::commit`):

1. Serialise the candidate *file layer*. Use `toml_edit` to keep the user's
   comments, key order and formatting from the loaded document, updating only
   the changed keys (decision D2; `toml_edit` would be a new workspace
   dependency next to the existing `toml` 1.1).
2. Validate the whole candidate (§7). Refuse it with a field-level error list
   if anything fails.
3. **Conflict check**: compare the on-disk file's hash and mtime with the
   values recorded at load or last commit. If someone edited the file by hand
   since then, refuse with `409 {"error":"file_changed"}` and offer *Reload
   from disk*. A hand edit is never silently clobbered.
4. Write `config.toml.tmp` in the same directory with mode `0600`, `fsync`,
   `rename` over `config.toml`, then `fsync` the directory (Unix). On Windows
   `std::fs::rename` replaces an existing target; whether it also needs
   write-through is to be checked during implementation. The `tempfile`
   crate's `NamedTempFile::persist` (already a dev-dependency) packages the
   same sequence and could be promoted to a normal dependency.
5. Before the rename, copy the previous file to
   `config-backups/config-<UTC timestamp>.toml` (0600). Keep the newest *N*
   (default 20) plus one per day for 7 days.
6. Publish the new `file` and `effective` configs, wake subscribers (RSS
   monitor), and record an audit line (§8.4).

The existing `config_write` mutex already serialises writers. Keep it.

**Schema version:** add a top-level `config_version = 1`. Load runs ordered
migration steps (`v0 → v1`: no-op apart from stamping), backing up first, so
future renames such as `cache_size` have a home.

**Reload:** `POST /api/config/reload`, and `SIGHUP` on Unix, re-read the file,
validate, diff, and apply live keys. Restart-only keys go on the pending list.
This makes hand edits and GitOps file updates take effect without a restart
where possible.

### 5.5 Hot reload versus restart

Each registry entry declares `apply`. A commit returns
`{"applied": [...], "restart_required": [...]}`. The server keeps a
`pending_restart` set: keys whose saved value differs from the value the
process started with. `GET /api/status` exposes it, and the GUI shows a
persistent banner: "3 changes take effect after a restart [Restart now]".

| Apply | Keys (today) | Plan |
|---|---|---|
| **Live** | `speed_limit_bps`, `max_active_downloads`, `history_retention`, `auto_sort_remaining_pct`, `min_free_space_bytes`, scripts (`scripts_*`, `script_*`), `rss_*`, `fetch_*`, `group_max_headers`, `api_key`, `log_level`, servers, categories, RSS feeds, `dav.auto_send_all`/`category_rules`/credentials | keep |
| **Cheap to make live** | `incomplete_dir`, `complete_dir` (setters `QueueManager::set_{in,}complete_dir` exist; apply to *new* jobs only), `abort_hopeless`, `early_failure_check`, `required_completion_pct`, `article_timeout_secs`, `direct_unpack`, `max_nested_archive_depth` (move to atomics read per job), `watch_dir` (restartable watcher task) | Phase 8 |
| **Restart** | `listen_addr`, `port`, `data_dir` (owns DB, lock, credentials), `max_post_processing_jobs` / `max_repair_workers` / `max_extract_workers` (pools cannot drop live permits), `otel.*` (global providers), `dav.enabled` | stay restart-only |
| **Dead** | `cache_size`, `log_file` | Phase 1: hide from GUI, mark deprecated, warn when set; implement or remove later (D7) |

**Restart action:** `POST /api/system/restart` (session auth, §8). The
implementation is a graceful shutdown (stop accepting work, flush DB, persist
queue), then one of:

- **re-exec in place** (`execv` of `current_exe()` with the original argv on
  Unix, spawn-and-exit on Windows). This works the same under Docker, systemd
  and bare runs, and needs nothing from the supervisor. **Recommended**
  (decision D5).
- exit with code 0 and rely on the supervisor. This fails under systemd's
  `Restart=on-failure` and under `docker run` without a restart policy.

SABnzbd also offers restart from the GUI (§3.4), so users expect it.

### 5.6 What changes in the existing handlers

The granular endpoints (`/config/speed-limit`, `/config/disk-guards`,
`/config/max-active-downloads`, `/config/history-retention`,
`/config/debug-logging`, `/config/general`) stay as thin wrappers over the new
store. They have external callers, and removing them is not worth the churn.
Each one gains a locked-key check and returns `restart_required`.

---

## 6. API

All under `/api`, behind the existing auth middleware, documented with utoipa.

| Method & path | Purpose | Auth class |
|---|---|---|
| `GET /config/schema` | registry: keys, kinds, bounds, apply, class, env names, labels | read |
| `GET /config/effective` | `{revision, values, sources, pending_restart, managed}`; secrets masked | read |
| `PATCH /config` | JSON merge patch (RFC 7396) of the file layer, `If-Match: <revision>`; returns `{revision, applied, restart_required}` or `400 {errors:[{key,message}]}` / `409 locked|managed|file_changed|stale_revision` | write (sensitive keys need step-up, §8.2) |
| `POST /config/validate` | the same body as PATCH, dry run | read |
| `POST /config/reload` | re-read the file from disk | write |
| `GET /config/backups` · `POST /config/backups/{id}/restore` | list and restore snapshots (a restore is itself backed up first) | write + step-up |
| `GET /config/export` | file layer as TOML, secrets replaced by `"********"` (`?secrets=1` needs step-up) | read / step-up |
| `POST /config/import` | upload a TOML, preview the diff, then apply (same validation as PATCH) | write + step-up |
| `POST /system/restart` | graceful restart | write |
| `GET /config` (existing) | **change:** mask `general.api_key`, `dav.password`, `dav.api_key`, and credentials in `proxy_url` | read |
| `GET /config/dav` (existing) | **change:** mask secrets; PUT keeps masked/absent secrets (same rule as servers) | read |

**The revision** is a hash of the serialised file layer. `If-Match` turns a
stale tab's save into a `409` instead of a lost update.

**Secret write semantics**, made uniform: key absent → unchanged; the mask
string `********` → unchanged; JSON `null` → cleared; any other string →
replaced. Today an empty server password means "unchanged", so there is no way
to *clear* one. `null` fixes that without breaking existing clients, which send
empty or mask strings.

**SABnzbd compatibility layer** (decision D8):

- `mode=get_config`: extend it beyond `misc.complete_dir` + categories,
  through the registry's `sab` column (servers with `password` blanked, as SAB
  does; `misc` with `api_key`/`nzb_key` withheld). nzb360 and similar clients
  read more of this than rustnzb returns today. Check the result against the
  5.0.4 golden fixtures.
- `mode=set_config` / `set_config_default` / `del_config`: implement only for a
  **whitelist** of Normal-class keys that map one to one (speed, history
  retention, categories). Return SAB's error shape for anything else. Never
  allow Sensitive or Bounding keys through the SAB key (§8.2).
- `mode=config&name=set_apikey`: implement as a rotate, requiring the current
  key, matching SAB.

---

## 7. Validation

Every write runs the same three layers, before anything is applied or persisted.
That is the existing "validate first, then mutate" rule in `h_general_update`,
generalised.

1. **Per key, from the registry kind**: type, numeric bounds (the existing
   `CACHE_SIZE_*`, `MAX_POSTPROC_WORKERS`, `ARTICLE_TIMEOUT_SECS_MAX`, … move
   into the registry), enums, string length, regex compiles
   (`compile_rss_regex`), URLs parse, `listen_addr` parses as an `IpAddr` (or
   `localhost`), port 1–65535.
2. **Paths**: absolute (relative paths are already refused on import, see #62),
   no `..` after normalisation, no control characters, and not inside a
   denylist (`/`, `/proc`, `/sys`, `/dev`, `/etc`, `/bin`, `/sbin`, `/usr`,
   `/boot`, `C:\Windows`, …). Must sit inside `RUSTNZB_ALLOWED_ROOTS` when that
   is set (§9). **Probe** before accepting: create the directory if missing and
   write then delete a test file as the service user, so "permission denied"
   surfaces in the form, not as a startup crash.
3. **Whole config**: `complete_dir` ≠ `incomplete_dir` and neither inside the
   other; `data_dir` not inside either; at least one category; unique server
   ids; category validation (`CategoryConfig::validate`, existing); feeds
   reference existing categories; `scripts_dir` set if any script path is
   relative (existing rule in `resolve_script_path`).

**At startup**, a hand-edited file that fails to parse still refuses to start,
naming the file and line (today's behaviour, kept). It is never overwritten. A
file that parses but fails semantic validation starts with a warning per key,
as `warn_unsafe_categories` already does, and the GUI shows the problem keys
highlighted.

Errors are returned as `400 {"errors":[{"key":"general.port","message":"must be 1–65535"}]}`
so the GUI can attach each one to its field.

---

## 8. Secrets and auth

### 8.1 Secrets

| Secret | Today | Proposal |
|---|---|---|
| Server `password` | masked in list/GET, mask round-trips | keep; add `null` = clear |
| `general.api_key` | **clear in `GET /api/config`**; dedicated reveal route | mask in `/config`; keep the explicit reveal route |
| `dav.password`, `dav.api_key` | **clear in `GET /api/config` and `/config/dav`** | mask; write-only semantics |
| `proxy_url` userinfo | clear | mask the `user:pass@` part |
| RSS feed URLs with `apikey=` | clear | mask query values whose name matches `api`/`key`/`token`/`pass` in list views; full value on the edit form only |
| Web login password | **plaintext in `credentials.json`** | hash with Argon2id (new workspace dependency `argon2`). Migrate on next successful login or at startup by hashing the stored plaintext. The file keeps 0600 |
| `config.toml`, backups | default umask | create as 0600 (the file holds server passwords) |

There is no encryption at rest. The key would have to sit next to the file, so
it would add complexity without protection (SAB stores passwords in plaintext too, §3.5). Env
`…_FILE` lets operators keep secrets out of the file entirely. Config is never
logged: the audit line (§8.4) records key names, and records values only for
non-secret keys.

**API key bootstrap:** a **new** config (no file existed) gets a random
`api_key` at creation, as SAB does on first run. An **existing** config without
a key keeps working unchanged, because setting one would break arr clients
configured with no key. The GUI shows a persistent warning, "SABnzbd API is
open: anyone who can reach port 9090 can control rustnzb", with a one-click
*Generate key*.

### 8.2 Who may change what

There are three credentials today: a web session (login → bearer token), Basic
auth with the web credentials, and `X-Api-Key`, the **same** key the
arr apps use for `/api`. Anyone holding that key can drive the whole native
API, config included.

Proposal: classify every setting in the registry:

- **Normal**: speed, concurrency, retention, categories, feeds, servers, DAV
  auto-send, log level. Writable by session, Basic auth, or `X-Api-Key`.
- **Sensitive**: credentials and keys (`api_key` rotation, DAV credentials,
  server passwords via export), `fetch_allow_private`, `fetch_allowed_hosts`
  (SSRF reach), `scripts_dir`, `script_*` (code execution),
  `listen_addr`/`port` (exposure), backups restore/import/export-with-secrets.
  These need an **interactive session plus step-up**: the request carries
  `X-Confirm-Password`, checked against the web credentials, or a session
  re-authenticated within the last 5 minutes. They are **never** writable
  through `X-Api-Key` or the SAB API. This matches the intent of SABnzbd's
  `nzb_key` versus `api_key` split, and goes further because rustnzb runs
  scripts by path.
- **Bounding**: settings that limit what the GUI may do:
  `RUSTNZB_ALLOWED_ROOTS`, `RUSTNZB_ALLOWED_HOSTS`, `RUSTNZB_CONFIG_MANAGED`, `RUSTNZB_ALLOW_REMOTE_SETUP`
  (§9, §10). They are **env/CLI only**, never in the file and never in the API.
  An operator sets the fence outside the thing it fences.

Optional (decision D9): a separate add-only `nzb_key`, as in SAB, so indexers
and browser extensions can add NZBs without holding the full key.

### 8.3 Transport and browser safety

- No CORS layer, and bearer tokens are not ambient credentials, so CSRF against
  the native API is not an issue today. Keep it that way: no cookie auth for
  config writes.
- **DNS rebinding** matters while the SAB API is keyless or before setup: a
  hostile page can resolve a hostname to the LAN IP and post to `/api`. Add a
  `Host` allowlist check (SAB's `host_whitelist` equivalent): accept IP
  literals, `localhost`, and configured names; refuse others with 403. It is
  active only while no `api_key` is set or before setup. Bounding setting
  `RUSTNZB_ALLOWED_HOSTS`.

### 8.4 Audit

Each commit logs at `info`:
`config changed by=<session|api_key|file-reload> keys=[general.port, servers.news-1.password(secret)] restart_required=[general.port]`.
Values are logged for Normal keys only. The last 200 entries also go in a
SQLite `config_audit` table that the GUI can show ("History" panel), next to the
backups that can undo them.

---

## 9. Security of GUI-written paths and the bind address

**Paths** (`incomplete_dir`, `complete_dir`, `watch_dir`, category
`output_dir`, `scripts_dir`, `data_dir`):

- The real boundary is the OS user (`abc` in the image, `rustnzb` under
  systemd). Mitigations stop accidents and limit what a stolen session can do;
  they do not replace that boundary.
- Apply the path rules in §7 (item 2), the denylist, and `RUSTNZB_ALLOWED_ROOTS`
  (colon-separated). When it is set, every path setting and
  `GET /api/browse-directory` are confined to those roots after
  canonicalisation. In the Docker image, default it to
  `/downloads:/data:/config` **only when the operator opts in** (D13); setting
  it by default would change behaviour for users with extra mounts.
- `data_dir` is **restart-only and Sensitive**. Changing it moves the DB, lock
  and credentials. The GUI offers it only on bare-metal installs, and only with
  "Move existing data" or "Start fresh". In Docker it is CLI-locked
  (`--data-dir /data`) and shown as "Set by the container".
- `scripts_dir` and scripts are Sensitive (§8.2), because a writable script
  path is code execution. When `RUSTNZB_ALLOWED_ROOTS` is set, scripts must
  also resolve inside `scripts_dir`. That is enforced already when
  `scripts_dir` is set.
- `browse-directory` already exists. Confine it as above, and never follow
  symlinks out of the allowed roots.

**Bind address and port:**

- They are restart-only, and they are Sensitive.
- The GUI warns when moving from `127.0.0.1` to `0.0.0.0` or `::` ("This
  exposes rustnzb to your network") and blocks the change when no web
  credentials exist.
- **Lock-out recovery** is always possible. The CLI and env override the file
  (`RUSTNZB_LISTEN_ADDR=127.0.0.1 rustnzb`), and the new `rustnzb config set
  general.port 9090` / `rustnzb config reset-network` subcommands edit the file
  offline (Phase 9).
- **In a container** (detected by `RUSTNZB_CONTAINER=1`, set in the
  Dockerfile), changing `port` or `listen_addr` from the GUI almost always
  breaks the published port mapping. Show them read-only with "Change the port
  mapping in your compose file instead" (D10).

---

## 10. First run for non-Docker users

**Problems today:** Docker-shaped defaults (`/downloads/*`, `/data`) are
created eagerly, so a non-root run fails before the GUI exists. The Windows
installer runs `rustnzb.exe` with `config.toml` relative to the working
directory. The systemd unit's config lives in `/etc`, which the service user
cannot write. The desktop shell has platform paths but still uses the
`/downloads/*` defaults.

**Proposal:**

1. **Platform-aware defaults, applied only when no config file exists** (Phase
   7). When `RUSTNZB_CONTAINER` is set, the defaults are exactly today's, so
   Docker is unchanged. Otherwise:

   | Platform | config | data_dir | downloads |
   |---|---|---|---|
   | Linux user | `$XDG_CONFIG_HOME/rustnzb/config.toml` | `$XDG_DATA_HOME/rustnzb` | `~/Downloads/rustnzb/{incomplete,complete}` |
   | Linux systemd | `/var/lib/rustnzb/config.toml` (`StateDirectory`, unit sets `RUSTNZB_CONFIG`) | `/var/lib/rustnzb` | `/var/lib/rustnzb/downloads/*` |
   | Windows | `%APPDATA%\rustnzb\config.toml` | `%LOCALAPPDATA%\rustnzb` | `%USERPROFILE%\Downloads\rustnzb\*` |
   | macOS | `~/Library/Application Support/rustnzb/` | same | `~/Downloads/rustnzb/*` |

   The resolved defaults are written **explicitly** into the new file, so a
   later default change can never move an existing install's directories. An
   explicit `--config` still wins. The `directories` crate is already used by
   the desktop shell, so add it to the workspace table.
2. **Never crash on directory creation at startup.** If a download dir cannot
   be created, start anyway in a "needs attention" state. The queue is held, and
   the GUI's first screen is the paths step with the OS error shown. `data_dir`
   remains fatal, since it holds the DB.
3. **Bind safely by default for new non-container configs:** `127.0.0.1`. The
   wizard offers "Allow access from other devices on my network" (→ `0.0.0.0`).
   Containers keep `0.0.0.0`.
4. **Close the first-boot claim race.** Today, whoever reaches `/auth/setup`
   first owns the instance. On first boot, when the listener is not loopback,
   rustnzb prints a one-time **setup code** to the log and console, as Jupyter
   does. `/auth/setup` requires it from non-local clients; loopback and
   `RUSTNZB_ALLOW_REMOTE_SETUP=1` skip it. The Docker user reads it from
   `docker logs` (D11).
5. **Wizard steps** (extending `features/welcome`, which today offers SAB
   import, "Set up manually" or skip):
   1. Create the web login (exists), with the setup code if required.
   2. Network: bind choice, shown only when not in a container.
   3. Usenet server: add and **Test** (existing `test-config` endpoint), or
      *Import from SABnzbd* (existing ini/API import).
   4. Folders: incomplete/complete with the directory browser and a live
      writability probe.
   5. Categories: keep the defaults or import them.
   6. Finish: the generated API key is shown with copy buttons for Sonarr and
      Radarr, and a link to settings.
6. **Packaging follow-ups:** the systemd unit moves to
   `RUSTNZB_CONFIG=/var/lib/rustnzb/config.toml` and migrates
   `/etc/rustnzb/config.toml` if present (copy, leaving the original). The
   Windows installer gains a Start-menu shortcut that passes `--config
   "%APPDATA%\rustnzb\config.toml"`, or relies on (1).

---

## 11. Migration and compatibility

| Install type | What changes on upgrade | Behaviour change? |
|---|---|---|
| Docker, env vars only, never touched the GUI | nothing; the file layer is whatever is there (default file) | **none** |
| Docker, env vars + GUI edits | env-locked fields become read-only in the GUI (before: editable and ignored). Later saves stop copying env values into the file | none to the effective config. Values copied in by earlier versions stay in the file (harmless: env still wins); the GUI offers *Clean up*, which removes file keys equal to their env override |
| GitOps, read-only `config.toml` mount | treated as managed (§5.2); GUI read-only instead of 500s on save | none to the effective config; the GUI gets clearer |
| `RUSTNZB_DAV_ENABLED=1` | no longer written to the file at startup; becomes a normal override | none while the var is set. Removing it later turns DAV off unless the file says `enabled = true`, which every earlier version already wrote, so in practice there is no change |
| Bare metal / systemd with an existing file | none (platform defaults apply only when no file exists) | none |
| New non-Docker install | platform defaults, `127.0.0.1` bind, API key generated, setup code | new behaviour for new installs only |
| New Docker install | API key generated; setup code printed if D11 says so | new installs only |

**Opting in to GUI management** needs no migration tool for Docker users: they
remove the env vars they no longer want, and those fields become editable. To
make that painless, the GUI's lock tooltip offers **"Copy to config file"**,
which writes the current override value into the file layer. Removing the
variable then changes nothing.

**Upgrading the file:** the `v0 → v1` migration only stamps `config_version`.
It runs under the backup rule (§5.4), so the first upgraded start leaves a
`config-backups/` snapshot of the original file.

**Rollback:** older binaries ignore `config_version` (serde `default`,
unknown-key tolerant) and keep working on a file written by the new version.
They do re-leak overrides on save, as they do today.

---

## 12. Phased implementation plan

Each phase is one small PR with its own tests, reviewable alone, in this order.
Phases 1–2 are safety fixes that stand on their own even if the rest is never
built.

| # | PR | Scope | Tests |
|---|---|---|---|
| 1 | `fix: mask secrets in config reads` | mask `api_key`, `dav.password`, `dav.api_key`, `proxy_url` userinfo in `GET /api/config` and `/config/dav`; DAV PUT keeps masked/absent secrets; secret-`null`-clears rule for servers/DAV; hide the dead `cache_size`/`log_file` from the API surface and warn when set; `--log-file` warns "unused" | handler tests: GET never contains a seeded secret; PUT with mask keeps the stored value; `null` clears |
| 2 | `fix: config writes are atomic, private and never persist overrides` | split `file` vs `effective` in `AppState`; `OverrideSet` from CLI/env/OTEL/DAV/desktop; `commit_config` saves `file` only; drop the DAV latch write; temp+fsync+rename, 0600, rolling backups; `config_version` stamp | property test: for random override sets, a save writes no override value; crash-safety test (tmp left behind is ignored); backup rotation test; DAV env no longer rewrites the file; regression: Docker-style `--data-dir` + GUI save leaves the file's `data_dir` untouched |
| 3 | `feat: settings registry` | `nzb-core::config::registry` with every leaf key; move bounds out of `handlers.rs`; `GET /api/config/schema`; generated section of `config.example.toml` | completeness test (every `AppConfig` leaf has an entry and vice versa); bounds parity with the old constants; example-config drift test |
| 4 | `feat: source-aware config API` | `GET /api/config/effective`, `PATCH /api/config` with `If-Match`, `POST /api/config/validate`, field-level 400s, `409 locked/stale_revision/file_changed`; existing granular endpoints delegate and gain the locked check; `pending_restart` in `/api/status` | locked-key refusal per override source; stale revision; hand-edit conflict; validate is side-effect free; utoipa snapshot |
| 5 | `feat(ui): every setting editable, with lock badges` | Settings page renders registry fields per tab (Paths, Post-processing, Telemetry, Network, Advanced); lock badge + "Copy to config file"; restart banner; category `cleanup_patterns`/`unwanted_extensions`; server `recv_buffer_size`/`trusted_fingerprint` under Advanced | component specs for locked/unlocked/pending; Playwright journey: change a live setting, change a restart setting → banner |
| 6 | `feat: restart, reload, managed mode, generalised env` | `POST /api/system/restart` (re-exec), `POST /api/config/reload` + SIGHUP, `RUSTNZB_CONFIG_MANAGED` / read-only file detection, `RUSTNZB__SECTION__KEY` + `_FILE` | integration: restart preserves queue; reload applies live keys and lists restart keys; managed mode refuses every write but leaves pause/resume working; env mapping table test |
| 7 | `feat: auth classes and step-up` | registry `class`; Sensitive keys need session + `X-Confirm-Password`; refused for `X-Api-Key`/SAB; audit log + `config_audit` table; Argon2id for `credentials.json` with in-place migration | auth matrix test (credential × class); plaintext→hash migration test; audit redaction test |
| 8 | `feat: path and bind safety` | path validation + probe + denylist + `RUSTNZB_ALLOWED_ROOTS` (also on browse-directory); bind warnings; container lock for port/addr (per D10); `Host` allowlist while keyless | unit tests per rule incl. symlink escape; Windows path cases |
| 9 | `feat: first-run for non-Docker installs` | platform defaults when no file exists; non-fatal download-dir creation; loopback default bind; API-key bootstrap for new configs; setup code; wizard steps; systemd unit and Windows installer updates; `rustnzb config set/reset-network` subcommands | startup tests per platform-default branch (container env set → old defaults exactly); setup-code gate tests; Playwright first-run journey |
| 10 | `feat: make cheap restart-only settings live; SAB get_config/set_config` | dirs for new jobs, abort/early-failure/completion/timeout/direct-unpack/nesting via atomics, restartable watcher; SAB `get_config` via registry, whitelisted `set_config`, `set_apikey` | engine tests that a changed value is used by the next job; SAB golden-fixture comparisons |

Dependencies: 2 → 3 → 4 → (5, 6, 7) → 8 → 9. Phase 1 can land first and
independently. Phase 10 needs 3–4. New dependencies across the plan:
`toml_edit` (Phase 2, if D2 = preserve), `argon2` (7), `directories` moved to
the workspace table (9).

---

## 13. Open decisions for the operator

Each has a recommendation; none blocks Phase 1.

| # | Decision | Options | Recommendation |
|---|---|---|---|
| D1 | Persisted store | (a) keep `config.toml`; (b) move config into SQLite; (c) switch to an ini file for SAB familiarity | **(a)**. Already in place, GitOps-friendly, `sabnzbd.ini` analogue; the DB stays for state |
| D2 | Preserve comments/formatting on GUI save | (a) `toml_edit`, preserving the user's document; (b) keep `toml::to_string_pretty`, which drops comments | **(a)**. GitOps users comment their files; today's saves wipe the comments from `config.example.toml` copies |
| D3 | Precedence model | (a) env/CLI as locked overrides above GUI-managed file; (b) env > file > GUI three-tier store | **(a)**. Same order as today; no new store |
| D4 | Env coverage | (a) generic `RUSTNZB__SECTION__KEY` for every scalar + `_FILE` secrets, lists stay file-only; (b) keep only today's few variables; (c) also index lists (`RUSTNZB__SERVERS__0__HOST`) | **(a)**. GitOps parity without the fragility of indexed lists |
| D5 | Restart mechanism | (a) in-process re-exec; (b) exit and rely on the supervisor; (c) no restart button, banner only | **(a)**. Works under Docker, systemd and bare runs |
| D6 | Is the speed limit config or runtime state in managed mode? | (a) runtime: stays changeable, persisted to the DB `settings` table while managed; (b) config: locked too | **(a)**. Users throttle from phones (nzb360); GitOps rarely pins it |
| D7 | Dead settings `cache_size`, `log_file` | (a) deprecate and hide now, implement later if wanted; (b) implement now; (c) remove | **(a)**. Implement `log_file` later if someone asks (stdout + Loki covers Docker) |
| D8 | SAB `set_config` | (a) whitelist of Normal keys only; (b) full parity; (c) read-only `get_config` expansion only | **(a)**. Helps nzb360-style clients without handing the arr key config power |
| D9 | Add-only `nzb_key` | (a) add, SAB-compatible; (b) defer | **(b)**. Defer until Phase 7 lands; it is a feature, not part of config management |
| D10 | Port/bind editable from the GUI inside a container | (a) read-only with mapping hint; (b) editable with a warning | **(a)**. A changed container port without a remapped host port is a lock-out |
| D11 | First-boot setup code | (a) required for non-loopback clients on new installs; (b) only when bound to `0.0.0.0` *and* not in a container; (c) no code, rely on the network | **(a)**, with `RUSTNZB_ALLOW_REMOTE_SETUP=1` as the escape hatch |
| D12 | Bootstrap an API key for existing keyless installs | (a) no, warn only; (b) generate on upgrade | **(a)**. (b) would break arr clients that send no key |
| D13 | Docker default for `RUSTNZB_ALLOWED_ROOTS` | (a) unset (opt-in); (b) default to `/downloads:/data:/config` | **(a)**. (b) breaks users with extra mounts, such as `/media` |

---

## 14. Verification of this review

- Every rustnzb claim above was read from source on branch `main` at
  `f119ea0`: `crates/nzb-core/src/config.rs`, `apps/rustnzb/src/main.rs`,
  `crates/nzb-web/src/startup.rs`, `crates/nzb-web/src/state.rs`,
  `apps/rustnzb/src/server.rs`, `apps/rustnzb/src/handlers.rs`,
  `crates/nzb-web/src/sabnzbd_compat.rs`, `crates/nzb-web/src/auth.rs`, the s6
  scripts, the systemd unit, the Windows installer, the desktop shell and the
  Angular settings/welcome components.
- Nothing was run against a live instance. The "secret in clear" findings come
  from reading the handlers (`masked_config` only masks server passwords;
  `h_dav_config_get` returns the struct as is). Phase 1 should start with tests
  that prove them.
- The SABnzbd section is from SABnzbd source and docs as cited there; items
  marked *unverified* were not confirmed in source.
