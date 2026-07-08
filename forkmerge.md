# Fork Merge Status

This document records what was reviewed from `FutureMan0/rustnzb-restyle`, what was actually worth pulling back, what was reproduced in current code, and what still remains before release work can be finished.

## Source reviewed

- Fork: `https://github.com/FutureMan0/rustnzb-restyle`
- Local app repo: `rustnzbd`
- Local shared crates:
  - `Active/apps/libs/nzb-web`
  - `Active/apps/libs/nzb-postproc`
  - `Active/apps/libs/nzb-core`
  - `Active/apps/libs/nzb-nntp`

## Decision summary

Do not merge the fork wholesale.

The useful uplift set was small and split across app code and shared crates:

- Keep the `mark-read` batching fix
- Keep minor queue UI behavior fixes
- Keep shared-crate fixes only where current code was actually reproduced as broken
- Ignore major GUI restyle work
- Drop uplift lines that are already superseded upstream

## Item status

### 1. Batched mark-read fix in `rustnzbd`

Status: implemented and tested

Files:

- [src/group_handlers.rs](/home/sprooty/Working/Active/apps/rustnzbd/src/group_handlers.rs)
- [tests/group_mark_read.rs](/home/sprooty/Working/Active/apps/rustnzbd/tests/group_mark_read.rs)

What changed:

- Added a batched `mark_headers_read` helper
- `h_header_mark_read` now performs one `with_db` call for the full batch
- Failures are no longer silently swallowed as per-item best-effort behavior
- The returned `"marked"` count reflects actual successful work

Tests added:

- `mark_headers_read_counts_every_success`
- `mark_headers_read_stops_on_first_error`
- integration test `group_mark_read_updates_counts_and_response`

Verification completed:

- `cargo test mark_headers_read`
- `cargo test --test group_mark_read`

### 2. Minor queue UI hardening in `rustnzbd`

Status: implemented and tested

Files:

- [frontend/src/app/features/queue/queue-view.component.ts](/home/sprooty/Working/Active/apps/rustnzbd/frontend/src/app/features/queue/queue-view.component.ts)
- [frontend/src/app/features/queue/queue-view.component.spec.ts](/home/sprooty/Working/Active/apps/rustnzbd/frontend/src/app/features/queue/queue-view.component.spec.ts)
- [frontend/angular.json](/home/sprooty/Working/Active/apps/rustnzbd/frontend/angular.json)

What changed:

- Added per-row pending-action tracking
- Prevented duplicate pause/resume/delete clicks while a request is in flight
- Ensured queue reload happens after both success and failure
- Added explicit snackbar feedback for row actions
- Hardened invalid queue metric handling:
  - clamped `percent()` to sane bounds
  - returned `--` for invalid ETA inputs
  - normalized invalid remaining/duration inputs

Important implementation note:

- The first test pass exposed a real bug in the uplift draft. Passing an already-created observable still triggered duplicate HTTP calls. The fix was to pass an `actionFactory: () => Observable<unknown>` and only create the observable after the pending guard is checked.

Tests added:

- invalid percent clamping
- invalid ETA handling
- duplicate row action ignored while pending
- queue reload after success
- queue reload plus snackbar on failure

Verification completed:

- `./node_modules/.bin/ng test frontend --watch=false`
- `npm run build -- --configuration=production`

### 3. Shared-crate RAR/direct-unpack fixes

Status: reproduced, fixed, and verified in shared crates

#### 3a. `nzb-web` queue preemption regression

Status: reproduced on current code

Files:

- `Active/apps/libs/nzb-web/src/queue_manager.rs`
- `Active/apps/libs/nzb-web/tests/harness_preemption.rs`

What was reproduced:

- With `max_active_downloads(1)`, a low-priority downloading job can be preempted by a high-priority job and then remain paused forever after the high-priority job finishes.

Fix applied:

- Added `preempted: bool` tracking on `JobState`
- Marked jobs paused by automatic preemption as `preempted`
- Added `resume_preempted_jobs()` and invoked it from `start_next_queued()`
- Cleared the `preempted` flag on manual pause/resume and other non-preemption pause paths

Tests added:

- `preempted_job_resumes_after_high_priority_job_finishes`

Verification completed:

- `cargo test preempted_job_resumes_after_high_priority_job_finishes --test harness_preemption -- --nocapture`

#### 3b. `nzb-web` direct-unpack newline-free prompt hang

Status: reproduced on current code

File:

- `Active/apps/libs/nzb-web/src/direct_unpack.rs`

What was reproduced:

- The current prompt handling used newline-based reads and could hang when `unrar` printed the volume prompt without a trailing newline.

Fix applied:

- Switched prompt detection to byte-by-byte stdout processing
- Checked prompt/success/error conditions continuously instead of waiting for newline termination

Test added:

- `test_unrar_prompt_without_newline_is_detected`

Verification completed:

- `cargo test test_unrar_prompt_without_newline_is_detected --lib -- --nocapture`

