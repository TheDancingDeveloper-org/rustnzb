---
name: rustnzb
description: Operator console for a Usenet downloader — dense, calm, truthful.
colors:
  # Rust dark (default theme). Midnight and Daylight override the same roles; see "Colors".
  bg: "#0f1419"
  panel: "#171d26"
  panel-raised: "#1e2632"
  line: "#2a3441"
  line-strong: "#6b7c92"
  text: "#e6edf3"
  mute: "#8b98a8"
  accent: "#3b82f6"
  accent-fill: "#2563eb"
  focus: "#60a5fa"
  ok: "#10b981"
  warn: "#f59e0b"
  danger: "#ef4444"
  postproc: "#a78bfa"
typography:
  page-title:
    fontFamily: "Inter, -apple-system, Segoe UI, Roboto, sans-serif"
    fontSize: "18px"
    fontWeight: 600
    lineHeight: 1.3
  stat-value:
    fontFamily: "Inter, -apple-system, Segoe UI, Roboto, sans-serif"
    fontSize: "22px"
    fontWeight: 600
    lineHeight: 1.2
    fontFeature: "tnum"
  section-title:
    fontFamily: "Inter, -apple-system, Segoe UI, Roboto, sans-serif"
    fontSize: "14px"
    fontWeight: 600
    lineHeight: 1.4
  body:
    fontFamily: "Inter, -apple-system, Segoe UI, Roboto, sans-serif"
    fontSize: "14px"
    fontWeight: 400
    lineHeight: 1.5
  table:
    fontFamily: "Inter, -apple-system, Segoe UI, Roboto, sans-serif"
    fontSize: "13px"
    fontWeight: 400
    lineHeight: 1.4
    fontFeature: "tnum"
  label:
    fontFamily: "Inter, -apple-system, Segoe UI, Roboto, sans-serif"
    fontSize: "13px"
    fontWeight: 500
    lineHeight: 1.4
  meta:
    fontFamily: "Inter, -apple-system, Segoe UI, Roboto, sans-serif"
    fontSize: "12px"
    fontWeight: 400
    lineHeight: 1.45
  overline:
    fontFamily: "Inter, -apple-system, Segoe UI, Roboto, sans-serif"
    fontSize: "12px"
    fontWeight: 500
    letterSpacing: "0.04em"
  code:
    fontFamily: "ui-monospace, Menlo, Consolas, monospace"
    fontSize: "0.9em"
    fontWeight: 400
rounded:
  sm: "4px"
  md: "6px"
  lg: "8px"
  pill: "999px"
spacing:
  "1": "4px"
  "2": "8px"
  "3": "12px"
  "4": "16px"
  "5": "24px"
  "6": "32px"
components:
  button:
    backgroundColor: "{colors.panel-raised}"
    textColor: "{colors.text}"
    rounded: "{rounded.md}"
    padding: "0 14px"
    height: "34px"
  button-primary:
    backgroundColor: "{colors.accent-fill}"
    textColor: "#ffffff"
    rounded: "{rounded.md}"
    padding: "0 14px"
    height: "34px"
  button-danger:
    backgroundColor: "{colors.panel-raised}"
    textColor: "{colors.danger}"
    rounded: "{rounded.md}"
    height: "34px"
  button-sm:
    typography: "{typography.meta}"
    rounded: "{rounded.md}"
    padding: "0 10px"
    height: "28px"
  input:
    backgroundColor: "{colors.panel-raised}"
    textColor: "{colors.text}"
    rounded: "{rounded.md}"
    padding: "0 10px"
    height: "34px"
  panel:
    backgroundColor: "{colors.panel}"
    rounded: "{rounded.lg}"
  status-pill:
    typography: "{typography.meta}"
    rounded: "{rounded.pill}"
    padding: "2px 8px"
---

# Design System: rustnzb

The web UI lives in `apps/rustnzb/frontend`. The tokens are implemented as CSS
custom properties in `apps/rustnzb/frontend/src/styles.scss`, and that file and
this document must agree. Change both in the same commit.

This guide was written from the incumbent UI and the
[2026-10-16 audit](design/audits/ui-audit-2026-10-16.md). Where the code still differs
from it, the guide is the target and the audit lists the gap.

## Overview

