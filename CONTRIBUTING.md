# Contributing to rustnzb

Thanks for helping improve rustnzb. GitHub is the canonical project home for
issues, pull requests, releases, and source code.

## Before opening an issue

1. Search existing [issues](https://github.com/TheDancingDeveloper-org/rustnzb/issues).
2. Include the rustnzb version, operating system, installation method, and
   relevant non-secret logs.
3. Remove provider credentials, API keys, NZB URLs, and personal paths from
   reports.

Security-sensitive reports must follow [SECURITY.md](SECURITY.md), not the
public issue tracker.

## Development setup

```bash
git clone https://github.com/TheDancingDeveloper-org/rustnzb.git
cd rustnzb
cargo build -p rustnzb
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

For the Angular frontend:

```bash
cd apps/rustnzb/frontend
npm ci --no-audit --no-fund
npm test -- --watch=false
npm run build -- --configuration=production
```

Compatibility and security regression tests are deterministic and must remain
network-independent. Use the focused harness gates while iterating:

```bash
cargo test -p nzb-web --tests --locked
cargo test -p rustnzb --tests --locked
cargo test -p nzb-postproc --tests --locked
```

Golden responses are reviewed as API contract changes: keep dynamic type
markers for timestamps, rates, paths, and generated identifiers, and update
the fixture README when the capture source changes. Do not add credentials,
provider URLs, private hostnames, or personal paths to fixtures or logs.

The containerized task interface in [`ci/run`](ci/run) provides local parity
with selected build tasks. See [docs/DEVELOPMENT.md](docs/DEVELOPMENT.md) for
the supported commands.

## AI-assisted contributions

AI coding agents and other AI-assisted tools are welcome contributors to this
project. They are held to the same engineering and community standards as any
other contribution.

The person submitting or merging an AI-assisted change remains responsible for:

- reviewing and understanding the complete change;
- running the required tests and accurately reporting their results;
- ensuring the change does not include credentials, private infrastructure
  details, personal data, generated artifacts, or copied material without an
  appropriate license; and
- responding to review feedback and maintaining the contribution after merge.

Use of AI does not require a co-author trailer or a contributor-credit entry.
When it materially helps reviewers—for example, for a broad refactor or a
generated test matrix—briefly describe the tool's role in the pull-request
body. Do not present unverified AI output as tested or production-ready.

## Pull requests

- Branch from current `main` and keep each pull request focused.
- Add or update tests for behavior changes.
- Run the relevant Rust and frontend checks above before requesting review.
- Describe user-visible behavior, compatibility implications, and test
  coverage in the pull request.
- Do not include credentials, generated build output, or `node_modules`.

## How changes land

Every pull request runs two required checks, `policy` and `rust`. They are
small on purpose: the merge queue is where the full suite runs.

- **Targeted checks on a pull request.** The `rust` check builds, lints and
  tests the crates your diff touches, with a shared build cache and
  `cargo-nextest`. A change to the lockfile, the root `Cargo.toml`, the
  toolchain, `ci/`, `.github/` or the nested workspaces falls back to the full
  workspace. Run the same commands locally before you push; see
  [docs/ci.md](docs/ci.md) for the path-to-crate mapping.
- **The queue proves the exact tree.** `merge_group` runs the full workspace,
  the frontend, `container-smoke`, and the quarantined `e2e` and `desktop`
  jobs. `e2e` and `desktop` do not run on pull requests, so a PR no longer
  pays for them.
- **No run on push to `main`.** A queue entry lands the tree its `merge_group`
  run already tested, so nothing repeats the suite after the merge.
- **The app version bumps at release, not in feature PRs.** The
  `[workspace.package]` version in the root `Cargo.toml` is the version the
  shipped binary and the release tag use. `ci/check-version.sh` runs only when
  a release tag is cut, and nothing gates a feature PR on it: do not bump the
  workspace version, and do not rebase to resolve version numbers, as part of
  an ordinary change. A crate that publishes on its own (`crates/nzb-*`) still
  bumps its own version in its own `Cargo.toml` when its public API changes —
  that is a per-crate fact, not a release version. Release PRs bump versions
  only; they do not carry feature work.
- **Release changes get a dry run.** A PR that touches
  `.github/workflows/release*.yml`, `ci/tasks/**`, `packaging/**` or
  `installer/**`, and a nightly run on `main`, builds the release artefacts
  without publishing, signing or creating a release
  (`.github/workflows/release-dry-run.yml`). A release tag therefore should
  not be the first time the packaging path runs.

By contributing, you agree to follow the [Code of Conduct](CODE_OF_CONDUCT.md).
