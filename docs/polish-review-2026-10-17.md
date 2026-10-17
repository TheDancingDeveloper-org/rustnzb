# Polish review (2026-10-17)

A read-only review of the web UI, the daemon and API, the docs and the packaging. It looks at quality of life, UX and how professional the project looks, and compares it with SABnzbd and NZBGet. No app code was changed.

**Out of scope, to avoid duplication:**

- Everything already covered by `docs/ui-audit-2026-10-16.md` and `docs/ui-fix-handoff-2026-10-16.md`: the Offline pill, contrast and focus tokens, breakpoints, the type scale, the pipeline stepper and button sizes.
- GUI-managed configuration and its persistence. These phases are planned in `docs/gui-config-design-2026-10-16.md` on the GUI-config design branch.

**How the findings were checked.** Findings came from three parallel code sweeps, followed by spot checks of the headline claims:

- `log_file` is unused.
- There is no webhook or Apprise support.
- The root compose file is personal.
- The page title is static.
- There are 20 `error: () => {}` handlers in the frontend.

Findings marked *(verify)* were found by grep and not by a line-by-line read. Confirm those before raising tickets.

**Key.** Impact is H, M or L. Effort is S (under a day), M (a few days) or L (a week or more).

---

## 1. Quick wins (high impact, small effort)

| # | Area | Finding | Where |
|---|------|---------|-------|
| 1 | Daemon | `--log-file` / `RUSTNZB_LOG_FILE` is parsed but never used. `tracing-appender` is a dependency but isn't wired up, so there is no file logging and no rotation. Wire up daily or size-based rotation with a retention count. | `apps/rustnzb/src/main.rs:41-42`, `apps/rustnzb/Cargo.toml:24` |
| 2 | Docs | The README quickstart runs `docker compose up -d` against the root `docker-compose.yml`, which is a personal deploy file. It uses a Docker Hub image instead of GHCR, maps port 9095 instead of 9090, hardcodes a host download path and TZ, adds a promtail sidecar, and healthchecks `/api/status` while the Dockerfile uses `/api/health`. The quickstart also copies the config to the repo root, but the compose file mounts `./config/`. Point the README at `docker-compose.example.yml` and move the personal file out of the repo. | `docker-compose.yml:3-35`, `README.md:57-69` |
| 3 | UI | When the queue or history fetch fails, the page shows the empty state ("No download history yet") instead of an error. Copy the error-and-retry pattern from Statistics. | `queue-view.component.ts:1242`, `history-view.component.ts:390,238`, `statistics-view.component.ts:127` |
| 4 | UI | The page title is static. Show live state from the status poll (for example "↓ 12.3 MB/s · 4 left – rustnzb" or "Paused – rustnzb") and add per-route titles. SABnzbd and NZBGet both do this, and users rely on it from a background tab. | `frontend/src/index.html:5`, `app.ts:493`, `app.routes.ts` |
| 5 | UI | The speed limit can only be set in Settings. Add a quick-limit popover on the speed pill in the header or on the queue card, like SABnzbd's header speed box. | `app.ts:76`, `queue-view.component.ts:71,1268`, `settings-view.component.ts:1752` |
| 6 | UI | First run with no servers only shows a passive line, and uploads are accepted silently even though they will stall. Add an app-level banner that links to `/welcome` or the servers settings, and a warning in the upload dialog. | `queue-view.component.ts:121`, `settings-view.component.ts:296`, `app.ts` |
| 7 | Docs | There are no screenshots in the README. Add 2-4 PNGs (queue, history, settings, mobile) under `docs/img/`. | `README.md` |
| 8 | Docs | There is no `CHANGELOG.md`, and the release job doesn't generate notes. Adopt Keep a Changelog, or generate it with git-cliff from the conventional commits the repo already uses. | `.github/workflows/release.yml` |

## 2. High impact, larger effort

