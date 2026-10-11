# Releasing rustnzb

GitHub is the release authority for rustnzb. Release tags use the `vX.Y.Z`
format and must point to a commit reachable from `main`.

## Release channels

The container image is published to GHCR (`…/rustnzbd`) on three tag channels:

- **`:latest`** — the current stable release. Moved only by a stable
  `vX.Y.Z` release.
- **`:beta`** — the newest pre-release, for soak testing before a stable
  promotion. A pre-release tag **never** moves `:latest`.
- **`:vX.Y.Z`** / **`:vX.Y.Z-beta.N`** — immutable, one per tag.

Pre-release tags use a suffix: `vX.Y.Z-beta.N` (also `-rc.N` / `-alpha.N`).
They are flagged as GitHub pre-releases and publish only `:vX.Y.Z-beta.N` and
`:beta`. Cutting a pre-release tag is how a build enters soak testing.

## Beta soak and promotion

Prefer shipping a stable release only after a pre-release has soaked:

1. Tag a pre-release on a `main` commit, e.g. `v1.5.0-beta.1`, and push it.
   The release workflow publishes `:v1.5.0-beta.1` + `:beta` and a GitHub
   pre-release. `:latest` is untouched.
2. Let it soak. Bump the suffix (`-beta.2`, …) for fixes during soak.
3. When satisfied, run the **Promote pre-release to stable** workflow
   (`workflow_dispatch`) with the soaked tag. It copies the **exact image
   digest** that soaked to `:v1.5.0` + `:latest`, then rebuilds the standalone
   release binaries from the same commit and publishes the stable GitHub
   release.

Because promotion copies the soaked image by digest, `:v1.5.0` is byte-identical
to the beta that was tested; only the tag changes. The image's own embedded
build ref therefore still reads the pre-release version it was built as — that
is the promoted lineage, not a rebuild. The standalone binaries are rebuilt and
carry the stable version.

A stable `vX.Y.Z` tag pushed directly (a hotfix without soak) still works and
takes the rebuild path below.

## Release checklist

1. Update the workspace version in `Cargo.toml` and any user-visible version
   metadata.
2. Run the checks in [DEVELOPMENT.md](DEVELOPMENT.md).
3. Merge the release change to `main` through a reviewed pull request.
4. Create and push an annotated `vX.Y.Z` tag on that `main` commit.
5. Monitor the GitHub Actions release workflow.
6. Verify the GitHub release assets, checksums, and multi-architecture GHCR
   image:

   ```text
   ghcr.io/thedancingdeveloper-org/rustnzbd:vX.Y.Z
   ghcr.io/thedancingdeveloper-org/rustnzbd:latest
   ```

GitHub generates release notes from the tagged history. Review them before
publishing; do not add version-specific release-note files to the repository.

## Standalone nzb-* crates

The seven `nzb-*` crates under `crates/` are published to crates.io from
standalone repositories (one per crate, named in `ci/nzb-crates.txt`, URL in
each crate's `repository` field). This monorepo is the head:

- **The monorepo owns version numbers.** Change a crate's `version` only in
  `crates/<name>/Cargo.toml`; the standalone repos never originate a bump.
  Keep each new version strictly above the crates.io maximum for that crate.
- **Standalone repos are sync targets.** `ci/tasks/sync-crate <name>` exports
  `crates/<name>` at `HEAD` and opens a pull request on that crate's standalone
  repo. It never publishes and never pushes to the monorepo.
- **Publish only from a synced tag.** After a sync PR merges, publish that
  crate from a `vX.Y.Z` tag on the standalone repo — never from an out-of-sync
  tree.
- **Drift is gated.** The scheduled `crate-drift` job
  (`.github/workflows/quality-schedule.yml`, via `ci/tasks/crate-drift`) fails
  when any standalone repo's tree or package version differs from the
  monorepo. A failure means a crate changed here without a sync; run
  `ci/tasks/sync-crate <name>`, merge, and publish from the tag.
- **A sync never deletes standalone-only files.** `ci/sync-preserve.txt` lists
  paths the standalone repos own (CI workflows, license, renovate). Both the
  sync and the drift check leave those alone, unless the monorepo has started
  shipping the same path, in which case the monorepo copy wins. Everything else
  in the standalone tree — including a workspace member such as `nzb-decode`'s
  `ffi/` — must live in the monorepo, or the next sync will not carry it and
  drift will fail.
- **A sync refuses over unported work.** `sync-crate` exits 3 when a commit
  since the last `sync: <crate> from rustnzb monorepo` commit on the standalone
  `main` changes a path the monorepo does not export byte-for-byte. Port that
  change into the monorepo first. A commit whose every non-preserved path
  already matches the export is treated as absorbed.

## Rollback

Do not move or replace a published tag. Revert the faulty change on `main`,
release a new patch version, and publish a new tag. Operators should pin an
immutable image tag rather than relying on `latest` for controlled rollouts.
