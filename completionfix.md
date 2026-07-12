# Completion, Failure Classification, and PAR2 Repair Fix

Date: 2026-07-12
Scope: rustnzb download completion, NNTP failure routing, hopeless-abort logic,
PAR2 recovery, terminal state handling, history persistence, and diagnostics.

## Executive summary

Node B is producing a recurring class of false failed downloads reported as:

```text
Aborted: only 100.0% of content available (need 100.2%),
6 of N content articles missing
```

The visible percentage is misleading, but it is not the primary failure. For
the investigated releases, NNTP providers returned `400 Idle timeout` on stale
connections. rustnzb classified those responses as protocol errors, rapidly
cycled the affected articles through all configured providers, and then
promoted the transient errors to definitive `NotFound` failures. The sixth
promoted failure crossed the five-article grace threshold and aborted each job.

The completion calculation also records but does not use PAR2 recovery
capacity. In addition, the abort path passes zero failed articles into
post-processing, causing PAR2 verification to be skipped even when recovery
files exist. Concurrency and history-state defects produce repeated aborts,
post-processing races, inaccurate counters, and duplicate history insertion
errors.

This must be repaired as a coordinated failure-path change. Adjusting the
configured completion percentage alone will not address the underlying errors.

## Live evidence

### Investigated releases

```text
Furiosa.A.Mad.Max.Saga.2024.1080p.AMZN.WEB-DL.DDP5.1.H.264.DUAL-RiPER
Aborted: only 100.0% of content available (need 100.2%),
6 of 16584 content articles missing

The.Deer.Hunter.1978.1080p.AMZN.WEB-DL.DDP2.0.H.264.DUAL-jus
Aborted: only 100.0% of content available (need 100.2%),
6 of 20017 content articles missing
```

At the abort times, Loki showed the corresponding NZB message IDs returning:

```text
Pipeline error: Protocol error: Unexpected ARTICLE response 400: Idle timeout.
```

The same message IDs were attempted on all four active providers:
`server_1`, `server_2`, `server_6`, and `server_7`. Message IDs from both
failure bursts were matched against the NZB data retained in the Node B
history database, confirming that the errors belonged to the two named jobs.

The failures were therefore not confirmed `430 Article Not Found` responses.
They were transient connection/session failures that became permanent article
failures through incorrect routing.

### Failure pattern

For the reviewed 36-hour Loki window:

| Result | Jobs |
|---|---:|
| Reached history | 51 |
| Completed | 12 |
| Failed | 39 |
| Aborted by hopeless detection | 33 |
| `ongoing_availability` aborts | 32 |
| `early_failure` aborts | 1 |

Most `ongoing_availability` failures stopped at exactly six missing articles.
That is a direct consequence of `HOPELESS_GRACE_ARTICLES = 5`, rather than
evidence that unrelated posts consistently have exactly six missing segments.

### PAR2 evidence

The two named NZBs contain no PAR2 files. If their articles were genuinely
missing on every provider, they would not be repairable. However, their
recorded failures came from idle-timeout responses, so they should not have
been counted as missing in the first place.

At least one retained Law & Order failure did include PAR2 data. It was aborted
after six alleged missing articles, but its history recorded:

```text
Verify: Skipped — zero article failures
```

This confirms that the abort path bypasses PAR2 verification by passing a zero
failure count into post-processing.

## Root causes

### 1. Transient NNTP errors become permanent missing articles

The pipelined NNTP parser maps an unexpected `400 Idle timeout` response to
`NntpError::Protocol` in:

```text
crates/nzb-nntp/src/pipeline.rs
```

The shared download engine then sends that protocol failure through
`handle_article_not_available`. Once every enabled provider has either been
tried or is circuit-broken, the function converts every failure other than a
decode error to `not_found_anywhere`:

```text
crates/nzb-web/src/download_engine.rs
```

This destroys the original failure classification. Protocol, timeout,
authentication, permission, service, and connection failures can all become a
definitive global `NotFound` result.

Required invariant:

> An article is definitively missing only when every eligible, healthy
> provider explicitly returns an article-absence response such as NNTP `430`.

A provider that is unavailable, circuit-broken, unauthenticated, timed out, or
returning protocol/session errors has not established article absence.

### 2. Idle sessions are not reconnected in the pipelined response path

