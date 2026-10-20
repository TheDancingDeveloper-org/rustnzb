# Changelog

All notable changes to rustnzb are listed here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project
uses [Semantic Versioning](https://semver.org/). Pre-releases
(`-beta.N`) are soak builds published to the `:beta` image tag; see
[docs/RELEASING.md](docs/RELEASING.md).

Entries are generated from the conventional-commit subjects between tags
(`feat` → Added, `fix` → Fixed, `perf` → Performance, `docs` →
Documentation). Test, CI and chore commits are left out. Each GitHub release
also carries generated release notes with the merged pull requests.

## [Unreleased]

### Added

- **config:** Back up and restore every setting as one TOML file, with a masked
  export by default and a validated import
- **ledger:** Prune damage-ledger rows for removed jobs and expired holes
- **queue:** Leave held PAR2 volumes out of queue size and record volumes fetched in history
- **dispatch:** Hold PAR2 recovery volumes until the slice map shows damage
- **queue:** Judge hopeless jobs by damaged slices against recovery slices
- **benchnzb:** Measure slice-verify cost per article against yEnc decode
- **dispatch:** Complete the slice map for late indexes, restarts and obfuscated files
- **dispatch:** Verify PAR2 slices in flight and emit the per-job slice map
- Wire the damage ledger into the download path
- **par2:** PAR2 set to SliceLayout and shared 16 KiB file matcher

### Fixed

- **queue:** Collapse the parity-tier if for the Rust 1.95 clippy lint
- **mock-nntp:** Use is_multiple_of for the new 1.95 clippy lint
- **webdav:** Drop par2 volume fields from the DAV history entry
- **desktop:** Sync the desktop lock file with nzb-core 0.2.21
- Batch ledger writes and confirm holes through the engine
- **ui:** Surface the remaining silently swallowed fetch errors

### Performance

- **history:** Read list columns from a side table, not past the NZB blob
- **ui:** Skip history polls while hidden or still loading
- **http:** Compress API and SPA responses, and leave detail-only fields out of
  history list rows (a 50-row history page drops from 67 KB to 5.2 KB on the
  wire)

## [1.7.0-beta.1] - 2026-10-18

### Added

- **sab:** Run change_complete_action and pass SAB script arguments
- **sab:** List logged warnings and add addlocalfile
- Attest release binaries with provenance and per-artefact SBOMs
- Release supply chain, single version source, OCI labels
- **ui:** Live tab title, quick speed limit, first-run banner, 404 route, statistics retry
- **ui:** Add opt-in browser notifications for downloads and disk pauses
- **notifications:** Add webhook, Apprise and ntfy targets
- **queue:** Detect duplicate NZBs by name and content digest
- **daemon:** Add --check-config, /api/ready, JSON logs and a shutdown timeout
- **config:** Reject unknown keys, validate values, write config 0600
- **ui:** Add DESIGN.md tokens, focus ring and phone breakpoints

### Fixed

- **sab:** Gate addlocalfile and widen change_complete_action triggers
- **ui:** Keep the header speed menu inside the viewport at phone width
- **ui:** Stop header menus clipping and the upload warning flashing
- **desktop:** Sync the desktop lock file with nzb-core
- **desktop:** Add regex to the desktop lock file for nzb-core
- **dispatch:** Bound undeclared yEnc files on the serial worker path
- **ui:** Show a retryable error when the queue or history fetch fails
- **settings:** Block Save until settings load, and show load errors
- **logging:** Write --log-file to a daily-rotating file
- **docker:** Make the compose stack and quickstart work for any host
- Resolve the operator config path once at startup
- Keep override values out of config.toml on every save
- Config writes are atomic, private and never persist overrides
- Mask secrets in config reads
- **a11y:** Name the history filters and the select-all checkbox
- **ui:** Show Offline and placeholder values instead of fake zeros
- **settings:** Replace inline font sizes with role classes
- **ui:** Use the accent-fill token under white text

### Documentation

- Add polish and QoL review (2026-10-17)
- Drop the live hostname and local paths from the UI audit docs
- **ui:** After screenshots and applied fix batch for the 2026-10-16 audit
- Handoff for UI fixes per DESIGN.md
- Add DESIGN.md design guide and point AGENTS.md at it
- Focused Settings typography review in UI audit
- Web UI audit 2026-10-16 with screenshots

## [1.6.0-beta.3] - 2026-10-13

### Fixed

- Recycle workers when server priority or connection parameters change

## [1.6.0-beta.2] - 2026-10-13

### Added

- Per-stage spans and metrics, scoped debug toggle, observability doc
- Live debug-logging toggle in settings

### Fixed

- **queue:** Progress save never writes a stale job status
- **dispatch:** Bound an undeclared file by the first yEnc size=, not the running min
- **dispatch:** Keep the first yEnc size= as the bound when the NZB declares none
- **dispatch:** Log a per-file yEnc size= range instead of warning per segment
- **dispatch:** Stop warning that yEnc size= disagrees with the NZB size
- Lowering concurrent jobs takes effect on active downloads

### Performance

- **queue:** Never hold the job list and DB locks together on progress saves
- **sabnzbd:** Filter SAB history over summary rows, load full rows per page
- **history:** Page, filter and aggregate native /api/history in SQL

## [1.6.0-beta.1] - 2026-10-12

### Added

- Paginate history server-side with filters and stats

### Fixed

- **web:** Point the About source link at the public GitHub repo
- **crates:** Clear lints the standalone CI's stable clippy rejects
- **crates:** Bump nzb-news/nzb-postproc past crates.io, align sibling floors
- **sync:** Standalone-buildable crate export, reviewed overwrite list, nzb-decode leftovers
- **desktop:** Pin nzb-dispatch 0.2.9 in the tauri lockfile
- **frontend:** Stop inlining critical CSS under the page CSP
- **groups:** Return 404 when a group id is unknown
- **web:** Drain progress listeners on shutdown instead of aborting them
- Write history and stats rows in one transaction
- **web:** Send browser security headers and restrict CORS to same-origin
- Make database migrations atomic with transaction wrapping
- **groups:** Pick the observation server by enabled and priority
- **rss:** Validate feed filter regex at save time and auto-download matching items
- **groups:** Page and aggregate group/header queries in SQL off the async threads
- Per-server header watermarks and dedicated browse connections
- **logs:** Reset Logs cursor when the server restarts
- **dav:** Accept the web UI session token on /dav; never refresh on DAV 401s
- **frontend:** Normalise WebDAV hrefs on the Media page
- **frontend:** Sequence queue polls so stale responses can't resurrect rows
- **frontend:** Stop concurrent tabs from logging each other out on refresh
- **frontend:** Shared size/speed formatter to EB; real speed limit on card
- **frontend:** Groups view filter, paging and fetch-poll teardown
- Count each job's server stats once; live progress as today
- Stop retry/restart double-counting downloaded bytes
- **startup:** Refuse second instance on the same data_dir
- **queue:** Flush article checkpoint for active downloads on graceful shutdown
- **api:** Keep multipart read failures as client errors on queue add
- **api:** Return 400 url_rejected for fetch-guard rejections
- **sab:** Keep the archives for pp=2 (repair+unpack without delete)
- **sab:** Implement mode=retry_all
- **core:** Strip bidi controls and zero-width spaces from job and file names
- **import:** Fall back to [[section]] header for SABnzbd INI names
- **sab:** Accept a target nzo_id as mode=switch value2
- **postproc:** Prefer 7zz over 7z when picking the 7-Zip binary
- **api:** Read category/priority/name from /api/queue/add form fields
- **sab:** Report the job's post-processing level as queue unpackopts
- **postproc:** Pass archive passwords to unrar/7z on stdin, not argv
- **config:** Honour general.log_level from config.toml
- **api:** Mask job archive passwords in the native queue response
- **sab:** Parse inline job passwords from job and file names
- **api:** Reject unknown and out-of-range fields on PUT /config/general
- **queue:** Report disk-space holds and lift them when space frees
- **api:** Validate imported categories on SABnzbd import apply
- **api:** 400/409 instead of 500 for bad input on add, browse and RSS download
- **api:** 404/409 for unknown and duplicate config entries; validate categories on save
- **server:** Layer the DAV extension in build_router so tests match prod
- **web:** Treat an RSS rule with no feeds as applying to all feeds
- **web:** Enforce a 60 s floor on RSS feed poll intervals
- **web:** Wake the RSS monitor on config changes and poll feeds per interval
- **web:** Make history retry idempotent per history entry
- **frontend:** Surface server error messages on failed mutations
- **web:** Count unknown ids as failed in bulk queue actions
- **web:** Honour sort field and direction in queue sort
- **postproc:** Deliver archives for download-only and repair-only jobs
- **web:** Close the three security blockers from the batch 2 review
- **queue:** Reclaim retained incomplete work dirs
- **queue:** Give same-named jobs unique complete folders
- **dispatch:** Apply speed-limit changes to parked bandwidth waiters
- **dispatch:** Keep pipelined article errors off the circuit breaker
- **sab:** Answer mode=warnings instead of "Unknown mode"
- **sab:** Give mode=server_stats SABnzbd's response shape
- **sab:** Accept .nzb.gz, .nzb.bz2 and .zip on addfile/addurl
- **watch:** Claim watched NZBs before enqueueing and wait for writes to settle
- **api:** Return 404/400 instead of 500 for unknown ids and invalid input
- **sab:** Honour the pp post-processing override on addfile/addurl
- **nntp:** Mark connection Error on STAT pipeline I/O failures
- **watch:** Import .zip and .nzb.bz2 archives from the watch folder
- **web:** Serialise config read-modify-write to stop lost updates
- **sab:** Honour nzbname and stop naming addurl jobs after the URL
- Close the remaining security blockers from the #158 review
- **config:** Default optional server fields and accept partial server updates
- **api:** Keep the stored server password when an update omits it
- **nzb-core:** Sanitize job names into portable directory names
- **postproc:** Normalise permissions of files extracted by unrar/7z
- **auth:** Revoke the whole session, including access tokens, on logout
- **web:** Allow LAN indexers through the fetch guard and validate RSS feed URLs on save
- **nzb-postproc:** Extract non-RAR archives left behind by direct unpack
- **nzb-postproc:** Only clean up old-style RAR volumes that belong to a set
- **nzb-postproc:** Accept only r..z for old-style RAR volume letters
- **dispatch:** Ignore a zero NZB size and cap per-article outage retries
- **dispatch:** Bound yEnc writes by the NZB size and stop counting outages as tries
- **desktop:** Bump nzb-decode to 0.1.5 in the src-tauri lockfile
- **dispatch:** Do not wait on unavailable optional servers
- **nzb-dispatch:** Drop in-flight serial fetch for an aborted job
- **nzb-dispatch:** Validate yEnc =ypart offset before writing
- **dispatch:** Fail over on article errors instead of retrying forever
- **dispatch:** Do not re-queue articles that already failed on resubmit
- **nzb-decode:** Track declared segment numbers instead of assuming 1..=N
- **web:** Respect max_active_downloads when resuming paused jobs
- **dispatch:** Honour speed limits below one article per second
- **sab:** Clamp timed pauses so a huge duration cannot panic
- **sab:** Never delete a history directory another job still uses
- **sab:** Report real disk space, uptime and servers
- **sab:** Implement mode=config speedlimit and set_pause setters
- **sab:** Fall back to the default category for unknown add categories
- **sab:** Report live speedlimit and pause_int in mode=queue
- **sab:** Support history delete by status and unbounded lookups
- **sab:** Answer mode=version and mode=auth without an API key
- **sab:** Report the default category as "*" in queue and history
- **sab:** Stop queue status flapping to Idle mid-download
- **sab:** Add jobs paused for SABnzbd priority -2
- **sab:** Resolve nzo_ids strictly and fix mode=switch
- **nntp:** Make Downloader use the server's connection pool
- **nntp:** Mark broken connections Error in pipeline and STAT paths
- **nntp:** Keep pool semaphore permits in step with checked-out connections
- **nntp:** Bound socket writes by the read-side timeout
- **nntp:** Parse status lines on bytes so multibyte UTF-8 cannot panic

### Documentation

- **config:** Document max_active_downloads = 0 as unlimited

## [1.5.0-beta.3] - 2026-09-29

### Fixed

- **web:** Confirm session before rendering and recover concurrent 401s

## [1.5.0-beta.2] - 2026-09-28

### Added

- **nzb-postproc:** Publish typed terminal failure codes
- **nzb-web:** Idempotent queue admission via Idempotency-Key
- **nzb-nntp:** Bounded NNTP observation API (ported from fork)

### Fixed

- **nzb-postproc:** Process every PAR2 recovery set, not just the first
- Bound worker shutdown to one deadline

## [1.5.0] - 2026-09-10

Tagged on the same commit as 1.5.0-beta.1, with no further changes.

## [1.5.0-beta.1] - 2026-09-10

### Added

- **nzb-decode:** In-flight PAR2 slice verifier core
- **nzb-core:** Per-article damage ledger
- Complete compatibility and hardening backlog

### Fixed

- **history:** Treat history_retention=0 as keep-all (#136)

## Earlier releases

Releases before 1.5.0 (1.0.0 to 1.4.7) are described on the
[GitHub Releases page](https://github.com/TheDancingDeveloper-org/rustnzb/releases).

[Unreleased]: https://github.com/TheDancingDeveloper-org/rustnzb/compare/v1.7.0-beta.1...HEAD
[1.7.0-beta.1]: https://github.com/TheDancingDeveloper-org/rustnzb/compare/v1.6.0-beta.3...v1.7.0-beta.1
[1.6.0-beta.3]: https://github.com/TheDancingDeveloper-org/rustnzb/compare/v1.6.0-beta.2...v1.6.0-beta.3
[1.6.0-beta.2]: https://github.com/TheDancingDeveloper-org/rustnzb/compare/v1.6.0-beta.1...v1.6.0-beta.2
[1.6.0-beta.1]: https://github.com/TheDancingDeveloper-org/rustnzb/compare/v1.5.0-beta.3...v1.6.0-beta.1
[1.5.0-beta.3]: https://github.com/TheDancingDeveloper-org/rustnzb/compare/v1.5.0-beta.2...v1.5.0-beta.3
[1.5.0-beta.2]: https://github.com/TheDancingDeveloper-org/rustnzb/compare/v1.5.0...v1.5.0-beta.2
[1.5.0]: https://github.com/TheDancingDeveloper-org/rustnzb/compare/v1.5.0-beta.1...v1.5.0
[1.5.0-beta.1]: https://github.com/TheDancingDeveloper-org/rustnzb/compare/v1.4.7...v1.5.0-beta.1
