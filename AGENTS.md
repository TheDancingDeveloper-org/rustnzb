# AGENTS.md

Guidance for AI coding agents (and humans who like checklists) working in the
rustnzb repository. This file is the entry point: read it before making
changes, and follow the linked documents for detail.

Agents are welcome contributors here. They are held to the same engineering
and community standards as anyone else — see the AI-assisted contributions
section of [CONTRIBUTING.md](CONTRIBUTING.md).

---

## Project in one paragraph

rustnzb is a Usenet downloader written in Rust: it takes an NZB and runs the
whole pipeline — pipelined NNTP downloads over TLS, SIMD yEnc decoding, PAR2
verification and repair, archive extraction — behind an Axum HTTP API and an
Angular web UI, shipped as a single static binary and a container image. It
also exposes a SABnzbd-compatible API so *arr applications and SAB clients can
drive it.

---

## Repository map

| Path | What lives there |
|------|------------------|
| `apps/rustnzb/` | The binary crate: startup, config wiring, systemd unit, integration tests |
| `apps/rustnzb/frontend/` | Angular web UI (Node 22, npm) |
| `crates/nzb-core/` | NZB parser, config, SQLite database, shared models |
| `crates/nzb-news/` | Download orchestration primitives, queue/worker coordination |
| `crates/nzb-dispatch/` | Server-aware dispatch engine feeding article work to `nzb-news` |
| `crates/nzb-nntp/` | NNTP protocol, connection pool, rustls TLS, pipelining, failover |
| `crates/nzb-decode/` | yEnc decoder, CRC32 validation, file assembler |
| `crates/nzb-postproc/` | PAR2 verify & repair, RAR/7z/ZIP extraction, cleanup |
| `crates/nzb-web/` | Axum server, REST API, SABnzbd compatibility layer, queue manager |
| `crates/mock-nntp-server/` | Deterministic NNTP fixture used by tests |
| `e2e/` | Playwright browser journeys (`e2e/USER_JOURNEYS.md` describes them) |
| `benchnzb/` | Benchmark harness — excluded from the workspace, not a correctness gate |
| `desktop/` | Tauri desktop shell — excluded from the workspace |
| `ci/` | Checked-in containerized task interface (`./ci/run`) and coverage gates |
| `docs/` | Development, releasing, known issues, performance status |
| `website/` | Project site and browser demo |

Cargo workspace: `resolver = "3"`, Rust 2024 edition, MSRV 1.88.
`default-members = ["apps/rustnzb"]`; `benchnzb` and `desktop` are excluded and
must be built or tested with their own manifests.

The workspace crates are also published to crates.io and patched back to their
local paths via `[patch.crates-io]` in the root `Cargo.toml`. If you change a
crate's public API, bump its version in `Cargo.toml` alongside the change.

---

## Build and test

Run from the repository root. These are the checks a change is expected to
pass before review:

```bash
cargo fmt --all --check
cargo check --workspace --all-targets
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo test --manifest-path benchnzb/Cargo.toml --all-targets --locked
```

Frontend:

```bash
cd apps/rustnzb/frontend
npm ci --no-audit --no-fund
npm test -- --watch=false
npm run build -- --configuration=production
```

For exact CI parity in pinned toolchain images (requires Docker):

```bash
./ci/run fmt
./ci/run check
./ci/run test
./ci/run clippy
./ci/run frontend-test
./ci/run e2e
```

Full task list: [docs/DEVELOPMENT.md](docs/DEVELOPMENT.md).

Scope your loop while iterating — `cargo test -p nzb-decode` is a far better
inner loop than the full workspace — but run the full set before you claim a
change is green.

---

## Conventions

- **Rust 2024 edition.** Clippy runs with `-D warnings`; the workspace also
  sets `clippy::all = warn` and `rust::unused = warn`. Do not silence a lint
  with `#[allow]` without a comment explaining why.
- **No new dependencies without cause.** Prefer the versions already pinned in
  `[workspace.dependencies]` and add new crates through that table, not
  per-crate.
- **Errors:** `thiserror` for library crates, `anyhow` at the application
  boundary.
- **Async:** Tokio throughout. Avoid blocking calls on the async runtime; use
  `spawn_blocking` for CPU- or filesystem-heavy work.
- **Tests belong with their code.** Unit and integration tests live with their
  crates and under `apps/rustnzb/tests/`; browser journeys live in `e2e/`.
  Use `crates/mock-nntp-server` rather than reaching for a real provider.
- **Determinism.** Tests must not depend on the network, on wall-clock timing,
  or on a live Usenet server.
- **Generated output is never committed.** `target/`, `.ci-output/`,
  `.ci-artifacts/`, `node_modules/`, and frontend build directories stay out of
  commits.

---

## Compatibility surfaces

Two API surfaces have external consumers and must not be changed casually:

- **`/api/...`** — the native REST API, documented at `/swagger-ui` via utoipa
  annotations. Update the annotations when you change a handler.
- **`/sabnzbd/api` and `/api?mode=...`** — the SABnzbd compatibility layer in
  `crates/nzb-web/src/sabnzbd_compat.rs`. Sonarr, Radarr, Lidarr, Readarr,
  Prowlarr, and mobile clients such as nzb360 depend on response *shape*, not
  just status codes. Treat SABnzbd's documented responses as the contract:
  add fields with SAB-compatible defaults rather than omitting them, and check
  changes against the SABnzbd 5.0.4 golden responses in
  `crates/nzb-web/tests/fixtures/sabnzbd-5.0.4/` (plus the integration tests in
  `apps/rustnzb/tests/sab_*.rs`).