**Creative North Star: "The Operator Console"**

rustnzb is a tool people leave open on a second monitor or check from a phone
on the sofa. Each screen answers three questions: is it working, what is it
doing, and what needs me? The UI is dark-first, dense and quiet. Colour is kept
for state (downloading, post-processing, failed, paused), and the one accent
marks the action or selection that matters. Brand appears in precise details,
not decoration.

The most important property of the console is that it is **truthful**. A
number on screen is either a fact from the backend or explicitly marked as
unknown. Loading, empty, error and offline are different states and look
different. A default `0` is never shown as if it were data.

**Key characteristics**
- Dark-first with three themes (Rust dark, Midnight, Daylight) built on one set
  of role tokens.
- Flat tonal layers (bg → panel → raised) separated by hairlines. No decorative
  shadows.
- One sans family (Inter) on a seven-step role ramp, with monospace only for
  real code, paths and identifiers.
- Tabular numerals everywhere numbers update live.
- Operate-mode density: 13px tables, 34px controls, 4px spacing grid.

## Colors

The palette is a cool charcoal ground with one blue accent and four state hues.
All colours are **role tokens**: components reference `var(--role)` and never a
hex value.

### Roles (Rust dark / Midnight / Daylight)

| Token | Rust dark | Midnight | Daylight | Use |
|---|---|---|---|---|
| `--bg` | `#0f1419` | `#080d1a` | `#f3f6fa` | App ground |
| `--panel` | `#171d26` | `#10182a` | `#ffffff` | Panels, cards, top bar |
| `--panel2` (raised) | `#1e2632` | `#17223a` | `#eef2f7` | Inputs, buttons, chips, code, track of bars |
| `--line` | `#2a3441` | `#263552` | `#d2d9e3` | Dividers and panel borders only (decorative, <3:1 allowed) |
| `--line-strong` | `#6b7c92` | `#5d72a3` | `#7a8496` | Borders that identify a control: inputs, selects, checkboxes, outline buttons (≥3:1, WCAG 1.4.11) |
| `--text` | `#e6edf3` | `#edf1ff` | `#172033` | Primary text |
| `--mute` | `#8b98a8` | `#93a1bd` | `#5f6b7f` | Secondary text, labels, help (≥4.5:1 on every surface) |
| `--accent` | `#3b82f6` | `#8b7cf6` | `#2563eb` | Accent **text**: links, active nav, selected outline |
| `--accent-fill` | `#2563eb` | `#6a5ae0` | `#2563eb` | Accent **surfaces** under white text: primary button, active segmented control (≥4.5:1 with `#fff`) |
| `--focus` | `#60a5fa` | `#a89bff` | `#2563eb` | Focus ring only |
| `--accent2` (ok) | `#10b981` | `#22d3a6` | `#07875f` | Completed, healthy, enabled |
| `--warn` | `#f59e0b` | `#f7b955` | `#b45309` | Paused, degraded, attention |
| `--danger` | `#ef4444` | `#fb7185` | `#c6283d` | Failed, destructive actions |
| `--purple` | `#a78bfa` | `#c4b5fd` | `#7c3aed` | Post-processing (verify, repair, unpack) |

State pills (`.s-*`) use paired `--{state}-bg` / `--{state}-text` tokens,
defined in every theme. The existing `--ok-*` and `--fail-*` pairs are the
pattern. Downloading, queued, post-processing and paused still need their
pairs; until then their hex literals are a known gap.

### Named rules

**The Accent-Text / Accent-Fill Rule.** `--accent` is for text and outlines and
`--accent-fill` is for filled surfaces. Never put white text on `--accent`: on
Rust dark that is 3.68:1 and on Midnight 3.33:1, and both fail AA.

**The State-Owns-Colour Rule.** Green, amber, red and violet mean ok, warn,
fail and post-processing everywhere. Don't use them for decoration, category
tags or brand. A category tag is neutral (`--mute` on `--panel2`).

**The Hairline Rule.** `--line` separates surfaces. A control's boundary is
always `--line-strong`. If you can't see where to click, the border is wrong.

## Typography

**Sans:** Inter (fallback `-apple-system, 'Segoe UI', Roboto, sans-serif`)
**Mono:** `ui-monospace, Menlo, Consolas, monospace`, exposed as `--font-mono`

