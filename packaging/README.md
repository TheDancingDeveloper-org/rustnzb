# Distribution packaging

Drafts for channels beyond the GitHub release assets. Nothing here is
published by CI; each channel is submitted by the operator.

| Path | Channel | Status |
|------|---------|--------|
| `unraid/rustnzb.xml` | Unraid Community Apps template | Ready to submit. Points at the public GHCR image `ghcr.io/thedancingdeveloper-org/rustnzbd:latest`. |
| `aur/PKGBUILD` | AUR `rustnzb-bin` | Ready to submit. Pinned to the `v1.6.0-beta.3` tarball with its published SHA-256. |
| `aur/update-pkgbuild.sh` | Refresh the PKGBUILD for a release | Verifies the tarball against the release `SHA256SUMS` before writing. |

## Submitting

- **Unraid:** submit `unraid/rustnzb.xml` to the Community Apps repository
  through its submission process. Re-check the `Repository` tag and the WebUI
  port against the release image on each release.
- **AUR:** on an Arch host, run `makepkg --printsrcinfo > .SRCINFO` next to the
  PKGBUILD, then push the two files to the `rustnzb-bin` AUR git repository.
  Run `update-pkgbuild.sh <tag>` first when moving to a new release. The
  PKGBUILD has not been built here: `makepkg` is not installed in this
  workspace, so the packaging step was checked by running `package()` against
  the real release archive.

## Not yet covered

Homebrew, winget/Scoop, and dmg/AppImage builds are follow-ups.
