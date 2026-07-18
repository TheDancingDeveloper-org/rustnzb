# rustnzb v1.3.4

## Summary

rustnzb 1.3.4 is a patch release that improves PAR2 compatibility and repair
diagnostics while making active-download and NNTP connection health visible in
the queue interface.

## Fixes

- PAR2 recovery volumes using the common `volNN-NN.par2` spelling are now
  recognized alongside traditional `volNN+NN.par2` files during NZB parsing,
  recovery-capacity calculation, verification, and post-processing.
- Native PAR2 repair now reports insufficient recovery data before attempting
  a repair that cannot cover the verified damage.
- Failed extraction history includes a concise excerpt from the extractor's
  error output, making archive and password failures easier to diagnose.
- Active downloads now display their failed article counts directly in the
  queue.
- The server status endpoint now reports established NNTP sockets per provider,
  and the queue displays live connection usage instead of an estimate.
- Short-lived NNTP connections remain visible as a clearly marked recent value
  for five seconds, so fast backup-provider cascades are not missed between UI
  polling intervals.

## Validation

- Regression tests cover hyphenated PAR2 filenames, recovery-volume ordering,
  insufficient recovery capacity, established-socket accounting, recent
  connection display, and failed-article queue rendering.
- The locked Rust workspace, WebDAV release build, strict Clippy checks,
  frontend unit suite, and deterministic browser tests are validated by the
  release workflow.

## Breaking changes and upgrade notes

- There are no intentional configuration, SQLite schema, or compatibility
  changes.
- The `/api/status` response adds an `nntp_connections` field; existing clients
  can ignore it.
- Existing container mounts and configuration remain compatible. Operators
  should upgrade normally by pulling `v1.3.4` or `latest` after publication.

## Bundled shared-crate versions

- `nzb-core 0.2.16`
- `nzb-decode 0.1.2`
- `nzb-dispatch 0.2.6`
- `nzb-news 0.1.12`
- `nzb-nntp 0.2.22`
- `nzb-postproc 0.2.6`
- `nzb-web 0.4.20`

## Downloads

- Linux x86_64: `rustnzb-v1.3.4-linux-x86_64.tar.gz`
- Linux aarch64: `rustnzb-v1.3.4-linux-aarch64.tar.gz`
- Windows x86_64 installer: `rustnzb-v1.3.4-windows-x86_64-setup.exe`
- Debian/Ubuntu amd64: `rustnzb-v1.3.4-amd64.deb`
- Debian/Ubuntu arm64: `rustnzb-v1.3.4-arm64.deb`
- Checksums: `SHA256SUMS-v1.3.4.txt`
- Docker: `ghcr.io/ausagentsmith-org/rustnzb:v1.3.4`

All downloadable files are attached to both the Forgejo and GitHub releases
and are also published at `https://dl.rustnzb.dev/v1.3.4/`.