| # | Area | Finding | Where |
|---|------|---------|-------|
| 9 | UI | Errors are swallowed silently by 20 `error: () => {}` handlers. The worst case is Settings: a failed load shows default values that look like real config, and the user can save them over the real config. Show an inline error with Retry, and block Save until the load has succeeded. | `settings-view.component.ts:1489-1872` (12 sites), `rss-view.component.ts:337-345`, `history-view.component.ts:348`, `logs-view.component.ts:148`, `welcome.component.ts:429`, `queue-view.component.ts:1217,1249,1256` |
| 10 | Daemon | There are no notifications at all: no webhooks, Apprise, SMTP, Pushover or ntfy. Both competitors ship notifications. The cheapest broad option is a generic webhook plus an Apprise URL on complete, fail and disk-full events. | `crates/nzb-core/src/config.rs`, `crates/nzb-web/` (grep finds nothing) |
| 11 | Daemon | There is no duplicate detection on name or hash. The idempotency key only catches the identical payload (`crates/nzb-web/src/error.rs:118`). Add a duplicate policy (pause, fail or tag) that matches SABnzbd's "Detect duplicate downloads". *(verify)* | `crates/nzb-core/src/db.rs` |
| 12 | Docs | There is no "Migrating from SABnzbd" guide and no feature comparison table, although `sabnzbd_import.rs` already exists. These are the main adoption levers for this category. | `README.md:93-117`, `crates/nzb-web/src/sabnzbd_import.rs` |
| 13 | Release | The release supply chain is thin. `provenance: false` is set, and there is no SBOM, no cosign signature and no build attestation. SHA256SUMS is the only integrity artefact. | `.github/workflows/release.yml:83,189` |

## 3. Medium impact, small effort

### Daemon and API

- Add a `--check-config` validate-only mode. Today the only one-shot mode is `--smoke-test` (`main.rs:44-46`).
- The default config path is a relative `config.toml` (`main.rs:19`), so it depends on the working directory. Resolve it to an absolute path and log that path at startup.
- A config file that doesn't exist is created silently with defaults (`crates/nzb-core/src/config.rs:436-446`), so a typo in `-c` starts a fresh instance with no error. At minimum log a warning, or require an explicit `--init`.
- Config errors are weak:
  - A TOML parse error has no file path in its context (`config.rs:433`).
  - There is no `deny_unknown_fields`, so misspelt keys are ignored.
  - There is no value validation for ports, paths or server entries.
- Make the startup banner more useful. Today it is only the version (`main.rs:416`). Add the config path, data dir, listen address, server count and the detected unrar/7z paths.
- Add a JSON log format option for log shippers. Today it is human-readable only (`main.rs:331`).
- Bound graceful shutdown with a timeout and log the in-flight jobs. Today a hung unpack step can block exit until SIGKILL (`apps/rustnzb/src/server.rs:551-573`).
- `/api/health` always returns `ok` (`handlers.rs:2545`). Add `/api/ready` with checks that the DB is open, the directories are writable and at least one server is reachable.
- The SABnzbd `warnings` mode reports only the disk hold, and `clear` does nothing (`crates/nzb-web/src/sabnzbd_compat.rs:1017-1022`). Feed it real WARN entries from `log_buffer.rs`. The *arr apps display these.
- Several SAB compat modes are missing or fall through to "Unknown mode" (`sabnzbd_compat.rs:970-1030, 2524-2536`). The ones worth adding first are `addlocalfile`, `get_files`, `queue&name=delete_nzf`, `change_opts`, `change_script`, `test_server`, `restart` and `rss_now`.
- Secrets are stored in plaintext `config.toml` with no 0600 mode enforced on save (`config.rs:420`). *(verify)* Also check whether `GET /config` masks them.

### UI

- Only the dropzone panel accepts a dropped `.nzb` (`queue-view.component.ts:207-210`). Elsewhere the browser opens the file. Add a window-level drop overlay in `app.ts` that hands off to `AddNzbService`.
- The hint says "paste a URL" (`queue-view.component.ts:189`), but there is no clipboard paste handler.
- Settings has no unsaved-changes guard (no `CanDeactivate`), so leaving the page discards edits silently.
- There is no PWA manifest, `theme-color` or apple-touch-icon in `index.html`. `public/` also lacks 512px and maskable icons. Being able to add the app to a phone home screen is cheap polish.
- Fonts load from the Google CDN (`index.html:10-13`). For a self-hosted tool that is a privacy issue, and it also breaks or slows air-gapped installs. Bundle Inter, JetBrains Mono and Material Icons locally.
- Async results aren't announced (no `aria-live`): bulk actions, upload progress (`queue-view.component.ts:267-274`) and the "N selected" count. Inline form errors in Settings, Welcome and Login lack `role=alert` and `aria-describedby`.
- Check that the theme defaults to `prefers-color-scheme` and respects `prefers-reduced-motion` (`theme.service.ts`). *(verify)*

### Docs and packaging

- Version sources disagree:
  - `desktop/package.json:4` is pinned at 0.1.0.
  - `installer/windows.nsi:1-2` defaults VERSION to `dev`.

  Single-source the version from the release tag.
