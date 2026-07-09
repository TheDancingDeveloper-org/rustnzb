# Release Procedure — rustnzb + NZB Crates

End-to-end release flow: publish all NZB shared crates to crates.io (and Forgejo), bump rustnzb, merge to the public branch, tag, and ship GitHub release artefacts.

---

## CI Docker Publishing Model

Woodpecker publishes two different Docker tracks:

- `main` pushes publish development images
- tag pushes publish release images and advance `latest`

### Development images (`main` pushes)

Every push to `main` publishes amd64-only Docker images to both registries:

- Forgejo: `repo.indexarr.net/indexarr/rustnzbd:dev`
- Forgejo: `repo.indexarr.net/indexarr/rustnzbd:<full-commit-sha>`
- GHCR: `ghcr.io/ausagentsmith-org/rustnzb:dev`
- GHCR: `ghcr.io/ausagentsmith-org/rustnzb:<full-commit-sha>`

Rules:

- `dev` is the moving integration tag for unreleased `main`
- `latest` must not move on ordinary branch pushes
- arm64 is still release-only because the current buildx worker exhausts its overlayfs during routine `main` cross-builds

### Release images (tag pushes)

Pushing `vX.Y.Z` publishes this release set:

1. Forgejo per-arch tags:
   - `repo.indexarr.net/indexarr/rustnzbd:vX.Y.Z-amd64`
   - `repo.indexarr.net/indexarr/rustnzbd:vX.Y.Z-arm64`
   - `repo.indexarr.net/indexarr/rustnzbd:latest-amd64`
   - `repo.indexarr.net/indexarr/rustnzbd:latest-arm64`
2. Forgejo multi-arch tags:
   - `repo.indexarr.net/indexarr/rustnzbd:vX.Y.Z`
   - `repo.indexarr.net/indexarr/rustnzbd:latest`
3. GHCR multi-arch mirrors:
   - `ghcr.io/ausagentsmith-org/rustnzb:vX.Y.Z`
   - `ghcr.io/ausagentsmith-org/rustnzb:latest`

`latest` therefore always means "most recent tagged release", not "most recent commit on main".

### CI verification and failure mode

Woodpecker verifies registry state after publishing:

- `main` pushes verify Forgejo `:dev` and `:<sha>`, then GHCR `:dev` and `:<sha>`
- tag pushes verify Forgejo `:vX.Y.Z` and `:latest` for amd64 and arm64, then GHCR `:vX.Y.Z` and `:latest` for amd64 and arm64

If an unrelated earlier step has already failed the workflow, downstream Docker mirror steps can be skipped even when the Forgejo image build itself succeeded. When that happens, do not assume GHCR is current just because Forgejo is.

## Phase 1 — Publish NZB crates to crates.io + Forgejo

### Crates in scope

In dependency order (must publish bottom-up):

```
Level 0 (no internal deps):  rust-yenc-simd, nzb-nntp, rust_par2
Level 1:                     nzb-decode (→ rust-yenc-simd)
                             nzb-core   (→ nzb-nntp)
Level 2:                     nzb-dispatch (→ nzb-nntp, nzb-core)
                             nzb-postproc (→ nzb-core, rust_par2)
Level 3:                     nzb-news     (→ nzb-dispatch)
Level 4:                     nzb-web      (→ nzb-news, nzb-decode, nzb-postproc)
```

All crates live under `~/Working/Active/apps/libs/<crate>/`.

### Pre-flight

```bash
# Ensure crates.io token is loaded
export CARGO_REGISTRY_TOKEN=$(infisical secrets get CARGO_CRATES_IO_TOKEN \
  --domain https://se.sprooty.com \
  --projectId 6d6caff5-7aaf-42f8-a135-2455d7629af8 \
  --env prod --plain)

# Forgejo token (already in ~/.cargo/credentials.toml usually)
# Used implicitly by --registry forgejo
```

### Per-crate procedure (repeat in dependency order)

For each crate `C`:

1. **Sync working tree**
   ```bash
   cd ~/Working/Active/apps/libs/$C
   git checkout main && git pull
   ```

