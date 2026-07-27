# Releasing rustnzb

GitHub is the release authority for rustnzb. Release tags use the `vX.Y.Z`
format and must point to a commit reachable from `main`.

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

## Rollback

Do not move or replace a published tag. Revert the faulty change on `main`,
release a new patch version, and publish a new tag. Operators should pin an
immutable image tag rather than relying on `latest` for controlled rollouts.
