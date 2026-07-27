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
