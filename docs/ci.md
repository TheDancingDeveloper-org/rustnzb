# CI: pull requests, the merge queue, and `main`

A pull request gets fast feedback on what it changed. The merge queue proves
that the exact tree `main` will become still passes everything. Without a
queue, every PR ran the whole suite and each merge forced sibling PRs to rebase
and run it all again.

## What runs where

| Event | `ci.yml` | `codeql.yml` | `policy` / `runner-policy` | `dependency review` |
|---|---|---|---|---|
| `pull_request` | the Rust crates the diff touches, plus a changed frontend (below) | **not run** | full | reviews the PR's dependency changes |
| `merge_group` (the queue) | **everything**, including the quarantined `e2e` and `desktop` | **not run** | full | posts the context; the review already ran on the PR |
| `push` to `main` | — (no trigger) | **not run** | full | — |
| `v*` tag push (`release.yml`) | — | **both languages**, via `workflow_call` | — | — |
| `workflow_dispatch` | everything | both languages | — | — |

Two workflows also run outside a merge: `release-dry-run.yml` on a PR that
touches the release tooling and the packaging paths, nightly on `main` (see below), and
`quality-schedule.yml` on its own schedule.

### No run on push to `main`

`ci.yml` has no `push` trigger. A rebase merge from the queue lands the exact
tree that the `merge_group` run already tested, so a push run on `main` would
repeat the full suite with the same result. Nothing in these workflows publishes
or fills a cache on push (the image is pushed only when `RUSTNZB_PUSH=true`,
which no workflow sets), so no job needs the trigger.

### CodeQL runs only when a release is cut

CodeQL does not run on pull requests, the merge queue, pushes to `main` or a
schedule. `codeql.yml` triggers on `workflow_call` and `workflow_dispatch`
only. `release.yml` calls it as its `codeql` job on every `v*` tag push, so dev,
beta and prod builds each analyse the tagged tree. The job does not gate the
release build; it runs in parallel with `verify-release-ref` and the image build.

Findings from a release build land under Security > Code scanning, and the
default-branch baseline is whatever the last release analysed. To analyse a
commit without cutting a release, run `workflow_dispatch` on `codeql.yml`.

Because CodeQL no longer runs on PRs or the queue, its `Analyze (rust)` and
`Analyze (javascript-typescript)` contexts never post there. Any ruleset or
classic protection that still requires them will block merges. Remove them from
the required checks before relying on this trigger set (see below).

### Path scoping on a pull request

`ci.yml`'s `classify changes` job diffs the PR against its merge base:

| Changed paths | Runs |
|---|---|
| `crates/nzb-core/`, `nzb-decode/`, `nzb-decode/ffi/`, `nzb-dispatch/`, `nzb-news/`, `nzb-nntp/`, `nzb-postproc/`, `nzb-web/`, `mock-nntp-server/` | the full workspace test set, scoped with `-p <crate>` |
| `apps/rustnzb/` | the same set with `-p rustnzb` |
| `apps/rustnzb/frontend/` | `frontend`, `container-smoke` |
| `.github/`, `ci/`, `Dockerfile*`, `.dockerignore`, `.cargo/`, `rust-toolchain.toml`, `Cargo.*`, `fuzz/`, `benchnzb/` | the full workspace |
| `packaging/`, `installer/` | nothing here; `release-dry-run.yml` builds them |
| `e2e/` | nothing; `e2e` is a queue-only job |
| `desktop/` | nothing; `desktop` is a queue-only job |
| anything else (docs, website, design) | nothing; the skipped jobs report success |

Workflows no longer use `paths-ignore`. A required check that never starts
leaves a PR or queue entry waiting forever; a skipped job reports success.

An unrecognised path under `crates/` or `apps/` (a new workspace member, or a
root-level file) is not narrowed: it takes the full workspace rather than
guessing. The same fallback applies when the classifier sees a Rust change but
can attribute it to no package.

### The required `rust` check

`rust` is required on PRs and in the queue, and keeps its context name in both.
On a pull request it is built to be quick:

- `Swatinem/rust-cache` restores `~/.cargo` and the workspace `target/`, keyed
  on the toolchain and the manifests and shared across the pool. PR runs only
  restore; the queue and manual runs save, so an untrusted PR cannot write the
  shared key.
