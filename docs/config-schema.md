# Settings schema

`GET /api/config/schema` returns the settings registry in
`nzb-core::config_registry`. Each entry has:

| Field | Meaning |
|---|---|
| `key` | Dotted path on `AppConfig`, for example `general.port`. |
| `kind` | Type and constraints (`bool`, `u64` with `min`/`max`, `path`, `secret`, `enum`, `string_list`, …). |
| `default` | Serde default. Secrets and unset options are JSON `null` or an empty string, never a live value. |
| `apply` | `live` or `restart`. |
| `restart_required` | `true` when `apply` is `restart`. |
| `class` | `normal` or `sensitive`. Sensitive matches `nzb-web::auth_class::SENSITIVE_KEYS` (WI-2497). Extra keys that are not yet on that list (`general.local_import_dir`, `general.trusted_proxies`, `general.step_up_ttl_secs`, notification URLs and tokens) are still `sensitive` here so a later writer treats them the same way. |
| `env` | Environment names, primary first. |
| `cli` | CLI flag, when one exists. |
| `group` | GUI section. |
| `label` | Short label. |
| `granular` | `true` when a granular endpoint already writes this key. |

The route sits under `/api`, so the same auth middleware as the other config
reads applies. The response is the static registry, not the current config.

Servers, categories, RSS feeds and schedule rules are lists with their own
endpoints and are not leaves in this registry. `schedule.enabled` is.

## Overlap with the granular endpoints

WI-2079 (beta.5) already edits paths, post-processing and network settings
through dedicated routes. Those keys stay in the registry (`granular: true`)
so one schema can render them, but writers must keep calling the existing
route. This registry does not add a second writer.

| Existing route | Keys |
|---|---|
| `PUT /api/config/general` | `general.incomplete_dir`, `general.complete_dir`, `general.watch_dir`, `general.direct_unpack`, `general.max_nested_archive_depth`, `general.scripts_dir`, `general.script_success`, `general.script_failure`, `general.script_timeout_secs`, `general.script_max_output_bytes`, `general.listen_addr`, `general.port`, `general.keep_history_on_arr_delete`, `general.data_dir` (refused, read-only) |
| `PUT /api/config/speed-limit` | `general.speed_limit_bps` |
| `PUT /api/config/history-retention` | `general.history_retention` |
| `PUT /api/config/max-active-downloads` | `general.max_active_downloads` |
| `PUT /api/config/disk-guards` | `general.min_free_space_bytes` |
| `PUT /api/config/debug-logging` | `general.log_level` (runtime toggle; the saved level still needs a restart) |
| `PUT /api/config/servers…` | `servers` (not a registry leaf) |
| `PUT /api/config/categories…` | `categories` (not a registry leaf) |
| `PUT /api/config/rss-feeds…` | `rss_feeds` (not a registry leaf) |
| `PUT /api/config/schedule` | `schedule` rules; `schedule.enabled` is a registry leaf |
| `PUT /api/config/dav` | `dav.*` (feature-gated; still listed so the schema is complete) |

Keys with `granular: false` have no granular writer yet. They are described
here so the GUI and a later patch can use one table. Notable ones:
`general.max_post_processing_jobs`, `general.max_repair_workers`,
`general.max_extract_workers`, `general.article_timeout_secs`,
`general.duplicate_policy`, `general.shutdown_timeout_secs`, `otel.*`,
`notifications.*`, `par2.*`.

`config.backup.restore` and `config.backup.export_secrets` are actions, not
fields, so they stay in `SENSITIVE_KEYS` and are not registry rows.
