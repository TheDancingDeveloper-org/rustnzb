# Settings audit (WI-2079)

Where each setting is changed. The `[general]` and `[par2]` rows are enforced
by the `settings_audit_covers_every_general_and_par2_setting` unit test
(`SETTINGS_AUDIT` in `apps/rustnzb/src/handlers.rs`): adding a setting without
a row fails the build.

"Restart required" settings are saved at once and listed by
`GET /api/config/restart-pending` (and in the `PUT /api/config/general`
response) until the process restarts; the settings UI shows a
"Restart pending" banner while the list is non-empty.

## Editable in the web UI

| Setting | Tab | Applies |
|---------|-----|---------|
| `general.complete_dir`, `general.incomplete_dir` | Paths & disk | new jobs only, at once |
| category `output_dir` | Categories | new jobs only, at once (with an *arr path-mapping warning) |
| category `cleanup_patterns`, `unwanted_extensions` | Categories | at once |
| `general.watch_dir` | Paths & disk | restart required |
| `general.min_free_space_bytes`, `general.abort_hopeless` | Paths & disk | at once |
| `general.direct_unpack`, `max_nested_archive_depth`, `max_post_processing_jobs`, `max_repair_workers`, `max_extract_workers` | Post-processing | restart required |
| `par2.defer_volumes`, `par2.trust_inflight_verify` | Post-processing | at once |
| `general.scripts_dir`, `script_success`, `script_failure`, `script_timeout_secs`, `script_max_output_bytes` | Post-processing | at once |
| `general.listen_addr`, `general.port` | API | restart required |
| `general.api_key` (regenerate) | API | at once |
| speed limit, concurrent jobs, history retention, debug logging | General | at once |
| servers, RSS feeds, schedule, DAV | their tabs | as before |

Directory changes are checked inside the container before they are saved:
the directory must exist (it is never created) and a test file must be
created and removed in it. Otherwise the save is refused with, for example,
"`/data/media` does not exist in the container (not mounted?)" or
"`/data/media` is not writable by uid 911".

## Not editable in the web UI, and why

| Setting | Reason |
|---------|--------|
| `general.data_dir` | Holds the database and the config itself; moving it needs the files moved first. Shown read-only; set with `RUSTNZB_DATA_DIR` / `--data-dir`. |
| `general.local_import_dir` | Security boundary for SABnzbd `addlocalfile`; widening it from the UI would let a stolen session read arbitrary files. |
| `general.fetch_allow_private`, `general.fetch_allowed_hosts` | SSRF guard; opening server-side fetches to the LAN from the UI would let a stolen session pivot into the network. |
| `general.log_level` | The tracing filter is built at startup from `--log-level` / `RUSTNZB_LOG_LEVEL` / `RUST_LOG`; the debug-logging toggle covers the runtime need. |
| `general.log_level_before_debug` | Internal state of the debug-logging toggle. |
| `general.cache_size` | Dead setting: nothing reads it. |
| `general.log_file` | Logging starts before the config can be edited; read at startup, with `--log-file` / `RUSTNZB_LOG_FILE` winning. |
| Base URL (sub-path) | Not a setting: rustnzb does not support serving under a sub-path yet. Serve it at the root of its host. |
| `--config` / `RUSTNZB_CONFIG`, `--log-format` / `RUSTNZB_LOG_FORMAT` | CLI/env only: needed before the config file is read (the config path) or before logging starts. |
| CLI/env overrides (`--port`, `--listen-addr`, `RUSTNZB_*`) | An override wins over the saved value by design; the UI notes this. |
| `general.queue_complete_pers` | Follow-up: SABnzbd queue-complete persistence, used only by SABnzbd clients. |
| `general.group_max_headers` | Follow-up: newsgroup browsing settings get their own panel. |
| `general.shutdown_timeout_secs` | Follow-up: process lifecycle setting, rarely changed; restart required. |
| `[otel]` (telemetry endpoints) | Follow-up: needs its own endpoint; restart required. The Telemetry tab stays informational. |
| `[notifications]` (webhook, Apprise, ntfy) | Follow-up: needs its own endpoint with secret masking. See `docs/NOTIFICATIONS.md`. |
