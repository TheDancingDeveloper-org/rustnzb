# Development

## Prerequisites

- Rust 1.95 or newer
- Node.js 22 for the Angular frontend
- Docker and Docker Buildx for container and browser-task workflows
- `7z` for archive extraction at runtime

## Core checks

Run these from the repository root before opening a pull request:

```bash
cargo fmt --all --check
cargo check --workspace --all-targets
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo test --manifest-path benchnzb/Cargo.toml --all-targets --locked
```

## Frontend checks

```bash
cd apps/rustnzb/frontend
npm ci --no-audit --no-fund
npm test -- --watch=false
npm run build -- --configuration=production
```

CI additionally gates frontend coverage against `ci/frontend-coverage-baseline.json`:

```bash
cd apps/rustnzb/frontend
npm test -- --coverage --coverage-reporters=text-summary --coverage-reporters=json-summary
cd -
node ci/check-frontend-coverage.mjs \
  apps/rustnzb/frontend/coverage/frontend/coverage-summary.json \
  ci/frontend-coverage-baseline.json
```

The baseline is a ratchet floor, not a target. Identical code reports a small
environment-dependent coverage spread (960/5191 statements on some machines,
965-966/5191 on others), so the floor sits below the low end of that band and
only a real regression trips it. Raise it deliberately when coverage improves.

## Containerized tasks

`./ci/run` runs checked-in task scripts in the pinned toolchain images. It is
useful where a local Docker environment is available:

```bash
./ci/run fmt
./ci/run check
./ci/run test
./ci/run clippy
./ci/run harness
./ci/run frontend-test
./ci/run e2e
./ci/run build-image rustnzb:local
./ci/run smoke-image rustnzb:local
```

The pinned toolchain images live in a private registry, so these tasks and a
plain `docker build .` need registry access. Without it, build the same
`Dockerfile` with the cross toolchain built locally from `Dockerfile.ci`:

```bash
docker buildx bake --file docker-bake.hcl --load image
```

The first run compiles the toolchain, so it takes several minutes.

Generated output belongs in `target/`, `.ci-output/`, `.ci-artifacts/`, or
frontend build directories and must not be committed.

## Tests

- Rust unit and integration tests live with their crates and under
  `apps/rustnzb/tests/`.
- Browser journeys and Playwright coverage live in `e2e/`.
- The deterministic NNTP fixture is in `crates/mock-nntp-server/`.
- `benchnzb/` is a benchmark harness, not a substitute for correctness tests.

### Deterministic compatibility harness

The compatibility test layers are intentionally local and deterministic:

```bash
cargo test -p nzb-web --tests --locked
cargo test -p rustnzb --tests --locked
cargo test -p nzb-postproc --tests --locked
```

The reusable fixtures live under `crates/nzb-web/tests/harness/` and the
checked-in response contract lives under
`crates/nzb-web/tests/fixtures/`. Update a golden only when the
wire contract changes, preserve dynamic `$type:*` markers, and include a test
that proves the changed mode's complete response shape. NNTP, URL, feed, and
watch-folder tests use loopback fixtures or temporary directories; correctness
tests must never contact a live provider. Every fixture owns its temporary
state and drops it at test completion. Tests that depend on an implementation
not yet present should be marked as an explicit implementation-gated plan
rather than weakened to pass.

The harness gate covers the compatibility and failure matrix as one explicit
review target. The matrix is intentionally split by ownership:

| Area | Current deterministic coverage | Implementation-gated extension |
| --- | --- | --- |
| API envelopes | Read modes, uploads, errors, filtering, paging, and golden fixtures | Add a fixture whenever a supported response field changes |
| NNTP lifecycle | Retry, pause/resume, cancellation, authentication failure, failover, and restart checkpoints | Add provider-specific protocol cases only when the production state machine gains them |
| URL and feed input | Scheme/address validation, redirect resistance, body limits, feed filters, and duplicate suppression | Add parser fixtures for newly accepted feed formats |
| Post-processing | Nested archives, path safety, cleanup, password diagnostics, repair, and resource limits | Tar and hardlink semantics remain explicit implementation gates until supported |
| Import and watch workflows | Multipart import, URL import, watch-folder ingestion, and compressed input limits | Add a workflow fixture before exposing a new ingestion source |

Focused gates are suitable for local iteration; the full workspace commands
above remain the review gate. Run a focused test three times when changing
timing-sensitive queue behavior to catch flakes before broadening the loop.
