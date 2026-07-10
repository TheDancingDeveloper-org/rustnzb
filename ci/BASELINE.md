# Pre-migration baseline (2026-07-10)

- Last successful main pipeline before the plan: Woodpecker rustnzb repo 38,
  pipeline 204, commit `aad5563b03e9f99866730216e044fc4adce5516d`, about 567 seconds.
- Plan commit pipeline 209 failed after about 224 seconds. Several concurrent
  tasks expired in the agent queue and the workflow ended with a duplicate
  Docker network error. This is why the converged workflow sequences the
  heavyweight gates instead of starting every build at once.
- The pre-migration Forgejo `dev` reference resolved to
  `sha256:57840249a2812a622b075c1df3ba4c96d639c59c410af295a6b7b6f6d7d15e30`.
  Preserve it as the first production rollback candidate.
- The old pipeline downloaded sccache in each Rust step, installed Rust
  components at runtime, installed Playwright/browser packages in E2E, and
  installed GTK packages in desktop. It built the frontend both in a separate
  task and conditionally inside Docker.
- The host filesystem had 57 GiB free (97% used) before migration work. Every
  arm64 attempt must capture Buildx usage and filesystem high-water marks.
- Forgejo API, Forgejo git, and Woodpecker API authentication passed. The
  secondary GitHub PAT returned HTTP 401 and must be rotated before GHCR or a
  GitHub release can be validated; Forgejo work is unaffected.

Runtime metrics are written under `.ci-output/metrics` by image-build and cold
build tasks. Fill the cold/warm comparison in the final migration record from
the first successful production-runner pipelines; local timings are not a
substitute for runner metrics.

