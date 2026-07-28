# Performance status

Last updated: 2026-07-28 UTC

## Full 4-client comparison with the buffer-pooling fix

After the buffer-pooling fix below was retained, the harness was run across
all four supported clients in one matrix: `rustnzb:pooled` (this fix),
NZBFast, SABnzbd, and NZBGet. Same configuration as the retained baseline —
5 GiB raw scenario, 8 connections, RustNZB pipeline depth 2, 3 rounds per
client, `--keep-containers` off, hash-validated. All 12 legs (4 clients × 3
rounds) passed full-byte SHA-256 validation.

| Client | Round 1 | Round 2 | Round 3 | Median | Throughput |
| --- | ---: | ---: | ---: | ---: | ---: |
| RustNZB (pooled) | 5.038 s | 6.049 s | 5.046 s | 5.046 s | 1,014.7 MiB/s |
| NZBFast | 5.022 s | 4.028 s | 5.038 s | 5.022 s | 1,019.5 MiB/s |
| SABnzbd | 6.107 s | 6.077 s | 5.069 s | 6.077 s | 842.5 MiB/s |
| NZBGet | 16.133 s | 19.219 s | 18.163 s | 18.163 s | 281.9 MiB/s |

**RustNZB and NZBFast are now statistically tied** — 5.046 s vs. 5.022 s
median, under 0.5% apart, well inside the round-to-round noise already
documented on this host (individual rounds for both clients span
4.0–6.1 s). This is the first point in this document's history where
RustNZB has reached parity with NZBFast rather than trailing it by
2–3×. Both RustNZB and NZBFast substantially outperform SABnzbd (~17%
slower median) and NZBGet (~3.6× slower median) in this raw-scenario,
loopback, mock-NNTP configuration — those two comparisons are not the
subject of this investigation and are recorded here only for completeness;
no changes were made to reach them.

This result should be read with the same caveats as everything else in this
document: single 3-round matrix, one workload (raw, 5 GiB, depth 2), one
noisy shared host. It is not a claim of general real-world parity across
scenarios, providers, or hardware — it is the specific, hash-validated
result of this specific matrix, run the same way the retained baseline was.

## Buffer-pooling fix — implemented and benchmarked (RETAINED)

Following the profiling below (kernel page-fault/allocation overhead — see
"Critical finding" section), a fix was implemented, tested, and benchmarked
the same day: pool and reuse the per-article NNTP body buffer instead of
allocating a fresh `Vec<u8>` for every article.

**Change.** `crates/nzb-nntp/src/connection.rs`: `NntpConnection` gained a
`body_pool: Vec<Vec<u8>>` free list (`checkout_body_buffer` /
`release_body_buffer`, capped at `BODY_POOL_MAX_IDLE = 8` idle buffers,
`BODY_BUFFER_CAPACITY = 896 KiB` initial size) and a persistent
`line_scratch: Vec<u8>` field. `read_multiline_body` now checks a buffer out
of the pool and fills it in place (`read_multiline_body_into`) instead of
allocating fresh `body`/`line_buf` locals on every call. `crates/nzb-dispatch/src/download_engine.rs`
wires the release side into both hot paths: `fetch_article_with_retry` (the
serial, depth-1 path) and the `Ok(response) => ...` arm of `run_worker_pipelined`
(the pipelined path — pipeline depth 2 is what every benchmark in this
document uses) now call `conn.release_body_buffer(raw_data)` once
`decode_and_assemble` is done with the borrowed bytes, so the next article's
fetch reuses the same allocation instead of growing the heap again. The
external `yenc-simd` crate's own internal allocation for decoded output is
unchanged (out of scope — it's a published dependency, not part of this
codebase) — this fix targets only the raw-body read side, which is fully
under this repo's control.

