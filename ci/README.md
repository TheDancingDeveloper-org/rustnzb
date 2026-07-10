# Container-first build interface

`./ci/run TASK` is the supported way to reproduce a Woodpecker gate locally.
It runs the checked-in `ci/tasks/TASK` script in the exact image digest recorded
in `ci/images.lock`, with the checkout mounted at `/woodpecker/src` just as it
is in CI. Docker Buildx is required. In the Sprooty workspace,
`mydevenv2-agent-auth` supplies Forgejo credentials in memory for tasks that
need the private WebDAV crates.

Common commands:

```sh
./ci/run fmt
./ci/run check
./ci/run test
./ci/run clippy
./ci/run e2e
./ci/run build-image rustnzb:local
./ci/run smoke-image rustnzb:local
./ci/run --cold-cache test
```

Warm local runs use Docker named volumes for Cargo downloads and npm downloads.
Targets and declared artifacts are always written beneath `.ci-output` or
`.ci-artifacts`; the host `target/`, Cargo installation, and npm installation
are never mounted. `--cold-cache` omits the download volumes and forces sccache
recompilation. When invoked inside MyDevEnv2, the wrapper translates the
container checkout path to the source path visible to the host Docker daemon.

## Task contract

| Task | Image | Private registry | Declared output | Architectures | Network after dependencies |
|---|---|---:|---|---|---:|
| `fmt` | core | yes (manifest resolution) | none | amd64 host | no |
| `check` | core | yes | `.ci-output/targets/check` | amd64 host | no |
| `test` | core | yes | `.ci-output/targets/test`, `benchnzb-test` | amd64 host | no |
| `clippy` | core | yes | `.ci-output/targets/clippy` | amd64 host | no |
| `frontend-test` | core | no | coverage console output | amd64 host | no |
| `frontend-audit` | core | no | policy result | amd64 host | yes, advisory API |
| `e2e` | e2e | yes | failure-only `.ci-artifacts/e2e` | amd64 host/Chromium | no |
| `desktop-test` | desktop | yes | `.ci-output/targets/desktop-test` | Linux amd64 | no |
| `build-linux` | cross | yes | `.ci-output/targets/build-linux` | Linux amd64 | no |
| `build-linux-arm64` | cross | yes | `.ci-output/targets/build-linux-arm64` | Linux arm64 output on amd64 | no |
| `build-windows` | cross | yes | `.ci-output/targets/build-windows` | Windows amd64 output on Linux amd64 | cargo-xwin may fetch SDK objects on first use |
| `package-release` | cross | no | `.ci-output/packages` | amd64, arm64, Windows amd64 | no |
| `build-image` | host Buildx | yes | named local image | amd64 or arm64 | registry/dependency access |
| `smoke-image` | host Docker | registry pull only | failure-only `.ci-artifacts/runtime-smoke` | native runtime | no |

All source tasks consume tracked source and lockfiles. A task removes any
frontend `dist` it created. No task consumes an untracked `frontend/dist` or a
host `target/`. Release packaging is the one explicit cross-step artifact
consumer and its three binary input paths are listed in `package-release`.

## Toolchain image update and bootstrap

1. Change version arguments/checksums in `Dockerfile.ci` and commit the source.
2. Run `./ci/run build-ci-images`. It builds and pushes SHA and human-readable
   tags to Forgejo, runs each candidate's self-test after pulling it, and prints
   the resolved digest assignments.
3. Copy those four assignments into `ci/images.lock` and update the immutable
   references in `.woodpecker.yml` in the same commit.
4. Run `ci/verify-image-pins`, then all local parity gates.
5. Push to Forgejo and monitor Woodpecker. Do not delete the replaced manifests.

For rollback, restore all four `PREVIOUS_*` digest values to the active values
and the matching Woodpecker commit. Retain at least two superseded toolchain
generations. The initial custom-image bootstrap has no custom predecessor; its
rollback is the pre-migration commit using the pinned upstream images.

## Candidate promotion and rollback

Main and tag pipelines first push immutable candidates. The runtime task pulls
the Forgejo candidate, runs the binary smoke test, starts it with isolated
config/data, polls `/api/health`, checks the embedded frontend and build ref,
checks `7z`, and shuts down cleanly. Only then does promotion copy the candidate
digest to mutable Forgejo tags and from Forgejo to GHCR. A failed gate does not
run promotion.

Record the previous `dev`/`latest` digest before each promotion. Rollback is a
Skopeo copy of that digest back to the mutable tag, followed by the same remote
digest checks and runtime smoke test; it never recompiles source.

## Caches, retention, and arm64

Redis sccache namespaces include Rust version and task/features. Production
Buildx uses a Forgejo registry cache; cache deletion only slows the next build.
Never cache credentials, `target`, frontend `dist`, package outputs, databases,
or runtime data. Registry administrators own retention: retain two toolchain
generations and the current plus previous production cache; delete older cache
tags before deleting immutable release or SHA candidates.

The current arm64 candidate is cross-built on amd64. The pipeline records disk
usage around the Buildx step and executes the target binary's smoke test under
Buildx emulation, then verifies the manifest. Native arm64 runtime execution is
still a release risk until an arm64 Woodpecker runner is provisioned. If the
recorded high-water mark exceeds the runner's configured safety threshold, use
a native runner; do not add generic privileged DinD.
