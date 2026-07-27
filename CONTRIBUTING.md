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

The containerized task interface in [`ci/run`](ci/run) provides local parity
with selected build tasks. See [docs/DEVELOPMENT.md](docs/DEVELOPMENT.md) for
the supported commands.

## Pull requests

- Branch from current `main` and keep each pull request focused.
- Add or update tests for behavior changes.
- Run the relevant Rust and frontend checks above before requesting review.
- Describe user-visible behavior, compatibility implications, and test
  coverage in the pull request.
- Do not include credentials, generated build output, or `node_modules`.

By contributing, you agree to follow the [Code of Conduct](CODE_OF_CONDUCT.md).