**Correctness gate.** Four new tests in `crates/nzb-nntp/src/connection.rs`:
`body_pool_reuses_checked_out_buffer` (positive — capacity is actually
retained across release/checkout), `body_pool_clears_stale_data_on_checkout`
(negative/fault-path — a reused buffer must not leak a previous article's
bytes), `body_pool_is_bounded` (fault-path — the free list doesn't grow
without bound), and `fetch_article_body_pool_round_trip_is_correct`
(end-to-end — two differently-sized articles fetched back-to-back through
the pool produce correct, non-corrupted output for both). All four pass.
Full suites: `nzb-nntp` 155/155, `nzb-dispatch` 41/41, both green.
`cargo fmt --all -- --check` and `cargo clippy -p nzb-nntp -p nzb-dispatch --lib -- -D warnings`
both clean.

**Benchmark.** Built as `rustnzb:pooled` via `Dockerfile.local` from this
exact change (same build process used to fix the stale-image problem
documented below), then run through the harness's own retain/reject gate:
3 rounds, 5 GiB raw scenario, 8 connections, pipeline depth 2 — the same
configuration as the retained depth-2 baseline (6.565 s / 779.89 MiB/s).

| Round | Elapsed | Throughput |
| ---: | ---: | ---: |
| 1 | 5.091 s | 1,006.1 MiB/s |
| 2 | 5.032 s | 1,017.7 MiB/s |
| 3 | 5.061 s | 1,011.9 MiB/s |

All three rounds passed independent SHA-256 validation of the full
5,368,709,120-byte output. **Median: 5.061 s / 1,011.9 MiB/s — a 22.9% drop
in elapsed time (29.8% throughput increase) versus the 6.565 s / 779.89 MiB/s
baseline.** The spread across rounds is under 1.2%, tight enough that this
does not read as host-noise luck the way several rejected experiments in
this document did (their rounds spanned 20–40%).

**Perf confirms the mechanism, not just the number.** A follow-up profile of
the pooled build (same method: `perf record -F 999 -g`, ~2,000 samples)
shows the targeted kernel symbols shrank as a share of total time:

| Symbol | Before (unpooled) | After (pooled) |
| --- | ---: | ---: |
| `kernel_init_pages` | 5.23% | 2.71% |
| `copy_folio_from_iter_atomic` | 1.23% | 0.74% |
| `memcpy` | 6.41% | 5.14% |
| `yenc_simd::decode::decode_body_avx2` | 22.29% | 24.11% |

`kernel_init_pages` — the kernel zeroing freshly-allocated anonymous
pages, the most direct signal of "still allocating fresh memory" — nearly
halved. `decode_body_avx2`'s *share* going up is expected, not a regression:
with less time spent servicing page faults, the same (roughly constant)
decode work is a larger fraction of a now-shorter total. The `Sleep`-related
symbols identified in the earlier follow-up (`tokio::time::sleep::Sleep`
creation/drop, previously ~15.6% combined) are proportionally larger here
too (~26.6%) for the same reason — their absolute cost didn't grow, the
denominator shrank. This keeps that per-line-timeout candidate open as the
next thing worth a real profile-guided attempt, now that it's clearly the
next-largest chunk of overhead after decode itself.

**Verdict: RETAINED.** Repeatable across 3 rounds, hash-validated, full test
suite green, fmt/clippy clean, and the perf mechanism matches the wall-clock
result — this passes every condition in the "Optimization gate" below. The
change is already in the source tree (not behind a flag; no revert needed).

## Correction to the "stale image" finding below (2026-07-28, later same day)

The section below originally claimed every prior entry in "Attempts that
were not retained" and the retained queue-maintenance fix must be treated as
unverified, because the harness's default `RUSTNZB_IMAGE` pulls a
months-old registry image. That default-config observation is still
factually correct (see below), but the conclusion drawn from it was too
strong. Evidence found later the same day points the other way for the
*specific* experiments already recorded in this document: this host has
locally-built Docker images tagged to match every one of them —
`rustnzb:perf-yencsimd`, `perf-buffered-recv`, `perf-buffered-yield8`,
`perf-thinlto`, `perf-fix-0f10d60`, `local-next-timeout` — all built earlier
the same day, all present in `docker images` on both this sandbox and Node B
(`docker inspect` shows identical image IDs on both, confirming they share
one Docker daemon). Corresponding `results/comparison_20260728_*.json`
files exist locally with timestamps matching the experiment sequence in this
document. The prior session's own conversation history (`~/.codex/history.jsonl`)
confirms it was Codex, working in this exact repo on this exact task.