2. **Resolve any feature branches** — confirm any `feat/*` or `release/*` branches are merged or intentionally discarded. Do not publish from a non-main branch.

3. **Quality gates**
   ```bash
   cargo fmt
   cargo clippy --all-targets -- -D warnings
   cargo test
   ```

4. **Bump version** in `Cargo.toml` (semver: patch for fixes, minor for features, major for breaking).

5. **Update downstream `Cargo.toml`** — if `C` is a dep of a crate being published in the same batch, bump the dep version there too.

6. **Commit + push to Forgejo**
   ```bash
   git add -A
   git commit -m "chore: bump to v<new-version>"
   git push origin main
   ```

7. **Publish to Forgejo first, then crates.io**
   ```bash
   cargo publish --registry forgejo
   cargo publish                       # crates.io (uses CARGO_REGISTRY_TOKEN)
   ```
   Forgejo first because consuming apps' CI fetches from Forgejo. crates.io publishes are immutable — verify version is correct.

8. **Tag the release**
   ```bash
   git tag v<new-version>
   git push origin --tags
   ```

9. **Wait for indexing** — crates.io can take 30–60s before the new version is resolvable. Verify with `cargo search <crate>`.

### Update consuming apps

After all crates publish, bump versions in each consuming app's root `Cargo.toml`:

- `~/Working/Active/apps/rustnzbd/Cargo.toml`
- `~/Working/Active/apps/Arz/Cargo.toml`
- `~/Working/Active/apps/nzb-mirror/Cargo.toml`
- `~/Working/Active/apps/rustnzbindxer/Cargo.toml`
- `~/Working/Active/apps/rustNewsreader/Cargo.toml`

Strip or refresh any `[patch.*]` sections that point at local checkouts. CI strips them anyway, but local builds should match.

---

## Phase 2 — Release rustnzb

Run from `~/Working/Active/apps/rustnzbd/`.

### 1. Land all pending work on `main`

```bash
git checkout main && git pull
git status                            # working tree must be clean (or only release-related changes)
```

Drop or commit stray files (`Dockerfile.local`, `e2e/playwright-report/`, etc.). Do not commit `.claude/scheduled_tasks.lock`.

### 2. Bump rustnzb version

Edit `Cargo.toml` → `[package] version = "X.Y.Z"`. Update any in-repo references (changelog, frontend `package.json` if mirrored).

```bash
cargo update -p rustnzb               # refresh lock entry
cargo build --release --features webdav   # smoke verify
cargo test --workspace
```

### 3. Commit + push to Forgejo

```bash
git add Cargo.toml Cargo.lock <changelog files>
git commit -m "release: vX.Y.Z"
git push origin main
```

Wait for the Forgejo dev image publish to complete for this commit. A `main`
pipeline can still end red if unrelated quality gates fail after
`build-and-push-dev` has already published the deployable image.

Expected Docker result for a normal `main` push:

- Forgejo `repo.indexarr.net/indexarr/rustnzbd:dev`
- Forgejo `repo.indexarr.net/indexarr/rustnzbd:<full-commit-sha>`
- GHCR `ghcr.io/ausagentsmith-org/rustnzb:dev`
- GHCR `ghcr.io/ausagentsmith-org/rustnzb:<full-commit-sha>`

`latest` should remain unchanged here.

### 3a. Deploy a dev build to Komodo

For Komodo-managed stacks, deploy the exact Forgejo commit image, not `:dev`
and not GHCR:

1. Confirm Woodpecker published:
   - `repo.indexarr.net/indexarr/rustnzbd:<full-commit-sha>`
2. Update the target stack in `indexarr/ops` to pin that exact image tag.
   Current arr stack path:
   - `personal/arr/compose.yaml`
3. Push the ops commit, then trigger a redeploy:

```bash
mydevenv2-agent-auth run -- python3 \
  ~/.codex/skills/komodo-stack-deploy/scripts/deploy_stack.py personal-arr
```

4. Verify the live container on Node B:

```bash
tailscale ssh sprooty@winrarhost \
  'docker ps --format "table {{.Names}}\t{{.Image}}\t{{.Status}}" | grep rustnzb'
```

Notes:

- Prefer the Forgejo `:<sha>` tag for dev deploys. GHCR mirror steps can be
  skipped if earlier jobs fail, even when the Forgejo image already exists.
- Do not advance `latest` for dev testing. `latest` remains tied to the most
  recent tagged release.

### 4. Tag the release on Forgejo

```bash
git tag vX.Y.Z
git push origin vX.Y.Z
```

The tag push triggers the release pipeline (cross-compile binaries, scp to `dl.rustnzb.dev`, Docker → Forgejo + GHCR, Discord notification).

Expected Docker result for the tag:

- Forgejo per-arch: `vX.Y.Z-amd64`, `vX.Y.Z-arm64`, `latest-amd64`, `latest-arm64`
- Forgejo multi-arch: `vX.Y.Z`, `latest`
- GHCR multi-arch: `vX.Y.Z`, `latest`

### 5. Merge `main` → `public-main` and push to GitHub

`public-main` is the public branch mirrored to GitHub.

```bash
git checkout public-main
git pull origin public-main
git merge --no-ff main -m "release: vX.Y.Z"
git push origin public-main           # Forgejo
git push github public-main           # GitHub mirror
```

Then tag on GitHub too:

```bash
git push github vX.Y.Z
```

### 6. Build + publish GitHub release artefacts

The Forgejo release pipeline builds Linux + Windows binaries and uploads to `dl.rustnzb.dev`. Mirror those to a GitHub Release:

```bash
# Pull binaries the Forgejo pipeline produced
ssh root@100.92.4.57 ls /var/www/dl.rustnzb.dev/vX.Y.Z/

# Locally stage them
mkdir -p /tmp/rustnzb-vX.Y.Z && cd /tmp/rustnzb-vX.Y.Z
scp root@100.92.4.57:/var/www/dl.rustnzb.dev/vX.Y.Z/* .

# Create GitHub release with artefacts
gh release create vX.Y.Z \
  --repo AusAgentSmith-org/rustnzb \
  --title "rustnzb vX.Y.Z" \
  --notes-file RELEASE_NOTES.md \
  ./*
```

`RELEASE_NOTES.md` should summarise: notable features, fixes, breaking changes, upgrade notes, and bumped crate versions.

### 7. Verify

- [ ] Forgejo dev image exists for the `main` commit SHA you plan to deploy
- [ ] Tag pipeline green
- [ ] Komodo deployed new container (check `http://192.168.1.75:3011`)
- [ ] `dl.rustnzb.dev/vX.Y.Z/` contains Linux + Windows binaries
- [ ] Forgejo has `repo.indexarr.net/indexarr/rustnzbd:vX.Y.Z`
- [ ] Forgejo has `repo.indexarr.net/indexarr/rustnzbd:latest`
- [ ] GHCR has `ghcr.io/ausagentsmith-org/rustnzb:vX.Y.Z`
- [ ] GHCR has `ghcr.io/ausagentsmith-org/rustnzb:latest`
- [ ] GitHub release published with artefacts attached
- [ ] Discord changelog webhook fired
- [ ] `public-main` on GitHub is at the new tag

---

## Rollback

- **Bad crate published**: crates.io is immutable. Yank with `cargo yank --version X.Y.Z <crate>`, then publish a fixed patch version.
- **Bad rustnzb release**: revert the offending commit on `main`, bump patch version, repeat Phase 2. Force-pushing tags is forbidden.
- **Bad Komodo deploy**: edit the image SHA in the relevant
  `repo.indexarr.net/indexarr/ops` stack compose file back to the previous SHA
  (currently `personal/arr/compose.yaml` for the arr stack), push that ops
  commit, then re-trigger DeployStack via the Komodo API or the deploy script.

---

## Notes

- **No Co-Authored-By Claude/AI lines in commits.** (Workspace rule.)
- **Forgejo is always pushed first**, GitHub second.
- **Pre-push hooks** in lib repos run `cargo fmt --check` + `cargo clippy`. Fix locally before retrying.
- **`[patch]` sections** are stripped by CI — they only help local builds.
- **Major bumps to `nzb-nntp` or `nzb-core`** ripple into nearly every app — review the dependency matrix in `~/Working/CLAUDE.md` before tagging.