Connection and I/O failures trigger requeue and reconnect behavior. Generic
per-article pipeline errors instead flow through article-availability routing.
An NNTP `400 Idle timeout` indicates that the provider considers the session
invalid or expired. It must invalidate the connection, requeue affected
in-flight work, and reconnect. It must not consume an article's provider
availability attempt.

The fix should recognize the response semantically rather than matching only
one provider's exact text where possible. At minimum, `400` responses that
indicate idle/session timeout must use the connection-loss path. Other
unexpected protocol responses should remain transient unless they explicitly
establish article absence.

### 3. Circuit-broken providers incorrectly count as article attempts

The `all_tried` calculation treats either of these states as equivalent:

```text
provider explicitly tried for this article
provider currently unavailable according to server health
```

They are not equivalent. A circuit-broken provider supplies no evidence about
whether the article exists. If untried providers are temporarily unavailable,
the job should pause or retry after provider recovery rather than finalize the
article as missing.

### 4. The completion formula excludes PAR2 capacity

`HopelessTracker` calculates and stores `par2_bytes` but explicitly does not use
it. The ongoing availability check is currently:

```text
content_available / total_content * 100
```

The configured default is `100.2`, and the configuration comment says PAR2
overhead makes completion above 100% normal. The formula and documentation
therefore contradict each other. Content-only availability cannot exceed 100%.

The intended high-level calculation is:

```text
effective_completion_pct =
    (content_bytes_available + usable_recovery_capacity_bytes)
    / total_content_bytes
    * 100
```

The `100.2%` requirement then represents a small safety margin after accounting
for recovery capacity.

Usable recovery capacity must not be the raw sum of every `.par2` file:

- the PAR2 index file is metadata, not recovery capacity;
- only recovery volume blocks should count;
- unavailable or failed recovery segments must be deducted;
- exact block counts are preferable to byte approximations;
- capacity should be associated with the correct PAR2 set;
- obfuscated filenames must not cause recovery files to be classified as
  content permanently.

PAR2 files are already prioritized in the shared queue. The completion gate
should wait until recovery capacity is known before declaring content damage
hopeless.

### 5. Missing-byte accounting is approximate

For a failed segment, the queue manager estimates its size using:

```text
file total bytes / file article count
```

The NZB model already stores the declared byte size for each article. The
failure update includes the file ID and segment number, so exact declared
segment bytes should be used. Average estimates can distort availability near
the threshold, particularly for the last segment of a file.

### 6. The abort path suppresses PAR2 verification

When the engine emits `JobAborted`, the queue manager calls:

```rust
self.on_job_finished(&job_id, false, 0).await;
```

The post-processing pipeline skips PAR2 verification whenever
`articles_failed == 0`. Consequently, an aborted job can log that PAR2 may
repair it and then deliberately skip PAR2 verification.

This path needs an explicit semantic split:

- **Repairable damage:** continue the download, finish with the real failed
  article count, and run PAR2 verification/repair.
- **Confirmed hopeless damage:** abort, skip repair/extraction, and move
  directly to failed history with a precise reason.
- **Transient provider failure:** pause/retry; do not count it as content
  damage and do not enter post-processing.

Passing the real count into the existing abort pipeline is necessary for
correct accounting, but it is not sufficient. A job declared genuinely
hopeless should not waste time extracting or repairing known-unrecoverable
content.

### 7. Terminal transitions are not single-owner or fully drained

Several failure updates can already be queued when the first one triggers an
abort. The progress handler processes subsequent updates and calls abort again,
which produced six-, seven-, and eight-missing abort logs for one Furiosa job.

The worker pool emits the terminal abort immediately after draining queued
items. In-flight workers may still hold job context or assembler state, and
progress updates may remain buffered. Post-processing can therefore begin
before all writes and counters are settled.

Observed consequences include:

- repeated abort decisions for one job;
- `downloaded_bytes = 0` despite files being present for extraction;
- post-processing running with stale failure/download counters;
- possible overlap between assembler writes and verification/extraction;
- inconsistent per-server and per-job statistics.

Each job needs one atomic terminal transition, such as:

```text
Downloading -> AbortRequested -> Draining -> Failed
Downloading -> Finishing -> PostProcessing -> Completed/Failed
Downloading -> PausedTransient
```

Only the owner of the successful transition should initiate drain,
post-processing, or history persistence. Abort must wait for in-flight work to
stop and assembler handles to close before any pipeline stage starts.