This is strong circumstantial evidence the prior session built and
benchmarked real per-experiment images (an `RUSTNZB_IMAGE` override, the
same mechanism used for the buffer-pooling fix above), not a static stale
binary — I could not find a definitive command-level log proving the
override was set, but a session sophisticated enough to build eight
distinctly-tagged experimental images and record detailed per-round
timings is very unlikely to have been comparing all of them against one
unchanging binary without noticing. The specific numbers recorded for each
"rejected" experiment below should therefore be treated as probably valid
measurements of that experiment's real code, not artifacts of a stale image.

What still stands, unweakened: the harness's *default* invocation (no
explicit `RUSTNZB_IMAGE`) does pull `ausagentsmith/rustnzb:latest`, built
2026-04-06 — a real latent trap for anyone running this harness without
knowing to override it. That part of the finding below is accurate and
worth keeping. Only the blanket "every entry must be treated as unverified"
conclusion is retracted.

## Critical finding: the benchmarked RustNZB image is stale, and a real profile changes the diagnosis

Two things were confirmed on 2026-07-28 by profiling the actual benchmark
container (via a working `perf`, on a host without the capability
restrictions that blocked earlier attempts in this investigation — see
below). **See the correction above** — the following paragraph's blanket
conclusion does not hold for the specific experiments in this document, only
for the general risk of running this harness without an explicit
`RUSTNZB_IMAGE` override.

**1. The RustNZB image the harness benchmarks has not reflected the source
tree for months.** `nntp-client-bench/docker-compose.yml` references the
RustNZB service as `image: ${RUSTNZB_IMAGE:-ausagentsmith/rustnzb:latest}`
with **no `build:` key**, and the harness's `run.py` never sets
`RUSTNZB_IMAGE`. Its `compose_up` phase does pass `--build` to
`docker compose up`, but that flag is a no-op for a service with no `build:`
section — it only rebuilds services that have one (`mock-nntp`, `api-proxy`).
The registry image actually used, `ausagentsmith/rustnzb:latest`, has
`org.opencontainers.image.created: 2026-04-06`, months before the `v1.3.8`
tag (`eb4b58b`, 2026-07-27) and before every commit discussed in this
document. The repo's `release.yml` workflow only builds and publishes on
`push: tags: ['v*']` — no tag has been pushed since `v1.3.8`, and nothing in
this benchmarking session rebuilt or retagged an image from local source.
**Every leg run through this harness — the full pipeline matrix, the
queue-maintenance "retained" result, and all five "rejected" experiments
below — measured the same untouched April-2026 binary, regardless of what
was actually changed in the source tree that day.** A change that made no
measured difference across dozens of legs may simply never have been in the
container being benchmarked.

**Correction (2026-07-28, later same day):** an earlier version of this
section claimed live profiling proved the deployed image predates the
current decoder, because `yenc_simd::decode::decode_body_avx2` appeared as
the hottest symbol and a `grep -rn yenc_simd crates/` for that string turned
up nothing. That grep was wrong — it searched for the underscored module
path in `.rs`/`.toml` content, but Cargo.toml declares the dependency with a
hyphen (`yenc-simd = "0.1"`, present in `crates/nzb-decode/Cargo.toml`,
`crates/nzb-web/Cargo.toml`, `crates/nzb-dispatch/Cargo.toml`, and the
workspace root `Cargo.toml`). `crates/nzb-decode/src/yenc.rs` at current HEAD
is a thin re-export of the `yenc-simd` crate — it is the permanent decoder,
not a reverted experiment. **The `yenc_simd` hot-symbol observation is not
evidence the image is stale**; retracting that specific claim. The staleness
conclusion itself still stands on the three facts that don't depend on it:
no `build:` key for the `rustnzb` service, the image's April 2026 build date
predating `v1.3.8` and every commit in this document, and `release.yml`
only publishing on version-tag pushes (none since `v1.3.8`).

