# rustnzb v1.3.2

## Summary

rustnzb 1.3.2 adds persistent download statistics, richer per-download
history insights, and selectable application themes. It also keeps long
failure details contained within the History view.

## Features

- A new Statistics view reports download counts, transferred bytes, average
  and fastest speeds, news-server hits, and article availability for the last
  24 hours, 7 days, 30 days, and the lifetime of the installation.
- Per-server statistics show transferred bytes, served and missing articles,
  availability, and the most recent activity time.
- History rows now expand into detailed download summaries with average speed,
  article availability, per-server usage, processing stages, and recorded
  errors.
- History CSV exports now include average download speed.
- Settings now offers Rust dark, Midnight, and Daylight themes. The selected
  theme is stored in the browser and restored on the next visit.

## Fixes

- Long download names and failed-download error messages no longer widen or
  wrap the History table unexpectedly; full values remain available through
  tooltips and the expanded details panel.

## Validation

- Unit coverage verifies theme persistence and the calculations and loading
  behavior used by History details.
- End-to-end coverage exercises the Statistics view, theme selection, History
  details, average-speed display, and long failed-download messages.

## Breaking changes and upgrade notes

- There are no intentional breaking HTTP API or configuration changes.
- The SQLite schema advances to version 7 with a compact, permanent download
  statistics ledger. Migration is automatic, and retained history is imported
  so existing installations begin with the statistics already available in
  their database.
- Existing container mounts and configuration remain compatible. Operators
  should upgrade normally by pulling `v1.3.2` or `latest` after publication.

## Bundled shared-crate versions

- `nzb-core 0.2.16`
- `nzb-decode 0.1.2`
- `nzb-dispatch 0.2.6`
- `nzb-news 0.1.12`
- `nzb-nntp 0.2.22`
- `nzb-postproc 0.2.6`
- `nzb-web 0.4.20`

## Downloads

- Linux x86_64: `rustnzb-v1.3.2-linux-x86_64.tar.gz`
- Linux aarch64: `rustnzb-v1.3.2-linux-aarch64.tar.gz`
- Windows x86_64 installer: `rustnzb-v1.3.2-windows-x86_64-setup.exe`
- Debian/Ubuntu amd64: `rustnzb-v1.3.2-amd64.deb`
- Debian/Ubuntu arm64: `rustnzb-v1.3.2-arm64.deb`
- Checksums: `SHA256SUMS-v1.3.2.txt`
- Docker: `ghcr.io/ausagentsmith-org/rustnzb:v1.3.2`

All downloadable files are attached to both the Forgejo and GitHub releases
and are also published at `https://dl.rustnzb.dev/v1.3.2/`.
