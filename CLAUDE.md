# CLAUDE.md — rustnzb

## Project overview

rustnzb is a Rust 2024 Usenet downloader with an Axum API, Angular 21 web UI,
SABnzbd-compatible API, optional WebDAV media library, and Tauri desktop app.
This repository is the canonical monorepo for the application and shared
`nzb-*` crates. Forgejo is the private source of truth; GitHub is a public
distribution mirror only.

Read the workspace `AGENTS.md` before authenticated CI, registry, ops,
or deployment work.

## Repository layout

```text
Cargo.toml                         workspace manifest and shared dependencies
apps/rustnzb/                     runnable server application
  src/                            app handlers, router, startup, DAV adapter
  frontend/                       Angular 21 SPA
  tests/                          app integration tests
  config.example.toml             complete configuration reference
crates/
  mock-nntp-server/               deterministic NNTP test server
  nzb-core/                       config, models, database, NZB/import parsing
  nzb-decode/                     yEnc and assembly
  nzb-dispatch/                   server-aware article dispatch
  nzb-news/                       worker/download orchestration
  nzb-nntp/                       NNTP protocol, TLS, pooling, pipelining
  nzb-postproc/                   PAR2 and archive processing
  nzb-web/                        queue manager and shared HTTP/API services
desktop/                          Tauri app, excluded from root workspace
e2e/                              Playwright browser suite
benchnzb/                         benchmark suite, excluded from workspace
ci/                               pinned toolchains and checked-in task scripts
Dockerfile                        canonical production image
Dockerfile.ci                     core/cross/e2e/desktop CI toolchain images
.woodpecker.yml                   Forgejo-connected Woodpecker pipeline
```

The root workspace uses resolver v3 and defaults to `apps/rustnzb`. Shared
crates are normal path-based workspace members. Keep the root
`[patch.crates-io]` entries: external private `nzbdav-*` crates depend on
published `nzb-*` versions, and the patch keeps WebDAV builds on the workspace
copies.

Current shared-crate versions:

| Crate | Version |
|---|---:|
| `nzb-web` | 0.4.20 |
| `nzb-nntp` | 0.2.22 |
| `nzb-core` | 0.2.16 |
| `nzb-decode` | 0.1.2 |
| `nzb-news` | 0.1.12 |
| `nzb-dispatch` | 0.2.6 |
| `nzb-postproc` | 0.2.6 |

The optional Forgejo-only WebDAV crates are `nzbdav-core`, `nzbdav-stream`,
`nzbdav-dav`, and `nzbdav-pipeline`, currently at 0.4.1.

## Build and test

Run Rust commands from the monorepo root:

```bash
cargo build -p rustnzb
cargo build -p rustnzb --release --features webdav
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Before every Rust commit, `cargo fmt`, Clippy with warnings denied, and the
workspace tests must pass. Live-provider NNTP tests remain ignored unless the
required external credentials and network access are deliberately supplied.

For exact local/CI parity, prefer:

```bash
./ci/run fmt
./ci/run check
./ci/run test
./ci/run clippy
./ci/run frontend-test
./ci/run frontend-audit
./ci/run desktop-test
./ci/run e2e
./ci/run build-image rustnzb:local
./ci/run smoke-image rustnzb:local
```

The wrapper selects the immutable toolchain digest from `ci/images.lock` and
mounts the checkout at the Woodpecker path. See `ci/README.md` for cache,
toolchain update, candidate promotion, and rollback rules.

## Frontend embedding

The Angular source is under `apps/rustnzb/frontend`; its production output is
`apps/rustnzb/frontend/dist/frontend/browser`.

`apps/rustnzb/src/server.rs` uses `rust-embed` with:

- a folder rooted at `$CARGO_MANIFEST_DIR`, so workspace launch directories do
  not change asset resolution; and
- `debug-embed`, so debug/E2E binaries contain the assets instead of reading
  a mutable `dist` tree at runtime.

This matters because Woodpecker tasks share a checkout and parallel tasks
remove generated frontend output during cleanup. A compiled server must keep
serving `/` after that cleanup. `index.html` uses revalidation caching;
Angular-hashed assets use immutable caching; SPA routes fall back to
`index.html`.

Frontend commands from `apps/rustnzb/frontend`:

```bash
npm ci --no-audit --no-fund
npm run build -- --configuration=production
npm test -- --watch=false
```

The root `e2e/` suite uses Playwright and deterministic main, first-boot, and
mock-download backends. Pipeline 218 passed all 85 browser tests after the
debug embedding fix.

## Container build and publishing

The canonical multi-stage `Dockerfile`:

1. builds Angular from tracked package/source inputs;
2. compiles the musl Rust binary with Zig and `webdav,vendored-openssl`;
3. reads the Forgejo token only from the required BuildKit secret
   `forgejo_token`; and
4. copies the binary into the pinned LinuxServer Alpine runtime with 7-Zip,
   curl, CA certificates, and s6 service definitions.

Never convert the Forgejo credential back to a Docker `ARG`, layer, committed
Cargo credential file, or cacheable environment value.

`ci/tasks/build-image` creates a temporary `docker-container` Buildx builder
when publishing with registry cache export. It supports:

- `RUSTNZB_BUILD_REF`
- `RUSTNZB_PLATFORM`
- `RUSTNZB_PUSH=true`
- `RUSTNZB_CACHE_IMAGE=<registry-ref>`

On `main`, Woodpecker repo ID 38 runs quality gates, 85 E2E tests, Linux and
arm64 cross-builds, and Windows build. It publishes an amd64 Forgejo candidate
as `:<full-sha>`, smoke-tests the exact pulled image, then promotes the same
digest to Forgejo/GHCR `:<full-sha>` and `:dev`. Ordinary pushes never move
`latest`. Tagged releases build/publish the multi-arch release and `latest`
tracks.

The runtime smoke test verifies the binary, `/api/health`, embedded UI, build
reference, 7-Zip, and graceful shutdown before mutable tags move.

## Deployment

An application push does not deploy Node B automatically.

Node B runs rustnzb inside Komodo stack `personal-arr`, sourced from the
private `indexarr/ops` repository at `personal/arr/compose.yaml`. The service
is exposed as host `8081` to container `9090`, with persistent config/data and
download mounts managed in that ops stack.

Deployment flow:

1. wait for Woodpecker to publish, smoke-test, and promote the candidate;
2. pin the immutable Forgejo SHA tag in the ops compose file;
3. commit and push the ops change, using a temporary worktree if the normal ops
   checkout is dirty or behind;
4. run the `komodo-stack-deploy` helper for `personal-arr`; and
5. require Komodo `success=true` with a successful `Compose Up` stage.

Validate on Node B with container image/status, `/api/health`, `/`, health
state, restart count, and recent logs. Do not use ad hoc `docker compose up` as
the source-of-truth deployment path.

The deployment completed on 2026-07-10 used application image
`2385c85fcad7981c08b0ae8b12725c05c3b89558`, ops commit `404f604`, and
reported healthy with zero restarts.

## Key runtime behavior

- Configuration precedence: CLI > environment > TOML > defaults.
- Core env vars: `RUSTNZB_CONFIG`, `RUSTNZB_LISTEN_ADDR`, `RUSTNZB_PORT`,
  `RUSTNZB_DATA_DIR`, `RUSTNZB_LOG_LEVEL`, and `RUST_LOG`.
- WebDAV startup is enabled with `RUSTNZB_DAV_ENABLED=1` or `ENABLE_DAV=1`
  when the feature is compiled.
- OTLP logs and metrics can be controlled independently; avoid duplicate OTLP
  logs when the platform already ships container stdout/stderr.
- `/api/health` is the container health endpoint.
- `/swagger-ui` serves OpenAPI documentation.
- `/sabnzbd/api` provides SABnzbd compatibility.
- WebDAV is rooted at `/dav`; clients should use `/dav` without a trailing
  slash because of the Axum nesting behavior.

## Coding conventions

- Tokio for asynchronous I/O.
- `thiserror` for library errors and `anyhow` at application boundaries.
- `tracing` for runtime logging; do not use `println!` in server code.
- SQLite uses WAL mode and queue/history data persists across restarts.
- Config changes through the API must persist back to the configured TOML.
- Angular uses zoneless change detection, signals/computed state, and
  lazy-loaded standalone components.
- Preserve unrelated dirty-worktree changes and never commit credentials or AI
  co-author lines.

## Release and operational references

- `README.md`: user-facing setup, architecture, development, and CI overview
- `ci/README.md`: reproducible task and image-promotion contract
- `generate_release.md`: crate/application release procedure
- `DEPLOY.local.md`: private environment-specific Node B notes (gitignored)
- Workspace `AGENTS.md`: workspace service access and commit rules