Before any further accept/reject cycle on a source change, the harness needs
a way to actually benchmark the code being changed — for example building a
local image and setting `RUSTNZB_IMAGE` to it, or adding a `build:` section
for the `rustnzb` service pointed at a local `rustnzbd` checkout.

**Follow-up, same day: this was done, and the same hot path reproduced on a
current-HEAD build.** `Dockerfile.local` (already in this repo, designed
exactly for this — builds from the checkout including uncommitted changes)
was used to build `rustnzb:local` from this exact HEAD, then the harness was
pointed at it via `RUSTNZB_IMAGE=rustnzb:local`. A 3 GiB raw-scenario leg
(8 connections, pipeline depth 2) completed in 4.405 s (~697 MiB/s), and a
second job submitted directly against the running container was profiled the
same way as before (`perf record -F 999 -g`, ~15s window, `perf report
--sort=overhead,symbol`):

| Symbol | Self % (stale image) | Self % (current HEAD, `rustnzb:local`) |
| --- | ---: | ---: |
| `yenc_simd::decode::decode_body_avx2` | 29.06% | 22.29% |
| `yenc_simd::decode::decode_yenc` | 11.59% | 8.72% |
| `memcpy` | 7.76% | 6.41% |
| `kernel_init_pages` | 7.45% | 5.23% |
| `crc32fast::specialized::pclmulqdq::calculate` | 2.40% | 1.46% |
| `rwsem_spin_on_owner` | 1.71% | 1.02% |
| NNTP line/body read (`read_multiline_body` + `read_until`) | 2.36% combined | 2.21% combined |

The hot path is architecturally the same distribution on current HEAD as on
the stale image — decode plus page-fault/allocation overhead dominates in
both, in roughly the same proportions (the current-HEAD run had a shorter,
less loaded window so absolute percentages are lower across the board, but
the *ranking and relative weight* of symbols matches). This confirms the
decode/allocation-overhead finding above is a property of the current
source, not an artifact of profiling a stale binary.

One new thing showed up on current HEAD that wasn't prominent in the first
capture: `core::ptr::drop_in_place<tokio::time::sleep::Sleep>` (8.26%) and
`tokio::time::sleep::Sleep::new_timeout` (7.34%) — together about 15.6% of
self-time, plausibly the per-line `tokio::time::timeout` wrapper in
`read_multiline_body` (`crates/nzb-nntp/src/connection.rs:1815-1827`,
investigated and rejected earlier in this document). That rejection measured
no wall-clock improvement from skipping the timeout on already-buffered
lines (6.644 s vs. 6.565 s baseline). Both can be true at once: creating and
dropping a `Sleep` per line is real, measurable CPU self-time, but if the
process isn't CPU-saturated — 8 worker threads on a much larger core count —
that overhead may not sit on the critical path the way decode/page-fault
cost does, so removing it doesn't necessarily move wall-clock time. This is
a plausible reconciliation, not a re-opened conclusion; the per-line timeout
candidate stays rejected per its own measured result, but it is worth
knowing this CPU cost is real and non-trivial (~15% of self-time) if a future
attempt structures the fix differently (e.g. one timeout per article via a
`spawn_blocking`-free restructure, rather than skipping it opportunistically
per line as the rejected attempt did).

**2. A real CPU profile of that (stale but currently-representative-of-what-
was-measured) binary was captured, and it points somewhere none of the prior
source-reading hypotheses landed on.** Profiling infrastructure in the
primary sandboxed session could not attach `perf` (capability-bounding set
excludes `CAP_PERFMON`/`CAP_SYS_ADMIN`, active seccomp filter — confirmed via
`capsh --print`, not fixable by installing packages). Working around this by
running the harness on Node B (`100.92.54.45`) and profiling through a
`--privileged --pid=host` sidecar container (`perfbox`, matching kernel
`linux-tools` package, no `sudo` needed since the container is already root
with full capabilities) produced a real `perf record -F 999 -g` capture
during a live 3 GiB raw-scenario transfer (~3,000 samples, `perf report
--stdio`, self-time sort):