### 8. Terminal queue removal writes history twice

Completed and failed jobs remain visible briefly in the queue after history is
inserted. Radarr then removes the queue entry. `remove_job` sees an error
message and attempts another plain `INSERT` into history, producing:

```text
UNIQUE constraint failed: history.id
```

Removal must distinguish an active failed job from a terminal row already
persisted to history. History movement should be idempotent, and deletion of a
temporary terminal queue view must not reinsert history or interrupt
post-processing.

### 9. Diagnostics cannot reliably correlate worker errors to jobs

Worker-level pipeline warnings contain article and worker/server fields but no
`job_id`. With multiple jobs active, Loki cannot directly associate transient
NNTP errors with the job that later aborts. Queue-manager failure logs contain
`job_id` but lose the original message ID and original multi-provider failure
sequence.

Every article fetch/retry/failure event should include at least:

```text
job_id
file_id
segment_number
message_id
server_id
worker_id
original_failure_kind
terminal_failure_kind, if finalized
attempt number
```

The final article decision should summarize provider outcomes without emitting
high-cardinality message IDs as permanent metric labels.

## Required design

### Failure provenance

Replace the simple list of tried server IDs with per-provider outcomes for the
article. A conceptual representation is:

```text
server_id -> ExplicitNotFound | TransientFailure(kind) | DecodeFailure | Success
```

Finalization rules:

1. Success on any provider resolves the article successfully.
2. Explicit `430` from every eligible healthy provider resolves it as
   definitively missing.
3. A mix of explicit absence and transient/unavailable providers remains
   unresolved and is retried or paused.
4. Decode failures retain their distinct classification and should not be
   rewritten as absence.
5. Authentication or permission failures should break/pause the affected
   provider and surface an operator-actionable reason.

### Repair budget

Track, preferably in PAR2 recovery blocks:

- total content size;
- exact confirmed missing content bytes or affected PAR2 source blocks;
- total recovery blocks declared by usable volume files;
- successfully available recovery blocks;
- unavailable recovery blocks;
- a configurable safety reserve corresponding to the required completion
  percentage.

Do not abort for missing content while available recovery capacity plus its
safety margin can cover the damage. If exact block mapping cannot be known
during download, use a conservative interim estimate and make the final
decision after PAR2 files are assembled and parsed.

For NZBs without PAR2, confirmed missing content can be considered
unrecoverable, but only after explicit absence is established across healthy
providers. The five-article grace constant should not be the fundamental
repair model.

### Early-failure detection

The early-failure detector is useful for completely dead posts but must operate
only on definitive failures. It should also avoid treating ten closely grouped
transient responses as a statistically meaningful sample.

Recommended constraints:

- count only explicit global absence or validated decode corruption;
- exclude connection/session/provider failures;
- use both a minimum sample count and a minimum fraction of the job;
- compare projected damage against known recovery capacity;
- retain a fast path for a post where all sampled articles receive explicit
  `430` from every healthy provider.

### Terminal ownership

Introduce an atomic or mutex-protected terminal state. The first successful
transition owns teardown. Later article updates may update diagnostics if safe,
but must not initiate another abort or overwrite the original terminal reason.

The owner must:

1. stop new work from being claimed;
2. drain queued work;
3. cancel or await in-flight work;
4. close assembler handles;
5. settle final counters;
6. decide between retry/pause, repair pipeline, or direct failed history;
7. persist history once;
8. publish one terminal event.

## Implementation sequence

### Phase 1: Correct transient error routing

- Map NNTP idle/session timeout responses to a reconnect-required error.
- Requeue all affected pipeline items and replace the connection.
- Preserve the original failure kind across provider attempts.
- Finalize `NotFound` only after explicit absence from all eligible providers.
- Stop treating circuit-broken providers as evidence of absence.

This phase addresses the immediate cause of the two investigated releases.

### Phase 2: Make completion PAR2-aware

- Replace unused `par2_bytes` with usable recovery capacity.
- Track unavailable recovery data.
- Use exact article sizes for content damage.
- Gate hopeless decisions on repair capacity plus safety margin.
- Keep no-PAR behavior explicit and separately tested.

### Phase 3: Repair terminal state handling

