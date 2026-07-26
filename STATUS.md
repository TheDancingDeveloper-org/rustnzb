# rustnzb status

## Reliability remediation — 2026-07-26

Confirmed and addressed in the reliability change set:

- Direct unpack remains enabled by default. `unrar` output is now consumed
  from both stdout and stderr, recognises legacy and current volume prompts,
  and has a five-minute no-output watchdog. Regression tests cover a modern
  stderr/no-newline prompt and an stderr error path.
- Terminal failed jobs now remove their raw work directory only after a
  history row is durably present. Completed jobs remove their work directory
  only after it is empty; a failed output move retains files for safety.

Benchmark report remediation is in progress in GitHub issue #24. The harness
now uses compact deterministic fixtures, records terminal outcome plus
source-payload SHA-256 verification, captures decoded/wire/request/430
metrics, and samples both completed and incomplete work directories. A
controlled missing-article/PAR2-repair run passed: rustnzb observed one 430,
repaired the payload, verified its SHA-256, and removed its terminal work
directory. The vendor measurements remain hypotheses until reproducible
controlled runs are published.

## Nested archive extraction — 2026-07-26

The post-processing extract stage now processes archive waves recursively,
with a default maximum depth of five, configurable through
`general.max_nested_archive_depth`. Direct unpack also scans its output for
nested archives. A remaining archive at the depth limit causes a terminal
post-processing failure and is retained for safety rather than being reported
as a complete usable payload. Deterministic nested-ZIP tests cover successful
recursive extraction, direct-unpack output, safe depth-limit failure, cleanup,
and preserving unrelated completed-directory archives.

## Usable-output completion contract — 2026-07-26

Jobs that reach post-processing but contain only raw archive and PAR2
artifacts are now recorded as Failed with an explicit `Output` stage rather
than being moved into the completed library. Paired tests cover the negative
raw-artifact case and the positive usable-payload case.

## Public WebDAV dependency chain — 2026-07-26

The optional WebDAV feature now resolves `nzbdav-core`, `nzbdav-stream`,
`nzbdav-pipeline`, and `nzbdav-dav` from public crates.io release `0.5.7`.
Their public source is https://github.com/TheDancingDeveloper-org/nzbdav-rs;
the release declares Rust 1.88 compatibility. `cargo check` and the complete
rustnzb WebDAV feature test suite pass against the public dependency chain.

Historical Node B incomplete-download directories are intentionally not
deleted by this change. They require a separately approved operator cleanup
after inventory and retention review.
