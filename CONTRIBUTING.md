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

By contributing, you agree to follow the [Code of Conduct](CODE_OF_CONDUCT.md).