Config is TOML with CLI and environment overrides, resolved in the order
CLI args > environment > TOML > defaults. When you add a setting, add it to
`apps/rustnzb/config.example.toml` too.

---

## Working with git and GitHub

- Branch from current `main`; keep each pull request focused on one change.
- Branch naming in use: `issue/<number>-<slug>`.
- Conventional commit prefixes: `feat:`, `fix:`, `perf:`, `test:`, `docs:`,
  `chore:`, `release:`.
- Add or update tests for behavior changes, and describe user-visible
  behavior, compatibility implications, and test coverage in the PR body.
- Rebase stacked branches on `main` before asking for review.
- Releases are cut from `main` — see [docs/RELEASING.md](docs/RELEASING.md).
  A release PR bumps versions only; it does not carry feature work.

---

## Boundaries

Things an agent should not do unprompted:

- Commit, push, tag, publish a crate, or cut a release without being asked.
- Modify `.github/workflows/`, `ci/`, or release tooling as a side effect of
  unrelated work.
- Weaken a test, coverage baseline, or lint gate to make a change pass.
- Commit credentials, API keys, NZB URLs, private hostnames, or personal
  paths — including in test fixtures and log excerpts.
- Reformat or restructure files beyond the scope of the change at hand.

Report results honestly: if tests fail, say so and paste the output; if you
skipped a check, say which one. Do not present unverified output as tested.

Security-sensitive findings go through [SECURITY.md](SECURITY.md), never the
public issue tracker.

---

## Work tracking — Vogt

This project is registered in **Vogt** as the single source of truth for its
work. Ask Vogt what is outstanding here; do not re-derive it from the repo.

| | |
|---|---|
| Slug | `rustnzb` (not `rustnzbd` — see below) |
| Instance | dev, `ins_01M019AZ8GBNE22FT74Y746JBK`, reachable at `http://127.0.0.1:8910/mcp` from MyDevEnv2 |
| Project id | `prj_01M04J3R35QZ2FHB04YKESAFDK` |
| Registered | 2026-08-16, via `vogt-onboarding/docs/IMPORT-PLAYBOOK.md` |
| Write-back | `none` — Vogt says nothing upstream on this repo |

**Names.** The directory is `rustnzbd`, the repo and the product are `rustnzb`.
The trailing `d` is a local directory artefact and is deliberately not carried
into Vogt. `rustnzbd` is also a GHCR/Forgejo image name; no repository by that
name exists.

**Canonical remote.** `https://github.com/TheDancingDeveloper-org/rustnzb`.
The older `AusAgentSmith-org/rustnzb` URL still resolves, because GitHub follows
transfer redirects and returns `200` — it is a redirect, not a home. Both `origin`
and `destination` in this checkout already point at the canonical owner.

**Other copies.** This working tree is the one people develop in, which is why
the project was `register`ed rather than `import`ed — `project import` would have
manufactured a second server-side checkout. A record for this project also exists
on the separate production Vogt instance (`ins_01KZTQ95C4Y7DGHEAK42M0TBR3`); the
dev instance above is the destination by owner decision of 2026-08-16, and the
production record is out of scope.

**The monorepo layout is deliberate.** This repository was converted to a
monorepo with its crates internal. `crates/` and `apps/` are the code; there is
no root `src/` and there is not going to be one. Note also that the root
workspace carries `exclude = ["benchnzb", "desktop"]`, and `fuzz/` is not a
member either — those three are separate Cargo workspaces nested in this
repository that reach back into the main workspace's crates by relative path.
That is intended, and any tool reasoning about this tree needs to handle it.

**Contract status: `non_compliant`, and accepted as such.** Contract v1 fails on
three criteria — no root `LICENSE` file (though `Cargo.toml` declares
`license = "MIT"`), no `design/`, and no `src/`. The last is structural, and
follows from the monorepo layout above: it is the intended end state, not a
migration anyone is waiting to see finish. Do not create a `src/` to score a
pass. Tracked as Vogt `WI-1`.

**Known collector noise.** The first drift pass opened 30 `unresolved_dependency`
proposals, every one of them an intra-workspace Cargo path dependency
(`../nzb-core`, `workspace:.`, `crates/…`) that resolves inside this repository.
They are not missing projects and must not be accepted as such — accepting one
asserts the target is not a project, which is false for every one of them.
Tracked as Vogt `WI-2`. `gh-posture` also fails on this project with a
`JSONDecodeError` (`WI-4`).

---

## Further reading

- [README.md](README.md) — features, install, configuration, API overview
- [CONTRIBUTING.md](CONTRIBUTING.md) — workflow and AI-assisted contributions
- [docs/DEVELOPMENT.md](docs/DEVELOPMENT.md) — prerequisites and full task list
- [docs/RELEASING.md](docs/RELEASING.md) — release process
- [docs/KNOWN_ISSUES.md](docs/KNOWN_ISSUES.md) — confirmed user-visible limits
- [docs/PERFORMANCE_STATUS.md](docs/PERFORMANCE_STATUS.md) — perf baseline and gate
- [e2e/USER_JOURNEYS.md](e2e/USER_JOURNEYS.md) — browser coverage
- [CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md)
