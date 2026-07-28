# Performance status

Last updated: 2026-07-28 UTC

## Current status

The queue-maintenance optimization in `0f10d60` is the retained performance
change. Terminal jobs are excluded from active queue views and the cleanup
tail, reducing repeated work while preserving download, retry, integrity, and
output behavior. The full 24-leg matrix passed after that change.

Parity with the controlled reference workload has not yet been reached. The
next optimization target is decode/dispatch/assembly overhead, guided by the
phase measurements below. No optimization is retained on a single favorable
run.

## Baseline measurements

These are three-run medians for the 5 GiB raw fixture with eight connections;
each output was independently SHA-256 validated.

| Pipeline depth | Median elapsed | Median throughput |
| ---: | ---: | ---: |
| 1 | 8.683 s | 589.66 MiB/s |
| 2 | 6.565 s | 779.89 MiB/s |
| 4 | 6.665 s | 768.19 MiB/s |
| 8 | 7.704 s | 664.59 MiB/s |

The depth-2 run is the current throughput baseline. Its internal download
phase measured roughly 5.1–5.4 seconds and its cumulative worker decode time
measured roughly 12.3–12.8 seconds, showing that decode work overlaps across
workers and that higher pipeline depths do not automatically improve
end-to-end time.

## Optimization gate

Every performance change must include all of the following before it can be
retained:

- a positive behavior test for the normal path;
- a negative or fault-path test proving malformed, missing, truncated, or
  otherwise invalid input is still rejected or retried correctly;
- `cargo fmt --all -- --check`;
- workspace Clippy with warnings denied, a workspace build, and the complete
  workspace test suite;
- a controlled 5 GiB benchmark with repeated rounds and independent
  byte-for-byte/hash validation.

Changes are retained only when the improvement is repeatable and the complete
functional gate remains green.

## Evaluated experiments

### Terminal-job queue filtering — retained

Commit `0f10d60` removed terminal jobs from active queue views and the cleanup
tail. The 24-leg throughput matrix passed, while the workspace suite covered
normal and fault paths. The change is the current deployed fix.

### Buffered receive — rejected

A boundary-aware buffered line/body reader was implemented and exercised with
positive pipelined-response-boundary coverage and a negative truncated-body
case. Focused tests, the workspace suite, and Clippy passed, but repeated 5
GiB runs showed no reproducible end-to-end gain. The implementation was
reverted to avoid changing a well-tested protocol boundary without measurable
benefit.

### Batched scheduler yields — rejected

Batching event-loop yields passed its paired positive and negative tests, but
the three-run result (6.652 s, 9.034 s, 7.885 s) had a 7.885 s median and was
a clear regression. The change was reverted.

### Thin release LTO — rejected

Cross-crate thin LTO with one codegen unit was tested as a semantics-neutral
release-profile change. All six controlled legs completed with valid hashes,
but the RustNZB runs were 7.639 s, 7.908 s, and 7.690 s (7.690 s median),
slower than the 6.565 s depth-2 baseline. The profile change was reverted.

## Functional boundary for future work

Performance work must preserve protocol status handling and fallback
behavior, retry handling for missing articles, yEnc length and CRC checks,
out-of-order positioned writes, final file synchronization, and exact output
bytes. Benchmark phase timings are diagnostic; they are not release claims
until the repeated validation gate passes.