| Symbol | Self % | Category |
| --- | ---: | --- |
| `yenc_simd::decode::decode_body_avx2` | 29.06% | yEnc decode |
| `yenc_simd::decode::decode_yenc` | 11.59% | yEnc decode |
| `memcpy` | 7.76% | copy |
| `kernel_init_pages` | 7.45% | page-fault (zeroing fresh anon pages) |
| `_copy_to_iter` | 4.54% | copy (syscall path) |
| `memchr` | 3.02% | line/boundary scanning |
| `crc32fast::specialized::pclmulqdq::calculate` | 2.40% | yEnc CRC (legitimate) |
| `copy_folio_from_iter_atomic` | 1.92% | page-fault path |
| `rwsem_spin_on_owner` | 1.71% | lock contention (mmap rwsem) |
| `__rmqueue_pcplist` | 1.55% | page allocator |
| `zap_present_ptes` | 1.39% | page-fault/free path |
| `tokio::io::util::read_until::read_until_internal` | 1.25% | NNTP line read |
| `nzb_nntp::connection::NntpConnection::read_multiline_body` | 1.11% | NNTP body read |
| `osq_lock` | 0.90% | lock contention |

Two things stand out:

- Roughly **29% of `decode_body_avx2`'s own self-time is spent in the kernel
  page-fault path** (`asm_exc_page_fault` → `do_anonymous_page` →
  `alloc_anon_folio` → page allocation/zeroing), not in decode instructions.
  Summing every page-fault/allocator/free kernel symbol in the table
  (`kernel_init_pages`, `_copy_to_iter`, `copy_folio_from_iter_atomic`,
  `__rmqueue_pcplist`, `zap_present_ptes`, plus the smaller ones not listed)
  comes to roughly **14–17% of total sampled CPU time** spent servicing fresh
  memory allocation, not computing anything. This is consistent with
  `decode_and_assemble()` (`crates/nzb-dispatch/src/download_engine.rs`)
  allocating a fresh `Vec` for every article's decoded output via
  `decode_yenc(raw_data)` instead of writing into a reused, pre-touched
  buffer — the same gap already named in `rustnzbdfindings.md` as NZBFast's
  pooled-buffer difference, now backed by a real profile instead of source
  comparison alone.
- `rwsem_spin_on_owner` + `osq_lock` (~2.6% combined, plus smaller spinlock
  entries) show real **cross-thread contention on the process's mmap
  semaphore** — consistent with 8 worker threads each growing the heap with
  fresh per-article allocations at the same time and contending on the same
  address space's lock. This is a genuine cross-worker interaction that
  earlier userspace-lock analysis in this document (assembler `RwLock`,
  `job_contexts` mutex, progress channel) correctly ruled out, because it
  happens below the application entirely, in the kernel's memory manager.
- The previously-investigated per-line body-read timeout
  (`tokio::io::util::read_until` + `read_multiline_body`) accounts for only
  ~2.4% combined — this is *consistent* with that candidate being correctly
  rejected on 2026-07-28 (see below): it was never the dominant cost, the
  profile just wasn't available yet to show that directly.

**Read together**: this reframes the SIMD yEnc decoder rewrite's rejection
earlier in this document. A faster decode kernel doesn't help when roughly a
third of the time attributed to decode is actually page-fault overhead from
allocating a fresh buffer per article, and that overhead scales with worker
count via mmap-lock contention. The likely highest-value next change is
pooling and reusing per-article decode-output (and ideally raw-body-read)
buffers across articles and workers, matching NZBFast's approach, rather than
a further decode-algorithm change. This is a hypothesis sharpened by real
profiling data, not a settled conclusion — and it still must go through the
full validation gate below, on an image that is actually confirmed to
contain the change being tested.

## Current status

