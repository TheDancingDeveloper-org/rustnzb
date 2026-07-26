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

Historical Node B incomplete-download directories are intentionally not
deleted by this change. They require a separately approved operator cleanup
after inventory and retention review.
