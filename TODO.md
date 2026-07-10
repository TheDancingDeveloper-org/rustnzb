# rustnzb — Remaining TODO

Status check against sabnzbd/review.md (March 2026). Items ordered by impact.

---

## Functional Gaps

1. [x] **URL import (addurl)** — SABnzbd compat handler is a stub (returns fake nzo_id, never fetches). Sonarr/Radarr use `addurl` for NZB indexer links. Wire up `reqwest` to actually download the NZB and enqueue it.
2. [x] **Queue reordering** — UI has move-up/move-down buttons that toast "not yet supported". Add `POST /api/queue/{id}/move` (or similar) and implement in `queue_manager.rs`.
3. [x] **Priority change after enqueue** — No endpoint to change a job's priority once it's in the queue. Add `PUT /api/queue/{id}/priority`.
4. [x] **Category CRUD via API** — Only `GET /api/config/categories` exists. Add create/update/delete so the UI and API consumers can manage categories without editing TOML.
5. [x] **Download resume on restart** — Queue persists across restarts but unfinished articles restart from scratch. Consider checkpointing per-file segment progress so partially-downloaded jobs don't re-fetch everything.

## Performance

6. [x] **SIMD yEnc decoder** — Replaced with published `yenc-simd` crate.

## API / Integration

7. [x] **Swagger UI wiring** — `utoipa` is in deps but verify the `/swagger-ui` route is actually mounted and working. If not, wire it up.
8. [x] **SABnzbd compat coverage** — Audit which `mode=` values Sonarr, Radarr, and Lidarr actually call. The compat layer covers the basics but may be missing edge cases (e.g. `mode=config`, `mode=get_cats`, `mode=change_cat`). Test with real arr instances.

## Operational

9. [x] **Graceful shutdown** — Verify that in-flight downloads are cleanly stopped and queue state is flushed to SQLite on SIGTERM/SIGINT. Important for Docker deployments.
10. [x] **Disk space checks** — Pre-flight check for available disk space before starting a download. Alert or pause if disk is critically low.
11. [x] **Docker health check** — Add a `/api/health` endpoint (or use `/api/status`) for `HEALTHCHECK` in Docker.

## Nice-to-Have (v2 territory per review.md)

These are explicitly deferred in review.md but worth tracking:

12. [x] Directory watching (watch folder for NZB files)
13. [x] RSS feed monitoring
14. [ ] File sorting / media renaming (guessit equivalent)
15. [ ] Notification system (apprise or similar)
16. [ ] External post-processing scripts
17. [x] Per-job bandwidth limiting
18. [ ] Scheduling (speed limits by time, pause/resume on schedule)

## GUI / Frontend Polish (2026-07-10 UI review)

Done this pass:

19. [x] History merged into `/downloads` as a collapsible panel below Active
    downloads instead of a separate tab (`queue-view.component.ts`).
20. [x] Failed/stuck jobs no longer vanish with no trace on manual delete —
    `remove_job` now writes a `Failed` history entry (preserving the error
    message) when the removed job was sitting in an error state
    (`queue_manager.rs`).
21. [x] Per-server connect timeout is now configurable in Settings → News
    Servers (`connect_timeout_secs`, default 30s) instead of relying on the
    OS-level TCP timeout. Wraps TCP+TLS+banner+auth as one budget
    (`nzb-nntp` `config.rs`/`connection.rs`).
22. [x] NNTP connection pool panel compacted from a per-server grid of up to
    40 cells down to one line per server (name, priority, a two-segment
    active/idle bar, counts).

All items below are done as of this pass:

23. [x] **Login/welcome screens reskinned to design tokens.**
    `login.component.ts`, `welcome.component.ts`, and
    `group-browser-dialog.component.ts` no longer hardcode the old
    GitHub-dark palette — all use `--bg`/`--panel`/`--accent`/etc. Welcome's
    primary action is blue now, matching the rest of the app.
24. [x] **Fake disk-usage bar fixed** — added `disk_space_total` to
    `/api/status` (via `statvfs`, Unix only — 0/unknown on other
    platforms), `diskUsedPct` now computes real usage and the bar/caption
    hide themselves when total is unknown instead of showing a bogus %.
25. [x] **Destructive-action confirmation standardized** — new
    `ConfirmService`/`ConfirmDialogComponent` (in-theme Material dialog)
    replaces every native `confirm()` call (bulk delete, history
    clear-all, delete server/category/feed/rule, WebDAV key regen) and
    single-job delete in the queue table now confirms too.
26. [x] Stray `--surface` token fixed → `var(--panel2)`.
27. [x] Loading state added to Queue and History tables (`loading()`
    signal, "Loading…" row shown until first response).
28. [x] Icon system unified — new shared `IconComponent` (small inline
    SVGs: close/play/pause/retry/chevron/drag-handle) replaces the
    Unicode glyphs across queue/history/media/groups/settings/app shell.
    Left a couple of prose mentions of another button's label as text
    (not real icon usage).
29. [x] Keyboard accessibility fixed on the pause-menu dropdown
    (`aria-haspopup`/`aria-expanded`, Escape-to-close returns focus to the
    trigger) and the mode-toggle (`aria-label`, `aria-pressed`).
30. [x] Group subscribe/unsubscribe now shows a snackbar on success/error
    and disables the star button while the request is in flight.
31. [x] Regenerating the WebDAV API key now confirms first, warning that
    connected clients will break — only when a key already exists (first
    generation needs no warning).
32. [x] Settings sidebar `position: static` vs. the mockup's `sticky` —
    checked git history: deliberately reverted in commit `c99c6f3`
    ("Fix remaining GUI regressions"), which also removed a capped
    `max-height`/`overflow-y: auto`. Not a regression — left as-is.
33. [x] Group header-fetch timeout now shows a snackbar ("taking longer
    than expected…") instead of silently reverting the button.
34. [x] History row-actions column widened to shrink-to-fit
    (`width:1%` + flex `nowrap`) instead of a fixed 140px that could clip.

## Backend fixes from the same review pass

35. [x] **Failed downloads no longer disappear with no trace.** Root
    cause: a job that hit `NoServersAvailable` was parked in `Paused`
    forever (the stall watchdog only scans `Downloading` jobs), and
    deleting it via `remove_job` wiped it with zero history record.
    `remove_job` now writes a `Failed` history entry (preserving the
    error message) when the removed job was sitting in an error state.
    Ordinary deletes of healthy jobs still just remove, no history
    clutter. (`crates/nzb-web/src/queue_manager.rs`)
36. [x] **Per-server connect timeout**, configurable in Settings → News
    Servers (`connect_timeout_secs`, default 30s), bounding the whole
    TCP+TLS+banner+auth sequence — previously unbounded, relying on the
    OS-level TCP timeout. (`crates/nzb-nntp/src/config.rs`,
    `connection.rs`)
37. [x] History merged into `/downloads` as a collapsible panel below
    Active Downloads (visible by default, state persists) instead of a
    separate Queue/History tab pair. `/queue` and `/history` still work
    as bookmarks. (`queue-view.component.ts`)
38. [x] NNTP connection pool panel compacted to one line per server
    (name, priority, a small active/idle bar, counts) instead of a grid
    of up to 40 cells per server.
