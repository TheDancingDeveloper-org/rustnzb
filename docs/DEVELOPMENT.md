# Development

## Prerequisites

- Rust 1.88 or newer
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
./ci/run frontend-test
./ci/run e2e
./ci/run build-image rustnzb:local
./ci/run smoke-image rustnzb:local
```

Generated output belongs in `target/`, `.ci-output/`, `.ci-artifacts/`, or
frontend build directories and must not be committed.

## Tests

- Rust unit and integration tests live with their crates and under
  `apps/rustnzb/tests/`.
- Browser journeys and Playwright coverage live in `e2e/`.
- The deterministic NNTP fixture is in `crates/mock-nntp-server/`.
- `benchnzb/` is a benchmark harness, not a substitute for correctness tests.
