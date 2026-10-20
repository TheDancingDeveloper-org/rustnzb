# CI: pull requests, the merge queue, and `main`

A pull request gets fast feedback on what it changed. The merge queue proves
that the exact tree `main` will become still passes everything. Without a
queue, every PR ran the whole suite and each merge forced sibling PRs to rebase
and run it all again.

## What runs where

| Event | `ci.yml` | `codeql.yml` | `policy` / `runner-policy` | `dependency review` |
|---|---|---|---|---|
| `pull_request` | only the areas the diff touches (below) | **not run** | full | reviews the PR's dependency changes |
| `merge_group` (the queue) | **everything** | **not run** | full | posts the context; the review already ran on the PR |
| `push` to `main` | — (no trigger) | **not run** | full | — |
| `v*` tag push (`release.yml`) | — | **both languages**, via `workflow_call` | — | — |
| `workflow_dispatch` | everything | both languages | — | — |

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
| `.github/`, `ci/`, `Dockerfile*`, `.dockerignore`, `rust-toolchain.toml` | everything, as in the queue |
| `crates/`, `apps/` (outside the frontend), `benchnzb/`, `fuzz/`, `e2e/`, `packaging/`, `Cargo.*` | `rust`, `e2e`, `container-smoke` |
| `apps/rustnzb/frontend/` | `frontend`, `e2e`, `container-smoke` |
| `desktop/` | `desktop` |
| anything else (docs, website, design) | nothing; the skipped jobs report success |

Workflows no longer use `paths-ignore`. A required check that never starts
leaves a PR or queue entry waiting forever; a skipped job reports success.

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
`docker, publish` for e2e, desktop and container-smoke. No workflow targets
GCP or spot labels.

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
