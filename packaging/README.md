# Distribution packaging

Drafts for channels beyond the GitHub release assets. Nothing here is
published by CI; each channel is submitted by the operator. The files are
generated from, or pinned to, the assets and `SHA256SUMS-<tag>.txt` that the
release workflow already produces.

| Path | Channel | Status |
|------|---------|--------|
| `unraid/rustnzb.xml` | Unraid Community Apps template | Ready to submit. Points at the public GHCR image `ghcr.io/thedancingdeveloper-org/rustnzbd:latest`. |
| `aur/PKGBUILD` | AUR `rustnzb-bin` | Ready to submit. Pinned to the `v1.6.0-beta.3` tarball with its published SHA-256. |
| `aur/update-pkgbuild.sh` | Refresh the PKGBUILD for a release | Verifies the tarball against the release `SHA256SUMS` before writing. |
| `homebrew/Formula/rustnzb.rb` | Homebrew formula (Linux tap or `homebrew-core`) | Template, pinned to the `v1.5.0` Linux tarballs. Not published. |
| `homebrew/update-formula.sh` | Refresh the formula for a release | Verifies both Linux tarballs against the release `SHA256SUMS` before writing. |
| `scoop/bucket/rustnzb.json` | Scoop manifest | Template, pinned to the `v1.5.0` NSIS installer. Not published. |
| `winget/manifests/t/…` | winget manifest set | Template, pinned to the `v1.5.0` NSIS installer. Not published. |
| `distribution/render-windows-manifests.sh` | Regenerate Scoop + winget from one checksum file | Writes both channels in one pass; verifies the installer when given `--assets-dir`. |

Everything is pinned to the last **stable** release (`v1.5.0`). Pre-release
tags are not offered to package managers.

## Windows: one asset, two manifests

The Windows release asset is an NSIS installer
(`rustnzb-v<tag>-windows-x86_64-setup.exe`). Both Windows channels describe
that one file, so they are rendered together:

```bash
# from a checkout, using the published checksum file
packaging/distribution/render-windows-manifests.sh v1.5.0 --release-date 2026-09-18

# or from downloaded release assets, verifying the installer's hash too
packaging/distribution/render-windows-manifests.sh v1.5.0 \
    --sums SHA256SUMS-v1.5.0.txt \
    --assets-dir .ci-output/packages \
    --release-date 2026-09-18
```

The release workflow runs the same script as a dry run against the assets it
just built, and uploads the result as the
`rustnzb-<tag>-distribution-manifests` artefact. It never submits anything.

- **Scoop** extracts the installer with `Expand-7zipArchive` rather than
  running it, so no registry entries, file associations or uninstaller are
  created. `7zip` must be installed in the same Scoop root; the manifest says
  so in its notes. `checkver`/`autoupdate` track stable GitHub releases.
- **winget** uses `InstallerType: nullsoft`, `Scope: machine` and
  `ElevationRequirement: elevatesSelf`, matching the NSIS script's
  `RequestExecutionLevel admin`. It declares the `rustnzb` command and the
  `.nzb` file extension that the installer registers.

### Submitting the Windows manifests

winget (after reviewing the generated files under
`packaging/winget/manifests/t/TheDancingDeveloper/rustnzb/<version>/`):

```powershell
winget validate --manifest packaging/winget/manifests/t/TheDancingDeveloper/rustnzb/1.5.0
winget install --manifest packaging/winget/manifests/t/TheDancingDeveloper/rustnzb/1.5.0
# then, from a winget-pkgs checkout:
wingetcreate submit packaging/winget/manifests/t/TheDancingDeveloper/rustnzb/1.5.0
```

Scoop: copy `packaging/scoop/bucket/rustnzb.json` into your bucket, then
`checkver rustnzb` and `scoop install rustnzb`.

The installer is **not Authenticode-signed**. winget accepts unsigned
installers, but SmartScreen warns on first run; that is a known precondition,
not a defect in the manifest.

## Homebrew

The formula installs the prebuilt Linux binaries — `x86_64` by default and
`aarch64` on Apple Silicon Linux hosts (`on_arm`). It depends on `sevenzip`
for extraction; Homebrew has no `unrar` formula (only a cask), so direct
unpack stays off unless the user installs one separately.

There is no macOS artefact in the release, so the formula is Linux-only
(`depends_on :linux`). A macOS formula needs a macOS binary or a source build
first; that is out of scope here.

```bash
# refresh for a new tag: verifies both tarballs against SHA256SUMS-<tag>.txt
packaging/homebrew/update-formula.sh v1.5.0

# local check from the repository root
brew install --build-from-source ./packaging/homebrew/Formula/rustnzb.rb
brew audit --strict --new-formula ./packaging/homebrew/Formula/rustnzb.rb
brew test ./packaging/homebrew/Formula/rustnzb.rb
```

`brew` is not available in this workspace, so the formula has not been
executed here. Its URLs and hashes were verified against the published
release, and `update-formula.sh` was run end to end to produce them.

To ship it, copy `Formula/rustnzb.rb` into your tap (or submit it to
`homebrew-core` as a new formula) yourself. Do not add a bottle block by hand;
Homebrew builds and publishes bottles.

## cargo-auditable binaries

The standalone release binaries and the container image are built with
[`cargo-auditable`](https://github.com/rust-secure-code/cargo-auditable), which
embeds the resolved crate list in a `.dep-v0` section. `ci/tasks/auditable`
pins the tool version and tarball checksum; the release workflows assert the
section is present in both shipped Linux binaries before generating SBOMs. That
is what makes the per-artefact SPDX files dependency-accurate instead of
heuristic. See [docs/RELEASING.md](../docs/RELEASING.md#sboms-and-dependency-accuracy).

## Not yet covered

- **macOS**: no dmg and no macOS binary, so no Homebrew-on-macOS formula.
- **AppImage**: not built.
- **Copying cosign signatures to the mirrors**: undecided; the signature and
  certificate stay on the GitHub release next to `SHA256SUMS-<tag>.txt`.
- **Publishing from CI**: deliberately absent. Every channel here is an
  operator submission.
