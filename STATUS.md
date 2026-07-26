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

Open benchmark-led work is tracked in GitHub issues #21–#24. Their vendor
measurements are hypotheses until reproduced with controlled, legal fixtures.
Recursive nested archive extraction is tracked in #22; it is not claimed as
implemented by this status note.

## Nested archive extraction — 2026-07-26

The post-processing extract stage now processes archive waves recursively,
with a default maximum depth of five, configurable through
`general.max_nested_archive_depth`. Direct unpack also scans its output for
nested archives. A remaining archive at the depth limit causes a terminal
post-processing failure and is retained for safety rather than being reported
as a complete usable payload. Deterministic nested-ZIP tests cover successful
recursive extraction, direct-unpack output, safe depth-limit failure, cleanup,
and preserving unrelated completed-directory archives.

Historical Node B incomplete-download directories are intentionally not
deleted by this change. They require a separately approved operator cleanup
after inventory and retention review.
