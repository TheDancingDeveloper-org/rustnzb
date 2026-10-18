# CI: pull requests, the merge queue, and `main`

A pull request gets fast feedback on what it changed. The merge queue proves
that the exact tree `main` will become still passes everything. Without a
queue, every PR ran the whole suite and each merge forced sibling PRs to rebase
and run it all again.

## What runs where

| Event | `ci.yml` | `codeql.yml` | `policy` / `runner-policy` | `dependency review` |
|---|---|---|---|---|
| `pull_request` | only the areas the diff touches (below) | only the languages the diff touches | full | reviews the PR's dependency changes |
| `merge_group` (the queue) | **everything** | both languages | full | posts the context; the review already ran on the PR |
| `push` to `main` | everything | both languages | full | — |

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
`package*.json` as `javascript-typescript`. A language with no change still
posts its `Analyze (<language>)` context, with its steps skipped. A change to
`codeql.yml` or `.github/codeql/` analyses both.

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

Keep the classic protection's required checks (`policy`, `rust`,
`Analyze (rust)`, `Analyze (javascript-typescript)`); all of them post on
`merge_group`.