**Character:** neutral, compact and numeric. Inter's tabular figures keep live
speeds and sizes from jittering.

### Role ramp (the only sizes allowed)

| Role | Size / weight / line-height | Use |
|---|---|---|
| Page title | 18 / 600 / 1.3 | One per route, as `h1` |
| Stat value | 22 / 600 / 1.2, `tnum` | Card metrics (`.card .val`) |
| Section title | 14 / 600 / 1.4 | `.panel h3`, settings sections. Never smaller than the body under it |
| Body | 14 / 400 / 1.5 | Prose, form values, buttons, nav |
| Table / label | 13 / 400 (labels 500) | Table cells, form labels, secondary prose |
| Meta | 12 / 400 / 1.45 | Help, hints, units, subtitles, pills, tags, `.btn.sm`. **12px is the floor** |
| Overline | 12 / 500, uppercase, 0.04em tracking | Table headers and stat-card labels only |
| Code | `--font-mono`, **0.9em** | Inline identifiers, paths, env vars. Relative, so it follows its context |

### Named rules

**The No-Inline-Size Rule.** Never set `style="font-size:…"` in a template, and
never invent a size off the ramp. If text needs a different size, it has a
different role: use that role's class (`.help`, `.meta`, `.sub`).

**The Same-Role-Same-Look Rule.** Every help or unit string next to an input is
Meta, whether it's "bytes/sec", "GB · 0 = disabled" or "Max jobs in Downloading
state". Every section heading is Section title.

**The Real-Code Rule.** Monospace is for things a user would type or copy:
paths, env vars, API modes, URLs, hostnames. It is not a "technical" costume
for prose.

**The Copyable-Value Rule.** A value the user must paste elsewhere (endpoint
URL, API key) gets a Copy button. A list of codes is a list with real gaps
(`gap: var(--sp-1)`), never chips separated only by template whitespace.

## Layout

- **Spacing:** 4px grid, using tokens `--sp-1` (4) through `--sp-6` (32). Inside
  a group, use 4–8. Between groups in a panel, 12–16. Between panels, 20–24.
  Put more space above a heading than below it.
- **Container:** `.wrap` is max 1320px with a 20px gutter (compact mode), or
  full width with a 24px gutter (expanded mode). Main content starts 28px below
  the top bar.
- **Panels:** header 12/16 padding, body 14/16 padding (`.body.flush` for
  edge-to-edge tables).
- **Grids:** `.cards4` and `.cards3` use a 16px gap.
- **Forms:** `.form` is a two-column grid (180px label, 1fr control) with a
  10/14 gap. Help sits **under** its control in column 2 (`.form .help`), never
  in the label column.
- **Breakpoints:**
  - `≤ 1024px`: `.cards4` becomes 2 columns.
  - `≤ 720px`: `.cards4` and `.cards3` become 1 column (2 if values are short),
    `.form` becomes one column (label above control), and the settings nav
    stacks above content.
  - `≤ 480px`: the top bar collapses secondary actions into an overflow menu.
    Wide tables scroll inside their panel (`overflow-x: auto`), and the page
    itself never scrolls horizontally.
- **Touch:** interactive targets are at least 32px at desktop density and 44px
  at `≤ 720px`.

## Elevation & Depth

The UI is flat and tonal. Depth comes from the bg → panel → panel2 steps plus
1px `--line` borders. The only shadows are on transient overlays (menus,
dialogs, snackbars), using Angular Material's level-2 shadow. Surfaces don't
lift on hover. Hover changes the border to `--accent` or the row background to
`--row-hover`.

## Shapes

- `--radius-sm` 4px: tags, code, small inline chips.
- `--radius-md` 6px: buttons, inputs, selects, segmented controls.
- `--radius-lg` 8px: panels and cards.
- `999px`: status pills and filter chips.
- Borders are 1px. No coloured side-stripes (`border-left` accents) on cards
  or rows. The settings nav's active marker is the one exception, because it
  is navigation state.

## Components

### Buttons
- **Default (`.btn`):** `--panel2` fill, `--line-strong` border, `--text`,
  34px tall, `--radius-md`, Body type. Hover changes the border to `--accent`.