- The OCI labels in the `Dockerfile` cover only title, source and revision. Add version, created, licenses, description and url. Document `PUID`, `PGID` and `TZ` in the README.
- The bare-metal requirements list `7z` but not `unrar` (`README.md:89`).
- There are no issue or PR templates and no CODEOWNERS (`.github/`).
- The systemd unit has no hardening (`apps/rustnzb/systemd/rustnzb.service`). Add `NoNewPrivileges`, `ProtectSystem=strict`, `ReadWritePaths=`, `PrivateTmp` and `ProtectHome`.

## 4. Medium impact, larger effort

- **Scheduler.** There is none. Only timed pause exists. Both competitors offer time-of-day speed limits, pause/resume and server enable/disable.
- **Bulk actions in History.** Add multi-select delete, "retry all failed" and "clear completed/failed" (`history-view.component.ts`).
- **Browser notifications.** Add an opt-in "notify on complete/failed" setting, driven from the existing status poll. This is separate from item 10.
- **Global keyboard shortcuts.** Add shortcuts for pause/resume, focus search, upload and `g d`/`g h` navigation, plus a `?` help overlay. Today there is no global key handler.
- **Consistent toasts.** Every mutation (queue bulk actions, RSS, settings save) should report success and failure through one helper. Offer Undo on deletes where the backend can restore the item. Today History uses `'Retrying…'` with a `'Close'` action (`history-view.component.ts:501`), and other pages differ.
- **Post-processing script environment.** Pass SAB-style positional arguments plus `SAB_*` and `NZBGet NZBPP_*` env vars, so existing user scripts work unchanged (`crates/nzb-web/src/queue_manager.rs:369-410`). *(verify)*
- **OpenAPI.** Publish a spec (for example with utoipa) for the roughly 100 native `/api` routes (`apps/rustnzb/src/server.rs:219-507`).
- **Backup and restore.** Provide a one-click export and import of config, DB and keys, plus a restart endpoint.
- **Configuration reference.** Write a complete reference that maps each TOML key to its env var and CLI flag. Today there is only `config.example.toml` and a table of seven env vars (`README.md:168-178`).
- **Distribution.** Release artefacts are deb, tar.gz and NSIS only. In order of expected value, add an Unraid Community Apps template, then AUR, Homebrew and winget/Scoop. Also confirm whether the desktop app ships dmg or AppImage builds.

## 5. Low impact

- The router has no `**` route, so an unknown path renders a blank shell (`app.routes.ts`).
- The Statistics error state has no Retry button and shows no data age (`statistics-view.component.ts:127`).
- There is no i18n scaffolding (`$localize`). Formatting goes through `core/format.ts` and doesn't use locale-aware `Intl`. This is cheap to start now and expensive to retrofit.
- `change_complete_action` is a no-op (`sabnzbd_compat.rs:994-997`). There is no action for when the queue finishes, such as running a script or shutting down.
- The root of the repo is cluttered:
  - `LOGOS/` (3.7 MB, randomly named PNGs) should move to `docs/assets/` or `website/img/`.
  - `mockup.html` and `mockups/` should go, or move to `docs/design/`.
  - `promtail.yml` belongs with the personal compose file.
  - `demo/` looks like built Angular output that duplicates `website/demo`.
- Move the internal working docs (the `docs/ui-audit-*`, `docs/ui-fix-handoff-*`, `PERFORMANCE_STATUS.md` and this file) to `docs/internal/`, so the public docs tree stays user-facing.
- README structure: shorten the `RUSTNZB_PORT` Docker note (`README.md:180-186`) and move "AI-friendly by design" (`README.md:258`) below the user-facing sections.

## 6. Already in good shape (no action)

**Web UI:** pause-for presets with a custom value, the drag-drop dropzone and drag-reorder in the queue, per-job priority, ETA and speed, queue bulk start/pause/delete, a themed confirm dialog, a welcome and import flow, single-item History retry, and empty states on every list page.

**Daemon:** a watched folder, disk-space guards, a server connection test, server health and stats, graceful SIGTERM handling, and a typed native API error.

**Packaging:** pinned image digests, unrar built with checksum verification, a container HEALTHCHECK, multi-arch (amd64/arm64) images, verified SHA256SUMS, and SECURITY, CODE_OF_CONDUCT and CONTRIBUTING docs plus Renovate.

## 7. Suggested order

1. Quick wins 1-8.
2. Silent-error cleanup (item 9), together with the settings unsaved-changes guard.
3. Notifications (item 10) and duplicate detection (item 11).
4. Strict config parsing, `--check-config`, readiness and the shutdown timeout.
5. The SABnzbd migration guide and comparison (item 12), the config reference and screenshots, as a "1.0 docs" pass.
6. Release supply chain (item 13) and distribution channels.
7. Scheduler, History bulk actions, keyboard shortcuts, PWA, OpenAPI.