- `cargo-nextest` runs the tests. It does not run doctests; every fence in the
  workspace is `text`, `toml` or `ignore`, so there is nothing to skip. Keep it
  that way, or add a `cargo test --doc` step.
- `cargo fmt --all --check` always runs over the full workspace, so formatting
  cannot depend on which crates the diff touched.
- The WebDAV feature test runs only when `rustnzb` is in scope (or on the full
  workspace), and the separate `benchnzb` workspace only on the full workspace.
- `CARGO_PROFILE_TEST_DEBUG=0` and a single build job keep the runner inside
  its memory limit; neither changes test behaviour.

The queue always runs the full workspace, so a PR-scoped run cannot let an
untested crate merge.

### Quarantined jobs

`e2e` and `desktop` run only on `merge_group` and `workflow_dispatch`, and both
are `continue-on-error: true` while they are failing on `main`
(`TODO(WI-2069)`). A known failure therefore records as a success and does not
block an otherwise green queue entry. Neither is a required check. Remove the
`continue-on-error` and move each back to a PR trigger once it is fixed; the
flaky-quarantine skill has the process.

### Release dry run

`.github/workflows/release-dry-run.yml` exercises the packaging path without
publishing anything. It runs on a PR that changes `.github/workflows/release*.yml`,
`ci/tasks/**`, `packaging/**` or `installer/**`, nightly on `main`, and on
`workflow_dispatch`.

It mirrors the `release-assets` job in `release.yml`: the same pinned
`CROSS_IMAGE`, the same `ci/tasks` build sequence (frontend, Linux, arm64,
Windows, packaging), the AppImage step, and `sha256sum -c` over the produced
`SHA256SUMS` file. It then uploads the packages as a workflow artefact. It does
not push an image, sign, attest, create a release, or upload to a package
registry, and it needs no write permission.

The tag it builds is derived from the workspace version (`v<version>`), so the
dry run validates packaging against the version a release would ship. The
`CROSS_IMAGE` digest must stay in step with `release.yml`; a comment in each
file names the other.

PR runs of the dry run are limited to branches in this repository, because the
build authenticates to `repo.indexarr.net` with `FORGEJO_CARGO_TOKEN`, which a
fork's run does not receive.

CodeQL classifies `*.rs` plus `Cargo.*` as Rust, and JS/TS/HTML sources plus
`package*.json` as `javascript-typescript`. Its release run analyses both
languages in full; the per-language scope check applies only to `pull_request`,
which CodeQL no longer triggers on.

### Concurrency

Every workflow that runs on PRs cancels in progress per ref (`github.ref`), so
a force-push cancels the superseded run. Each queue entry has its own
`gh-readonly-queue/main/pr-N-<sha>` ref, so entries never cancel each other.

### Runners

Queue runs use the same labels as PR runs: `self-hosted`, plus
`docker, publish` for e2e, desktop and container-smoke. The `rust` job runs on
the local node-f runners, `[self-hosted, Linux, X64, tailnet]`. It no longer
targets the `gcp-rust` spot runners: those scaled from zero, and jobs waited up
to 68 minutes for a runner. No workflow targets GCP or spot labels.

## Landing

Enqueue with `gh pr merge --auto --rebase` (or the workspace `land-pr`, which
detects the queue and follows the entry's `merge_group` runs). Do not rebase a
PR by hand while it is queued.

## Turning the queue on (operator)

The workflows support a queue but do not require one. Until a ruleset with a
`merge_queue` rule exists on `main`, PRs merge as before, with the narrower PR
checks. The ruleset (`POST repos/TheDancingDeveloper-org/rustnzb/rulesets`):

```json
{"name":"main merge queue","target":"branch","enforcement":"active",
 "conditions":{"ref_name":{"include":["refs/heads/main"],"exclude":[]}},
 "rules":[{"type":"merge_queue","parameters":{"merge_method":"REBASE",
   "max_entries_to_build":3,"min_entries_to_merge":1,"max_entries_to_merge":3,
   "min_entries_to_merge_wait_minutes":5,"grouping_strategy":"ALLGREEN",
   "check_response_timeout_minutes":60}}]}
```

Keep the classic protection's required checks `policy` and `rust`; both post on
`merge_group`. Do not require `Analyze (rust)` or
`Analyze (javascript-typescript)`: CodeQL no longer runs on `pull_request` or
`merge_group`, so a required `Analyze` context never posts and the PR or queue
entry waits forever.