- **Primary (`.btn.primary`):** `--accent-fill` with white text. Hover is a
  10% darker fill. Use at most one per view region.
- **Danger (`.btn.danger`):** `--danger` text with a `--danger` border on
  hover. Destructive actions always go through `ConfirmService`.
- **Small (`.btn.sm`):** 28px, Meta type. Use in table rows and setting rows.
  In a setting row the Save button matches the control height (34px), not sm.
- **Disabled:** 0.5 opacity and `cursor: not-allowed`. Keep it in the tab
  order only if a tooltip explains why it is disabled.

### Inputs, selects, checkboxes
- `--panel2` fill, `--line-strong` border, `--radius-md`, 34px tall, Body type.
- **Focus:** the global focus ring (below). Components must not set
  `outline: none` without providing the ring.
- **Label:** every control has a `<label for>` or an `aria-label`. A
  placeholder is not a label.
- **Error:** `--danger` border and a Meta-size message under the control
  naming the problem and the fix.

### Focus ring (global)
`:focus-visible { outline: 2px solid var(--focus); outline-offset: 2px; }`.
Text inputs use `box-shadow: 0 0 0 2px var(--bg), 0 0 0 4px var(--focus)` so
it survives component `outline: none`. Never remove focus indication.

### Panels and cards
- `.panel`: `--panel`, a `--line` border and `--radius-lg`. `h3` is Section
  title with a right-aligned `.hint` (Meta, `--mute`).
- `.card` (stat card): an overline label, a Stat value with an optional
  `.unit` (Meta), a `.sub` (Meta), and an optional `.bar`.
- Don't nest cards inside cards.

### Tables (`table.data`)
- Table type, `tnum`, 9/12 cell padding, `--line` row dividers and
  `--row-hover`.
- Headers use Overline and are **left-aligned for text** and right-aligned for
  numbers. Don't centre a header over left-aligned content.
- Row actions go in one trailing column, not one column per icon.
- On phone the table scrolls inside its panel, or uses a card-row layout for
  the queue.

### Status pills
- `.status-pill.s-{state}` uses Meta type, a pill radius and paired bg/text
  tokens. The text always names the state, so it isn't conveyed by colour
  alone.
- The top-bar `.pill` shows system state: `● Live` (ok), `● Paused` (warn),
  `● Offline` (err) when the status poll fails. Before the first successful
  poll, values show `—`.

### Navigation
- Top bar: `--panel` with a bottom `--line`. Links are Body type in `--mute`,
  and the active link is `--text` with a 2px `--accent` underline.
- Settings nav: grouped list with Overline group labels. The active item has a
  `--panel2` fill.

### Required states (every data view)
| State | Treatment |
|---|---|
| Loading | Shell renders immediately. Panels show a skeleton or "Loading…" row. Numbers show `—` |
| Empty | One sentence on what's missing and the next action ("No downloads in queue. Click + Upload NZB…") |
| Error | An inline panel row naming the failure and that it is retrying. Never fall through to the empty state |
| Offline | Top-bar pill turns `● Offline`. Live values are dimmed and keep their last known value with a "stale" hint |

## Do's and Don'ts

### Do:
- **Do** reference role tokens (`var(--mute)`, `var(--accent-fill)`) for every
  colour, and add a token to all three themes before using it.
- **Do** use the type role ramp. 12px is the smallest text.
- **Do** keep `--line-strong` on every control border and `--line` on every
  divider.
- **Do** give every data view loading, empty, error and offline states that
  look different from each other.
- **Do** check new UI at 1440px and 390px, and in Rust dark and Daylight,
  before review.
- **Do** use `tnum` for numbers that change.

### Don't:
- **Don't** set inline `style="font-size|color|margin…"` in templates. Add or
  reuse a class.
- **Don't** put white text on `--accent`. Use `--accent-fill`.
- **Don't** write `outline: none` without the focus ring.
- **Don't** show a default `0`, `0%` or `0 B` before data has loaded or after a
  request failed.
- **Don't** use state colours (green, amber, red, violet) for decoration or
  categories.
- **Don't** separate inline chips with template whitespace alone.
- **Don't** add coloured side-stripe borders, decorative shadows, gradients or
  emoji as icons. Use `shared/icon.component.ts`.