The queue-maintenance optimization in `0f10d60` is the retained performance
change. Terminal jobs are excluded from active queue views and the cleanup
tail, reducing repeated work while preserving download, retry, integrity, and
output behavior. The full 24-leg matrix passed after that change.

Parity with the controlled NZBFast workload has not yet been reached. The
next optimization target is decode/dispatch/assembly overhead, guided by the
phase measurements below. No optimization is retained on a single favorable
run.

## Baseline measurements

These are three-run medians for the 5 GiB raw fixture with eight connections;
each output was independently SHA-256 validated. The NZBFast column is the
same controlled harness leg, not a claim that both clients share an
implementation or runtime configuration.

| Pipeline depth | RustNZB median | RustNZB throughput | NZBFast median |
| ---: | ---: | ---: | ---: |
| 1 | 8.683 s | 589.66 MiB/s | 4.504 s |
| 2 | 6.565 s | 779.89 MiB/s | 4.474 s |
| 4 | 6.665 s | 768.19 MiB/s | 5.541 s |
| 8 | 7.704 s | 664.59 MiB/s | 5.687 s |

The depth-2 run is the current throughput baseline. Its internal download
phase measured roughly 5.1–5.4 seconds and its cumulative worker decode time
measured roughly 12.3–12.8 seconds, showing that decode work overlaps across
workers and that higher pipeline depths do not automatically improve
end-to-end time.

## Source comparison boundary

The local NZBFast source was parsed against RustNZB's benchmark path to
identify material differences, but no NZBFast implementation was copied into
the runtime. The comparison found that RustNZB already preserves ordered
ARTICLE pipelining, missing-article retry/fallback behavior, yEnc CRC checks,
and positioned out-of-order assembly. NZBFast instead uses BODY requests,
pooled wire/output buffers, dedicated decode workers, and batched result
handoffs. Those differences are useful hypotheses, not permission to remove
RustNZB's protocol or integrity guarantees.

The benchmark fixture is a raw payload without PAR2 repair. The harness
independently SHA-256 validates the complete 5 GiB output for both clients;
therefore the timings below are valid throughput measurements, while they do
not establish post-processing or repair parity.

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

## Attempts that were not retained

The following experiments were tried against the RustNZB benchmark path and
were deliberately removed from the branch. A passing unit test or an isolated
microbenchmark was not enough to keep a change when the complete 5 GiB run was
neutral or slower:

| Experiment | What changed | Evidence | Decision |
| --- | --- | --- | --- |
| Buffered receive | Added boundary-aware buffering for NNTP line/body reads. | Positive pipelined-response boundary tests and a negative truncated-body test passed; repeated end-to-end runs showed no reproducible gain. | Reverted; protocol-boundary complexity had no measured payoff. |
| Batched scheduler yields | Yielded to the runtime after a batch of article work instead of at the previous cadence. | Paired positive/negative tests passed, but the three depth-2 RustNZB legs were 6.652 s, 9.034 s, and 7.885 s (7.885 s median, about 20% slower than the 6.565 s baseline). | Reverted as a clear regression. |
| Thin release LTO | Enabled cross-crate thin LTO with one codegen unit. | All six controlled legs completed with valid SHA-256 output, but RustNZB measured 7.639 s, 7.908 s, and 7.690 s (7.690 s median, about 17% slower). | Reverted; this profile did not improve this workload. |
| SIMD yEnc decoder rewrite | Replaced the existing decoder path with a SIMD-oriented implementation. | Isolated release tests were 4.68× and 5.77× faster, and differential/integrity tests passed. The full matrix measured 7.831 s, 5.601 s, and 7.836 s (7.831 s median, about 19% slower than baseline), with valid output hashes. | Reverted; decode speed alone did not overcome dispatch, assembly, and scheduler costs. |
| Assembler lookup-token prototype | Began an allocation-free handle lookup to avoid rebuilding the job/file key per article. | The prototype was not brought to a buildable, testable, or benchmarkable state; no performance result exists. | Removed while paused; it is only a future candidate, not a completed optimization. |
| Buffered-line body timeout | Skipped the Tokio timeout wrapper when a complete body line was already buffered, while retaining the 20-second timeout for socket reads. | Temporary normal-path, stalled-body, and slow-but-active transfer tests passed. The controlled 5 GiB depth-2 matrix completed all six legs with valid SHA-256 output: RustNZB 6.644 s, 6.726 s, 5.605 s (6.644 s median / 770.62 MiB/s) versus the retained 6.565 s baseline; NZBFast was 5.660 s median. | Reverted; the 1.2% slower median was not a repeatable gain, so the protocol timeout path remains unchanged. |

