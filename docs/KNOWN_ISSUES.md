# Known issues

This page records confirmed user-visible limitations. Track implementation and
discussion in the linked GitHub issues.

## Download dispatch

### Cross-job fairness is not guaranteed

[`#33`](https://github.com/TheDancingDeveloper-org/rustnzb/issues/33) is
confirmed. The current dispatch queue scans FIFO work items and rotates items
that are not eligible for the current server. It does not round-robin eligible
work between jobs. A job with a large eligible backlog can therefore delay a
sibling job sharing the same server.

The intended fix is per-server round-robin selection across eligible jobs plus
a regression test demonstrating that each active job receives work.

### Provider outages have no distinct queue status

[`#34`](https://github.com/TheDancingDeveloper-org/rustnzb/issues/34) is
confirmed. During transient provider outages, rustnzb retains queued work for
retry after circuit-breaker cooldown. This avoids misclassifying a temporary
provider failure as missing content, but the queue currently does not surface a
specific waiting-for-providers status or explanation.

The intended fix is a non-terminal user-visible state/message that distinguishes
provider unavailability from ordinary queue capacity without treating the
article as missing.

## Benchmark results

Benchmark artifacts are generated locally and are not published as project
claims by default. See `benchnzb/METHODOLOGY.md` and `benchnzb/issues.md` for
methodology limits that must be addressed or disclosed before publishing any
comparison.
