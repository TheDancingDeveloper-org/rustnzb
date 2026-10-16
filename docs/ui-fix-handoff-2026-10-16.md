# Handoff: rustnzb web UI fixes per DESIGN.md

**Branch:** `rustnzb-01M537GWN8XZ2VB3WY0H97RWX9` (pushed; three docs-only
commits on `origin/main`). Work in a worktree on this branch. Push the branch
only and open no PR. Oversight reviews and merges, and the operator deploys.

**Read first:**

1. `DESIGN.md` (repo root). This is the spec you implement. Tokens, type
   roles, layout, components, required states, Do's and Don'ts.
2. `docs/ui-audit-2026-10-16.md` has the findings with severities, screenshots
   in `docs/ui-audit-2026-10-16/`, the screen-to-source map, and the "Focused
   review: Settings typography" section.
3. `AGENTS.md` has the repo checks and conventions. It now points at
   DESIGN.md.

**Current state:** no UI code has changed. `styles.scss` still has only colour
tokens, and none of the fixes below are applied.

## Tasks (in order, one commit per green step, push after each)

1. **Tokens in `apps/rustnzb/frontend/src/styles.scss`**, matching DESIGN.md
   exactly:
   - Add `--line-strong`, `--accent-fill`, `--accent-fill-hover` and `--focus`
     to all three theme blocks. The values are in the DESIGN.md Colors table.
     Hover values: dark and light `#1d4ed8`, midnight `#5b4bd0`.
   - Change the Daylight `--mute` to `#5f6b7f`.
   - Add the shared tokens: `--font-mono`, the `--fs-*` sizes (meta 12, table
     13, body/section 14, title 18, stat 22), `--sp-1` to `--sp-6`,
     `--radius-sm/md/lg`, `--control-h: 34px`, and `--focus-ring`.
2. **Global rules in `styles.scss`:**
   - Add the `:focus-visible` ring, with a box-shadow ring on inputs, selects
     and textareas.
   - Make `code` use `font-family: var(--font-mono); font-size: 0.9em` instead
     of the absolute 12px.
   - Use `--line-strong` on `.form` and `.search-bar` control borders.
   - Set `.panel h3` to 14px, and move shared 10–11px text (pills, tags, `th`,
     `.hint`, `.article-failures`, `.form .help`) up to 12px.
   - Add `tabular-nums` to tables and `.card .val`.
   - Point `.btn.primary` and its hover at the `--accent-fill` tokens.
   - Add `.help` and `ul.code-list` (flex, wrap, `gap: var(--sp-1)`).
   - Breakpoints: `.cards4` goes to 2 columns at ≤1024px. At ≤720px, `.cards4`
     and `.cards3` go to 1 column and `.form` to a single column.
3. **Accent-fill on surfaces with white text.** These five places put white
   text on `--accent`:
   - `app.ts` `.pm-go`
   - `login.component.ts` `.submit-btn` and its hover
   - `settings-view.component.ts` `.btn.sm.active` (around line 1209)
   - `statistics-view.component.ts:133` `.periods .active`
   - `welcome.component.ts:305-306`

   Leave the queue-view pool bars and legend on `--accent`, since they carry no
   text.
4. **Settings typography** (`features/settings/settings-view.component.ts`):
   - Remove all 16 inline `style="font-size…"` attributes. Use
     `grep -n 'style="font-size\|font-size:1'`; the known spots are 540, 549,
     557, 974, 982, 1038, 1048 and 911.
   - Make the unit and help spans (630, 905, 911) `.help`.
   - Render the "Supported modes" codes (981–985) as `<ul class="code-list">`.
   - Add a Copy button for `sabnzbdExample` (978), reusing the existing
     `copy()`.
   - Rename the heading `tracing` to "Tracing".
   - Stop `.title.dim` (around 1182) from shrinking the font. Dim it only.
   - Stack the settings nav above the content at ≤720px.
5. **States and the Offline pill** (`app.ts`, around lines 66–72 and 478–490):
   - Add `offline` and `loaded` signals. Successful polls set
     `loaded=true, offline=false`, and the poll's `error` handler sets
     `offline=true`.
   - The pill shows `● Offline` with the `err` class when offline.
   - Speed, queued and free show `—` until `loaded` is true.
   - Update or add specs for this.
6. **History filter labels** (`features/queue/queue-view.component.ts`,
   history toolbar): add `aria-label`s to the filter input, the three selects
   and the select-all checkbox.

**Out of scope** (record as follow-ups, don't build):

- phone top-bar overflow menu
- queue card rows on phone
- per-panel error rows and loading skeletons
- per-state pill tokens
- the developer copy decision

These are listed under "Open decisions" in the audit.

## Verification

- Run from the frontend: `npm ci`, `npm test`, `npx ng build`. queue-view is
  already 128 B over its style budget, so don't add CSS there. Run
  `cargo fmt --check` only if you touch Rust (you shouldn't).
- Take before and after screenshots at 1440×900 and 390×844 of Downloads,
  Settings (General, Paths & disk, API & SABnzbd compat), Statistics and
  Search, in Rust dark and Daylight.
- For a local build with sample data, follow `demo/README.md`:
  1. Build with `--base-href=/demo/` into a scratch directory.
  2. Copy `demo/mock-api.js` into the build and load it before `main-*.js`.
  3. Serve with an SPA fallback to `/demo/index.html`.

  Chromium is at `~/.cache/ms-playwright/chromium-1248/chrome-linux64/chrome`,
  and Playwright is at
  `e2e/node_modules/playwright`.
- **Live instance** (the live Node B instance) is **read-only**. Get the
  login from the operator; it isn't in Cadastre or Infisical. In Playwright,
  abort every non-GET request except `/auth/login` and `/auth/refresh`. Never
  pause or resume, change settings or touch jobs.
- Re-check contrast after the changes:
  - white on `--accent-fill` ≥4.5:1
  - `--line-strong` against panel and panel2 ≥3:1
  - `--mute` on every surface ≥4.5:1
- Run the detector once at the end:
  `~/.claude/skills/impeccable/scripts/impeccable detect --json <changed files>`.

## Hand-back

Commit the after screenshots to `docs/ui-audit-2026-10-16/` with the prefix
`after-`. Tick off the "Planned low-risk fix batch" section in the audit doc.
Push, then report:

- branch and head SHA
- what was done and what remains
- the gates that were run, with results
- a verified / not-verified line