#### 3c. `nzb-postproc` 7z no-password flag handling

Status: fixed conservatively with unit coverage

File:

- `Active/apps/libs/nzb-postproc/src/unpack.rs`

What changed:

- Split argument construction into helper functions
- Kept `-p-` for `unrar` no-password mode
- Omitted `-p-` for `7z` when there is no password

Tests added:

- `sevenz_password_arg_is_omitted_without_password`
- `rar_extract_args_keep_dash_password_only_for_unrar`
- `sevenz_extract_args_do_not_include_dash_password_without_password`

Verification completed:

- `cargo test unpack -- --nocapture`

### 4. Already-landed or superseded fork changes

Status: intentionally not uplifted

Do not pull these from the fork:

- SSRF protection changes
- parallel server-health check changes
- major visual restyle work

Reason:

- Current `rustnzbd` already has stronger SSRF validation and concurrent health checks
- The fork's large UI restyle is out of scope for this uplift

### 5. Queue preemption/prioritization work

Status: attempted reproduction first, then kept because current code was actually broken

This line should stay in scope. The reproduction pass proved the bug is real on current `nzb-web`, so the fix is not speculative.

## Consumer verification status

### Verified

1. `rustnzbd`

Completed checks:

- backend mark-read tests passed
- frontend unit tests passed
- frontend production build passed

### Consumer checks completed

1. `Active/apps/nzbservice/gui`

Result:

- `cargo check` passed after restoring the expected local `libs/` symlink layout and running Cargo with Forgejo package auth

Important limitation:

- this consumer still resolved `nzb-web v0.1.10` from the Forgejo registry
- the local `nzb-web v0.4.x` patch was not used because the consumer is on an older dependency line
- it therefore did not exercise the `nzb-web` forkmerge fixes

### Consumer checks that failed for reasons outside the forkmerge delta

1. `Active/apps/nzbservice/client`

Observed failure:

- after restoring the expected local `libs/` symlink layout and configuring Cargo auth, `cargo check` still failed
- failure is in the consumer itself, not the forkmerge changes:
  - `error[E0639]: cannot create non-exhaustive struct using struct expression`
  - call sites build `nzb_core::ServerConfig` directly against a newer `nzb-core` line

Interpretation:

- this consumer is on an older API line and is not currently compatible with the newer shared-crate heads already present in local development

### Consumer checks blocked by repo state

1. `myotherrepos/StackArr`

Blocker observed:

- workspace manifest references missing member `crates/stackarr-postgres`
- no such directory exists in the current checkout, so Cargo fails before dependency resolution

Interpretation:

- current `StackArr` checkout is not in a buildable state for this verification pass

## Verification notes

Rust formatting in `rustnzbd` is slightly awkward in this environment:

- plain `cargo fmt` fails unless the optional Forgejo registry is configured
- direct `rustfmt --edition 2024 ...` works for touched files

Cargo verification for Forgejo-registry consumers also needs the same auth setup used in CI:

- a temporary `CARGO_HOME`
- `[registries.forgejo] credential-provider = "cargo:token"`
- credentials sourced from `GIT_AUTH_TOKEN`, not the Forgejo API token

This does not invalidate the code changes, but it does matter for final release prep.

## Remaining work before release

1. Commit the `rustnzbd` uplift changes cleanly on top of current `main`
2. Commit the `nzb-web` shared-crate fixes cleanly without disturbing unrelated dirty files
3. Commit the `nzb-postproc` shared-crate fix cleanly without disturbing unrelated dirty files
4. Run full repo-level checks where environment allows:
   - `cargo fmt`
   - `cargo clippy -- -D warnings`
   - `cargo test`
5. Update `nzbservice/client` only if it is still considered a release-path consumer worth keeping on current shared-crate heads
6. Restore a buildable `StackArr` checkout if it is still considered a release-path consumer
7. Publish the moved shared-crate versions
8. Refresh `rustnzbd` dependency resolution and lockfile against published versions
9. Run one release-candidate build that does not rely on local path patches
10. Cut the `rustnzbd` release and generate release notes from the verified uplift set only

## Practical release order

If shared-crate releases are required:

1. release `nzb-postproc`
2. release `nzb-web`
3. update `rustnzbd` versions and lockfile
4. rerun `rustnzbd` verification
5. rerun consumer verification
6. build a release candidate
7. publish the app release

If shared-crate releases are deferred and local patches are acceptable temporarily:

1. land the app changes
2. document the local shared-crate dependency state explicitly
3. do not cut a public release until the shared crates are versioned and consumed cleanly

## Net result

The worthwhile uplift from the fork was real, but narrow:

- one backend handler correctness fix
- one small queue UI hardening set
- one real queue preemption fix in `nzb-web`
- one real direct-unpack prompt handling fix in `nzb-web`
- one conservative extractor-argument fix in `nzb-postproc`

Everything else should be treated as either superseded, out of scope, or requiring a separate product/UI decision.