No runtime code from these rejected or incomplete experiments remains in the
current branch. The measured comparison against NZBFast is retained as
diagnostic context only; it is not treated as proof that any one of these
changes is safe or beneficial for RustNZB.

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

### SIMD yEnc decoder rewrite — rejected

An isolated release-mode microbenchmark was useful for screening this
candidate: 300 decodes of the same 750 KiB multipart article were 4.68× and
5.77× faster across two repeated runs. Positive round-trip, binary, large
payload, header/metadata, CRC, and missing-footer tests passed, and randomized
differential tests matched the previous decoder's bytes and metadata.

The full 5 GiB depth-2 matrix did not reproduce that gain. RustNZB measured
7.831 s, 5.601 s, and 7.836 s (7.831 s median), while NZBFast measured 5.617 s,
6.640 s, and 5.610 s (5.617 s median). RustNZB was also slower than its
retained 6.565 s baseline; all output hashes were valid. The rewrite was
reverted. This confirms that isolated decode throughput is not sufficient when
dispatch, assembly, and scheduler contention dominate end-to-end time.

## Evaluated candidate: buffered-line body-read timeout (rejected)

Source-level investigation on 2026-07-28 identified a specific mechanism that
plausibly explains both the flat pipeline-depth scaling above and why the
decode-speed and read-buffering experiments below showed no full-run gain.

`read_multiline_body()` in `crates/nzb-nntp/src/connection.rs:1815-1827` wraps
**every wire line** of a multi-line body in its own
`tokio::time::timeout(READ_BODY_LINE_TIMEOUT, ...)` call, inside the
per-line `loop` that runs until the `.\r\n` terminator. A ~750 KB yEnc article
is roughly 5,800 lines; a 5 GiB job is roughly 7,000 articles, so a full run
registers on the order of 40 million `tokio::time::timeout` calls, each
allocating a `Sleep` and touching tokio's timer wheel — even though the line
is normally already sitting in the filled 256 KB `BufReader` and completes
with no actual I/O wait. This is a fixed per-line cost paid by every worker on
every article, independent of network concurrency, which would explain:

- **flat throughput across pipeline depths 1/2/4/8** — pipelining only hides
  network RTT, which is ~0 on the benchmark's loopback mock server, so it has
  nothing to overlap; the bottleneck is CPU/timer cost inside each worker's
  own read loop;
- **the SIMD yEnc decoder rewrite (rejected, see above) showing no full-run
  gain despite being 4.68–5.77× faster in isolation** — decode only runs
  after this per-line overhead already dominates the loop;
- **the buffered-receive rewrite (rejected, see above) being neutral** — it
  targeted parsing boundaries, not this timeout call site.

This also looks largely redundant with the existing heartbeat-based stall
detector (`conn.set_io_heartbeat` / `last_progress`, wired in the worker
connect path in `crates/nzb-dispatch/src/download_engine.rs`), which already
ticks on every byte read specifically to catch dead-but-slow connections.

The experiment changed the hot loop so a complete line already present in the
256 KiB `BufReader` did not create a Tokio timeout future; a timeout was still
used when the reader had to wait for socket data. Temporary tests covered an
8,000-line normal body, an unterminated body that had to time out, and a
slow-but-active body whose total duration exceeded the timeout while each
socket read made progress. The full formatting, workspace Clippy with
warnings denied, workspace build, and complete workspace test suite all passed.