- Add a single-owner terminal state machine.
- Ignore duplicate abort triggers after `AbortRequested`.
- Drain/await workers and close assemblers before post-processing.
- Pass final, accurate counters into post-processing.
- Separate repairable, hopeless, and transient outcomes.

### Phase 4: Make history and cleanup idempotent

- Persist terminal history exactly once.
- Treat removal of an already-persisted terminal queue row as view cleanup.
- Prevent queue deletion from aborting active post-processing.
- Eliminate duplicate-history constraint errors without hiding unexpected DB
  conflicts.

### Phase 5: Improve observability

- Add job context to worker-level article events.
- Log original and final failure classifications.
- Emit one structured terminal summary per job.
- Include content damage, recovery capacity, safety reserve, and the exact
  reason a job is repairable or hopeless.
- Add counters for transient reconnects, explicit global absence, repairable
  damage, hopeless aborts, duplicate terminal attempts, and PAR2 outcomes.

## Test plan

### NNTP classification tests

- One provider returns `400 Idle timeout`, reconnect succeeds, article
  downloads.
- Every provider initially returns idle timeout, all reconnect, job completes.
- One provider returns `430`, another succeeds: article succeeds.
- Some providers return `430`, another is circuit-broken: article remains
  unresolved rather than globally missing.
- Every healthy provider explicitly returns `430`: article becomes definitive
  `NotFound`.
- Authentication, permission, `502`, protocol, and connection failures never
  become `NotFound` solely because all providers were attempted.

### PAR2 completion tests

- More than five missing content articles remain repairable when recovery
  blocks cover them.
- Damage exactly at capacity respects the configured safety reserve.
- Damage beyond available recovery capacity aborts as hopeless.
- Missing PAR2 volume segments reduce usable capacity.
- The PAR2 index contributes no recovery capacity.
- Multiple PAR2 sets use capacity only for their associated content.
- Obfuscated PAR2 filenames are identified or conservatively deferred.
- A no-PAR release with explicit global `430`s aborts with an unambiguous
  message.

### Post-processing tests

- Repairable article failures pass the real count to PAR2 verification.
- PAR2 verification does not skip when content failures exist.
- A genuinely hopeless abort does not run extraction or cleanup stages.
- Post-processing starts only after all assembler handles and in-flight writes
  are closed.

### Concurrency and persistence tests

- Many simultaneous failures produce one abort decision and one terminal
  event.
- Progress counters are settled before history insertion.
- Radarr deletion during the terminal visibility window does not insert
  history twice.
- Deleting a genuinely active failed/paused job preserves one history entry.
- Cancellation during PAR2 verification cannot remove its work directory.

### Observability tests

- Worker errors carry `job_id`, file, segment, article, and server context.
- Final logs retain original failure provenance.
- Percentage formatting does not round a sub-100 value to a misleading
  `100.0%` without also showing exact missing bytes and recovery capacity.

## Acceptance criteria

The work is complete when all of the following are true:

1. Replaying the Furiosa and The Deer Hunter NZBs against providers returning
   stale-session `400 Idle timeout` responses reconnects rather than recording
   missing articles.
2. Only explicit global article-absence responses contribute to the missing
   content budget.
3. A circuit-broken or unavailable provider cannot help prove global absence.
4. PAR2 recovery capacity participates in the completion decision.
5. Repairable jobs reach PAR2 verification with accurate article-failure and
   recovery-block counts.
6. Hopeless jobs skip inappropriate extraction/repair work and retain the
   original failure reason.
7. Each job emits one terminal transition and creates one history row.
8. No pipeline stage begins while article workers can still write job files.
9. Loki can correlate every terminal article decision with its job and
   provider outcomes.
10. Unit, integration, workspace, Clippy, formatting, and relevant E2E tests
    pass.

## Operational guidance before the fix

Changing `required_completion_pct` is not a sufficient mitigation. Setting it
to `100.0` would still reject `99.96%`, and values below 100 are currently
clamped. Disabling hopeless abort may prevent the sixth-failure cutoff, but it
does not correct transient errors being finalized as missing and can allow a
job to continue with incorrectly failed segments.

Restarting the service may temporarily replace stale NNTP sessions, but the
problem can recur after connections idle again. Retrying affected jobs is only
meaningful after fresh connections are established and should be monitored for
new `400 Idle timeout` bursts.

The durable solution is the classification, repair-budget, and terminal-state
work described above.
