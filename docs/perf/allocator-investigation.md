# Per-article allocator investigation (GH #327)

Investigation note for [GH #327](https://github.com/TheDancingDeveloper-org/rustnzb/issues/327).
Scope is the per-article download/decode path only. This document makes no
source, Cargo, CI or version change, and quotes no benchmark number that is not
already recorded in this repository.

## Verdict up front

- The repository does **not** set a global allocator anywhere. Confirmed:
  `rg global_allocator` over the whole tree returns nothing, and
  `rg 'mimalloc|jemalloc|snmalloc|tikv-jemallocator'` matches only
  unrelated text. `apps/rustnzb` has no `#[global_allocator]`.
- The raw NNTP body buffer is **already pooled** and reused per connection
  (`crates/nzb-nntp/src/connection.rs:357-372`). That was the earlier fix and it
  removed the read-side allocation churn.
- The remaining **large, per-article** allocation is the decoded-output `Vec`
  created inside the published `yenc-simd` crate
  (`yenc-simd 0.1.1`, `decode.rs:106`). It is not under this repository's
  control and it is a fresh mmap-sized allocation on every article.
- Everything else that allocates per article in this repo is **small** in bytes
  (tens to hundreds of bytes each) but there are roughly ten of them per
  article, so they matter for allocation *count* and allocator/lock contention,
  not for bytes.

So the honest framing is: this is two separable problems — one large external
allocation (bytes / page faults) and a cluster of small internal allocations
(count / contention). A global allocator is a candidate lever for both; a
per-article arena is only worth it if the large allocation can be moved under
this repo's control, which today it cannot without an upstream change.

## Is a global allocator already configured?

No. Evidence:

- No `#[global_allocator]` in any crate (`rg global_allocator` → no matches).
- `yenc-simd` is pinned at `0.1.1` in `[workspace.dependencies]` (`Cargo.toml:106`,
  `Cargo.lock:4344-4347`) and has no decode-into-buffer API (see below).
- The workspace has no `[profile.release]` of its own in the root `Cargo.toml`
  (only `[profile.dev.package.*]` overrides, `Cargo.toml:119-123`), so release
  builds use Cargo defaults unless the container passes flags; `Dockerfile:80`
  builds with `cargo zigbuild --release --locked -p rustnzb` (cross/static musl),
  `Dockerfile.local:38` with `cargo build --release -p rustnzb --features webdav`.
  `lto`/`codegen-units` tuning is present only in `benchnzb/Cargo.toml:26-28`,
  which is excluded from the workspace.
- `benchnzb` and `desktop` are separate workspaces (`Cargo.toml:16`), so any
  global allocator added to `apps/rustnzb` covers the shipped binary but not the
  harness or the Tauri shell.

## The per-article path, end to end

Pipelined worker (depth 2) and serial (`depth 1`) paths share the same steps.
Line references are for the current tree.

1. **Send command.** `Pipeline::flush_sends` builds `mid` (a clone or
   `format!`) and then `format!("ARTICLE {mid}")` —
   `crates/nzb-nntp/src/pipeline.rs:100-107`. `send_command_no_flush` copies the
   command again into a fresh `String` and appends CRLF —
   `crates/nzb-nntp/src/connection.rs:2307-2317`, allocation at `:2315`.
   The serial path is the same via `fetch_article`:
   `normalize_message_id` (`:2587-2593`) + `format!` (`:1028-1029`).
2. **Read body.** `Pipeline::receive_one` → `read_multiline_body`
   (`crates/nzb-nntp/src/pipeline.rs:137-150`, `crates/nzb-nntp/src/connection.rs:2385-2390`),
   which checks a buffer out of the free list
   (`connection.rs:357-363`, `BODY_BUFFER_CAPACITY = 896 KiB` at `:324`,
   `BODY_POOL_MAX_IDLE = 8` at `:329`) and fills it in place, reading one line at
   a time into the persistent `line_scratch` field
   (`connection.rs:2410-2413`, `2403-2530`). In steady state this step
   **allocates nothing**. With gzip compression enabled
   (`compress = true`; default is `false`, `crates/nzb-nntp/src/config.rs:138`)
   there is an additional `Vec::with_capacity(raw.len() * 4)` and a full copy at
   `connection.rs:950-975` (allocation at `:958`).
3. **Hand the raw buffer to the blocking pool.** `decode_and_assemble_offloaded`
   clones the `WorkItem` so it can cross into `spawn_blocking`
   (`crates/nzb-dispatch/src/download_engine.rs:4263-4276`, clone at `:4269`).
4. **Decode.** `decode_and_assemble` calls `decode_yenc(raw_data)` and receives a
   fresh `YencDecodeResult` (`download_engine.rs:4299-4310`, call at `:4304`).
   The decoded `Vec<u8>` is allocated inside `yenc-simd`
   (`Vec::with_capacity(data_region.len())`, `decode.rs:106`; `out.reserve(len)`
   in the SIMD kernels, `decode.rs:278`).
5. **Write out.** `assembler.assemble_article(...)` looks the file up with a
   freshly built `FileKey` of two `to_string()`s
   (`crates/nzb-decode/src/assembler.rs:456-458`) and `pwrite`s
   `&decoded.data` (`:506`). The decoded buffer is dropped when
   `decode_and_assemble` returns.
6. **Return the raw buffer.** Both call sites hand it back to the pool:
   `download_engine.rs:3027` (pipelined) and `:3970`/`:3977` (serial).
7. **Report progress.** `send_outcome` clones three `String`s per attempt
   (`download_engine.rs:161-174`); `ProgressUpdate::ArticleComplete` clones the
   job/file ids and the yEnc filename (`:3064-3073`).

### Allocation sites counted per article (steady state, uncompressed, pipelined)

| # | Site | Kind | Bytes | Owner |
|---|------|------|------:|-------|
| 1 | `pipeline.rs:105-107` `mid` + `format!("ARTICLE {mid}")` | small ×2 | ~60 B | repo |
| 2 | `connection.rs:2315` `cmd.to_string()` | small | ~30 B | repo |
| 3 | `connection.rs:357-363` body buffer | **pooled — 0** | 896 KiB reused | repo |
| 4 | `download_engine.rs:4269` `item.clone()` (`WorkItem`) | small ×~6 | ~200 B | repo |
| 5 | `yenc-simd decode.rs:106` decoded `Vec` | **1 large** | ~body size | **external** |
| 6 | `yenc-simd decode.rs:388-405` `extract_param` (`format!` + `to_string`) | small ×~5-8 | ~tens B | external |
| 7 | `yenc-simd decode.rs:382-384` `lossy_str` (`into_owned`) | small ×1-3 | ~line len | external |
| 8 | `assembler.rs:456-458` `FileKey` | small ×2 | ~40 B | repo |
| 9 | `download_engine.rs:161-174` `send_outcome` | small ×3 | ~60 B | repo |
| 10 | `download_engine.rs:3064-3073` `ArticleComplete` | small ×~4 | ~80 B | repo |

Repo-owned per article: roughly 12-16 small allocations. External per article:
1 large (≈ article body size) plus ~6-11 small header-parsing allocations.

### Where the bytes are

The decoded `Vec` in row 5 dominates by bytes and is the one that shows up in
the existing profile. `docs/internal/PERFORMANCE_STATUS.md:270-324` records a
live `perf record -F 999 -g` capture on a 1.100-era image during a ~3 GiB
transfer: `kernel_init_pages` self-time 7.45%, and the page-fault/allocator/free
kernel symbols sum to roughly **14-17% of total sampled CPU**, with
`rwsem_spin_on_owner` + `osq_lock` (~2.6%) indicating **mmap-semaphore
contention across the 8 workers**. The document attributes this to a fresh
`Vec` per article and to buffers that are not pre-touched.

Two mechanisms are worth naming because they decide which option helps:

- Articles are ~700-750 KiB encoded. That is above glibc's default
  `MMAP_THRESHOLD` (128 KiB), so each decoded `Vec` is a candidate for a fresh
  `mmap`/`munmap` pair rather than recycling from an arena. glibc raises the
  threshold dynamically after an mmap'd chunk is freed, which can mask this after
  warm-up — which is exactly why the profile, not source reading, has to decide.
- On **musl** (two of the four shipped targets, `rust-toolchain.toml:5-10`)
  there is no such arena behaviour to fall back on: musl's allocator services
  large allocations with `mmap` and releases them on free, so the churn is
  expected to be worst there. On **Windows MSVC** the process heap behaves
  differently again.

The `memchr` entry in the same profile (3.02%) is *not* `yenc-simd`'s scanning:
that crate's `memchr_byte` is a plain `iter().position()` loop
(`decode.rs:367-370`); the symbol belongs to tokio's read path.

## Options

### Option A — keep the system allocator, do nothing

- **Benefit:** zero risk, zero binary-size change, no behaviour change for the
  web server, SQLite, TLS or logging, and it preserves the existing raw-body
  pool contract. The read side is already fixed, and the pool already keeps
  ~896 KiB per connection warm.
- **Cost:** the ~14-17% page-fault/allocator/contention ceiling from
  `PERFORMANCE_STATUS.md:270-324` stays, because the largest per-article
  allocation still goes through the system allocator every article.
- **Risk:** none beyond leaving performance on the table.
- **Verdict:** the correct default until a real-path measurement says otherwise.

### Option B — global `mimalloc` (`#[global_allocator]`)

- **Benefit:** direct lever on exactly the observed signature. mimalloc retains
  freed segments and reuses their (already-zeroed) pages, so the repeated
  mmap/munmap + kernel zeroing of ~700 KiB buffers is the thing it removes; it
  also has per-thread free lists that reduce cross-worker contention. It is
  supported on musl and on Windows MSVC, so it applies to all four shipped
  targets, and it captures the `yenc-simd` Rust `Vec` because that allocation
  goes through the Rust global allocator.
- **Risk:** process-wide. The issue explicitly forbids changing behaviour for
  the web server, SQLite, TLS or logging without operator approval, and a global
  allocator does change allocation behaviour everywhere. Note it only redirects
  **Rust** allocations: C allocations from `rusqlite` (bundled sqlite3),
  `ring`, zlib and the like keep using the system `malloc` unless
  `MI_OVERRIDE`-style C interception is enabled, which the plain `mimalloc`
  crate does not do. Deployment surface: adding a dependency via
  `[workspace.dependencies]` (`AGENTS.md`), a feature flag so it can be
  A/B-tested, and re-validating the static musl builds that `cargo zigbuild`
  produces.
- **Binary size:** increases. No figure is quoted here — it must be measured per
  target with `size`/`cargo bloat` before adoption; static musl is the case to
  watch.
- **Unsafe surface:** none authored in this repo (the crate owns its `unsafe`);
  still needs operator approval and the issue's invariants/testing expectations.
- **Verdict:** the most promising single lever, but only behind a flag and only
  after measurement.

### Option C — global `jemalloc` (`tikv-jemallocator`)

- **Benefit:** same class of improvement as B for the large-allocation churn on
  Linux.
- **Risk:** weaker fit than mimalloc here. jemalloc must be *built* for the
  target, which complicates the `cargo zigbuild` musl cross-builds and adds a
  C toolchain step; Windows MSVC support is not first-class. It does not obviously
  beat mimalloc on the mmap-retention problem and costs more build complexity.
- **Binary size:** increases; measure if pursued.
- **Verdict:** not the first choice given the target matrix; B dominates it.

### Option D — pooled / reusable decoded-output buffer (the real fix)

- **Benefit:** removes the largest per-article allocation outright and keeps its
  pages permanently warm, which is the change
  `PERFORMANCE_STATUS.md:319-324` itself recommends. It composes with B (the
  allocator has one fewer large request to service) and is free of `unsafe` if
  done as a plain per-worker `Vec<u8>` scratch — no custom `Allocator`
  needed. `bytes::BytesMut` reuse is not a better fit here, because the decoder
  already needs a growable `Vec` and the write is a one-shot `pwrite`
  (`assembler.rs:506`), not a shared/sliced buffer.
- **Blocker:** the decoded buffer is created inside `yenc-simd 0.1.1` and the
  crate exposes only `decode_yenc(&[u8]) -> YencDecodeResult` and
  `encode_article(...)` (`docs.rs` for 0.1.1: `lib.rs:18-22`; no decode-into
  variant). There is nothing in this repo to reuse without either:
  - an upstream `decode_yenc_into(&[u8], &mut Vec<u8>)` (or equivalent) in a new
    `yenc-simd` release, which upstreams the lifetime boundary to the crate
    where it belongs; or
  - a local fork/vendor via `[patch.crates-io]`, which buys the win now at the
    cost of maintaining a fork of a published dependency (arguably against the
    "no new dependencies without cause" boundary in `AGENTS.md`).
  Copying the decoded `Vec` into a pooled buffer after the fact is pointless —
  it adds a full-size `memcpy` and keeps the fresh allocation.
- **Risk:** if a reusable decoder scratch is added, the invariants are the usual
  ones (clear before reuse, no aliasing of a buffer that is still being written),
  and the existing raw-body pool tests
  (`connection.rs:3568-3660`) are the model to copy. No `unsafe`. The risk today
  is scheduling/ownership, not soundness, because the change is blocked upstream.
- **Verdict:** the durable fix; pursue it as an upstream request, not as a fork.

### Option E — custom `Allocator`, per-article bump arena, slab

- **Benefit:** in principle, one arena reset per article instead of a dozen
  frees, and pre-touched pages.
- **Risk:** highest of all the options, and the issue lists why (hidden memory
  growth, lengthened object lifetimes, unsound `Allocator` impls, worse
  multi-worker contention). `Vec::new_in(System)` alone achieves nothing; the
  win requires a bump/arena allocator, i.e. real `unsafe` plus an invariants
  document, focused tests and Miri/ASAN, per the issue's done-when list.
- **Fit:** poor for the dominant cost. The largest allocation is *external*
  (`yenc-simd`), so an in-repo arena cannot capture it unless yenc-simd also
  accepts a caller-provided allocator/buffer — i.e. Option E does not subsume
  Option D, it needs D first. A slab for the small internal allocations
  (rows 1, 2, 4, 8-10) would remove count, but those are cheap; the page-fault
  cost is in the big buffer.
- **Verdict:** too much unsafe surface for the measured payoff. Do not start
  here. Revisit only if D and B together leave a material residue.

## Recommendation

In order, each gated on the previous one's measurement:

1. **Land the free, zero-unsafe, repo-owned cuts first.** They are independent of
   the allocator question and shrink allocation count where an arena would
   otherwise be needed:
   - `download_engine.rs:4269`: stop cloning the whole `WorkItem` into
     `spawn_blocking`; move only the fields the blocking task and its error
     message need (or pass an `Arc<WorkItem>`).
   - `connection.rs:2315` / `pipeline.rs:107` / `connection.rs:1029`: build the
     command into a reusable per-connection `String` scratch and pass `&str`
     instead of `format!`-then-`to_string`.
   - `assembler.rs:456-458`: avoid the two `to_string()`s per call with a
     borrowed-key lookup.
2. **Prototype a global `mimalloc` behind a non-default feature flag** and measure
   it on the real download path against the 1.100 baseline, on `x86_64-linux-gnu`
   and one musl target. This is the option that most directly attacks the
   measured page-fault/mmap-contention signature, needs no `unsafe` in this repo,
   and covers all shipped targets. It needs operator approval before it can
   become a default.
3. **File the upstream `yenc-simd` decode-into-buffer request** (Option D). If it
   lands, the reusable per-worker decoded scratch is the durable fix and removes
   the large allocation regardless of allocator. Do not fork the crate.
4. **Do not build a custom `Allocator`/arena** (Option E) on the current evidence.
   It cannot reach the dominant allocation, and its unsafe surface is not
   justified by the small internal allocations that remain after step 1.

Net: adopt nothing by default yet. The most defensible near-term change is the
feature-flagged mimalloc measurement plus the step-1 cleanups; the durable fix is
upstream in `yenc-simd`.

## What measurement would settle it

The issue's own done-when list is the right gate. Concretely, one experiment on
the **real download path** on a 1.100 image *confirmed to contain the change*:

- **Workload:** the same transfer the existing profile used
  (`PERFORMANCE_STATUS.md:270-324`), so the numbers are comparable. Do **not**
  build on `benchnzb` for the decision — it is excluded from the workspace
  (`Cargo.toml:16`), the issues record it as unstable, and the earlier
  36,351-`malloc`/3 MB-per-article figure in GH #327 is explicitly labelled
  supporting evidence, not a real profile.
- **A/B:** system allocator vs. mimalloc (feature flag), and, once available,
  fresh-decode vs. reused-decode-scratch.
- **Collect:** wall time, peak RSS, allocation count (an LD_PRELOAD counter or the
  allocator's own stats), and `perf record -F 999 -g` self-time for
  `kernel_init_pages`, `__rmqueue_pcplist`, `zap_present_ptes`, `copy_folio_from_iter_atomic`,
  `rwsem_spin_on_owner`, `osq_lock`.
- **Targets:** at minimum `x86_64-unknown-linux-gnu` and one musl target, since
  the mechanism (mmap/munmap churn) is expected to be strongest on musl; the
  Windows MSVC path needs a separate check if it is in scope.
- **Binary size:** `size` on the shipped artifacts, per target, before/after, for
  whichever allocator is being considered.

Settled when either (a) the page-fault/allocator symbols and peak RSS move
materially at unchanged or better wall time, in which case adopt the measured
option behind the flag and then by default with operator approval, or (b) they do
not move, in which case close #327 as measured-not-worth-it and keep the system
allocator plus the raw-body pool.

## Boundaries this note does not cross

- No source, `Cargo.toml`, CI, version or generated file was changed; this file
  is the only addition.
- No global allocator was introduced.
- No `unsafe` allocator code exists in this repo today, and none is proposed here
  beyond naming it as rejected.
- Any global allocator change and any `unsafe` allocator require explicit
  operator approval and an unsafe-review owner, per GH #327.
