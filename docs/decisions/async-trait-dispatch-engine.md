# Decision: keep `async-trait` for `DispatchEngine`; drop it from `nzb-web`

- **Status:** decided
- **Date:** 2026-11-25
- **Issue:** GH #323 / Vogt WI-4772 — "Evaluate async-trait removal for DispatchEngine"

## Context

`nzb-web` and `nzb-dispatch` both declared `async-trait = "0.1"`.
`crates/nzb-dispatch/src/dispatch_engine.rs` applies
`#[async_trait::async_trait]` to the `DispatchEngine` trait and to its one
implementation, `DispatchHandle`. The queue layer owns the engine as
`Arc<dyn DispatchEngine>` (`crates/nzb-web/src/queue_manager.rs:2078`), so the
trait must stay dyn-compatible.

Rust now supports `async fn` in traits (RPITIT / AFIT), but such traits are
**not** dyn-compatible, so a naive deletion is not possible: the choice is the
macro, or hand-written boxed-future signatures.

## Findings

- `rg` found **no** `async_trait` use under `crates/nzb-web` (src or tests):
  the dependency there was a leftover. Removed.
- `DispatchEngine` has exactly **one** async method, `shutdown`; every other
  method is synchronous. `Arc<dyn DispatchEngine>` is a real requirement
  (`crates/nzb-web/src/queue_manager.rs:2078`, and the engine is the boundary
  between queue and dispatcher by design — see the module docs and
  `crates/nzb-dispatch/src/lib.rs`).
- `async-trait` remains in the workspace dependency graph through `tonic`
  (via `opentelemetry-otlp` / `opentelemetry-proto`) regardless of what the
  two internal crates do, so removing it here does not remove the crate from
  the build.

## Decision

**`nzb-web`:** drop the unused `async-trait` dependency (done; `Cargo.lock`
updated).

**`nzb-dispatch`:** **keep `#[async_trait]`** on `DispatchEngine` and
`DispatchHandle`.

### Why

1. **It is a published public API.** `nzb-dispatch` is published to
   crates.io and the root `Cargo.toml` patches it back by path. Converting
   `async fn shutdown(&self)` to
   `fn shutdown(&self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>>` is a
   breaking change to the trait's public signature, so per AGENTS.md it would
   have to ship with a version bump and a minor/major release — out of scope
   for a tidy-up, and a cost with no functional gain.
2. **`dyn` is the whole point.** The trait exists to be a `dyn` boundary. AFIT
   is ruled out (breaks dyn); that leaves the macro or hand-written boxed
   futures, and the macro's only real advantage over hand-writing is not
   spelling out the `Pin<Box<dyn Future + Send + '_>>` types. There is little
   to gain and a public-API break to pay.
3. **Keeping it avoids the `double_must_use` juggling for free.** The trait
   already carries a documented `#[allow(clippy::double_must_use)]` for the
   `#[must_use]` the macro generates; that is a known, commented exception, not
   a new one.
4. **No workspace-level payoff.** `async-trait` still compiles for `tonic`, so
   the change would not shorten the dependency graph or the build in any
   meaningful way.

## Experiment (compile-time observation)

The hand-written boxed-future variant was tried before deciding, not merely
reasoned about. The exact shape below compiled clean under the pinned
toolchain (1.99.1) and passed
`cargo clippy -p nzb-dispatch -p nzb-web --all-targets -- -D warnings`:

```rust
fn shutdown(&self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>>;

// impl:
fn shutdown(&self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
    Box::pin(async move { self.0.shutdown().await })
}
```

So the boxed form is *technically* safe and simple — `Send`, the `'_` lifetime
and object safety all hold. The decision to keep the macro is therefore
deliberate and not driven by a compile failure: it is made on the public-API
versioning cost and the absence of any real benefit, with the experiment
recorded here should a future release choose to take the break anyway.

The experiment was reverted; `dispatch_engine.rs` keeps `#[async_trait]`.

## Effect on tests

No signature changed, so no existing test needed updating. The `dyn` path is
covered by the dispatch crate's integration tests, which drive the pool
through the `DispatchEngine` trait (`crates/nzb-dispatch/tests/`:
`shared_budget.rs`, `worker_recycle.rs`, `slice_map.rs`, `defer_volumes.rs`,
`yenc_size_bound.rs`), and by `nzb-web`'s queue-manager call sites — e.g.
`self.dispatch.shutdown().await` at
`crates/nzb-web/src/queue_manager.rs:6932`. `cargo test -p nzb-dispatch` passes.

## Revisit when

- the crate is next cut for a breaking release, in which case the boxed form
  above can be adopted at no extra cost; or
- `dyn DispatchEngine` stops being needed (e.g. the queue layer is made generic
  over the engine), at which point AFIT becomes viable and the macro can go.