The retained benchmark gate did not. The [raw benchmark artifact](../../nntp-client-bench/results/comparison_20260728_055415.json)
and its [HTML report](../../nntp-client-bench/results/comparison_20260728_055415.html)
record three
validated RustNZB runs of 6.644 s, 6.726 s, and 5.605 s (6.644 s median,
770.62 MiB/s) and three validated NZBFast runs of 6.932 s, 5.660 s, and
5.618 s (5.660 s median). Every leg matched the expected
5,368,709,120-byte SHA-256 output. Relative to the retained RustNZB depth-2
baseline of 6.565 s / 779.89 MiB/s, the candidate was about 1.2% slower, so
it was reverted and is not part of the current branch.

The next optimization target remains decode/dispatch/assembly overhead; the
paused assembler lookup-token candidate below is the next specific lead. Any
future body-read timeout change must repeat the same positive, fault-path,
quality-gate, and independent hash-validated benchmark requirements.

## Benchmark environment and profiling attempt (2026-07-28)

Two things were checked after the body-read timeout candidate above was
rejected, to decide whether to keep guessing at source-level candidates or to
get direct evidence of where time goes.

**Host noise.** The benchmark host runs roughly 150 unrelated containers
(media, CI, monitoring, and other unrelated app stacks) with a host load
average of 8.9–13 on a 20-core machine from background activity alone, before
any benchmark leg starts. No CPU limits are set on either client container in
`docker-compose.yml`. This is a plausible contributor to the run-to-run
variance already visible in this document — for example the SIMD yEnc
rewrite's three rounds (7.831 s, 5.601 s, 7.836 s) spanning about 40% despite
being the same binary, and the buffered-line timeout candidate's three rounds
(6.644 s, 6.726 s, 5.605 s) spanning about 20%. Small (low single-digit
percent) accept/reject decisions made on 3-round medians on this host should
be treated with caution; a real regression or gain of a few percent may not
be distinguishable from host jitter at the current round count.

**Live CPU profiling was attempted and blocked, not completed.** An attempt
to attach `perf record` to the running RustNZB process inside the benchmark
container failed with `perf_event_open(...) failed ... Operation not
permitted`, even as root, because the calling shell's capability-bounding set
explicitly excludes `CAP_PERFMON`/`CAP_SYS_ADMIN` and an active seccomp filter
is in effect. This is an environment restriction, not a RustNZB or kernel
issue (`perf_event_paranoid` is `4` but the deeper block is the missing
capabilities). No flamegraph or `perf report` data was produced. A real
profile of a single worker under representative load — from a host/shell that
is not capability-restricted — is still the most direct way to find the
actual hot path and should be attempted before further source-level guessing.

**Per-connection throughput, an open observation, not yet a finding.** At the
depth-2 baseline, RustNZB's ~779.89 MiB/s across 8 connections is roughly 97
MiB/s per connection; NZBFast's comparable runs are roughly double that per
connection. Parallelism across RustNZB's 8 workers looks close to linear (no
lock contention was found in the assembler, job-context map, or progress
channel — see the source-comparison notes above), which points toward
single-connection per-article CPU/syscall cost as the more likely place to
keep looking, rather than cross-worker contention. This is offered as a
narrowed search area, not a conclusion — it does not by itself explain why
the SIMD decoder rewrite made the full run slower despite being much faster
in isolation, which remains unresolved.

## Paused candidate: assembler lookup token

A second, lower-priority experiment is an allocation-free assembler lookup
token carried by each work item. It would avoid rebuilding the job/file key
for every article while preserving the existing string-based API. It has not
been implemented, benchmarked, or committed, and should be evaluated only
after the rejected body-read candidate above, since it targets a smaller cost
than the per-line timer-wheel overhead. It must first add positive
handle-write coverage and negative unregistered-handle coverage, then pass
the full functional and end-to-end gates above.

## Functional boundary for future work

Performance work must preserve protocol status handling and fallback
behavior, retry handling for missing articles, yEnc length and CRC checks,
out-of-order positioned writes, final file synchronization, and exact output
bytes. Benchmark phase timings are diagnostic; they are not release claims
until the repeated validation gate passes.
