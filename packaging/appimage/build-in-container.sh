#!/usr/bin/env bash
# Run build-appimage.sh inside the CI cross image, installing squashfs-tools
# and curl there, so the runner host needs nothing beyond docker.
#
#   packaging/appimage/build-in-container.sh IMAGE TAG ASSETS_DIR
#
# ASSETS_DIR is relative to the repository root. The runner may itself be a
# container talking to the host's docker daemon, so a bind mount of $PWD would
# name a host path that does not exist; the workspace is copied in and the
# assets copied back with docker cp, as the release build step does. Only the
# script, the icon and the two Linux tarballs go in: copying the workspace
# would drag .ci-output/targets (many GB) onto the daemon's disk a second time.
set -euo pipefail
image=${1:?image}; tag=${2:?tag}; assets=${3:?assets dir}
container="rustnzb-appimage-${GITHUB_RUN_ID:-$$}-${GITHUB_RUN_ATTEMPT:-0}"
cleanup() { docker rm -f "$container" >/dev/null 2>&1 || true; }
stage=$(mktemp -d)
trap 'cleanup; rm -rf "$stage"' EXIT
mkdir -p "$stage/packaging/appimage" "$stage/desktop/src-tauri/icons" "$stage/$assets"
cp packaging/appimage/build-appimage.sh "$stage/packaging/appimage/"
cp desktop/src-tauri/icons/128x128.png "$stage/desktop/src-tauri/icons/"
for arch in x86_64 aarch64; do
  cp "$assets/rustnzb-$tag-linux-$arch.tar.gz" "$stage/$assets/"
done
docker create --name "$container" -w /workspace -e TAG="$tag" -e ASSETS="$assets" \
  --entrypoint sh "$image" -ec '
    install_deps() { apt-get update -qq && apt-get install -y -qq --no-install-recommends squashfs-tools curl ca-certificates; }
    if ! command -v mksquashfs >/dev/null || ! command -v curl >/dev/null; then
      # Stale or truncated lists give "InRelease is not signed"; start clean and
      # fall back to https if the plain-http mirror path is mangled.
      rm -rf /var/lib/apt/lists/*
      install_deps || {
        rm -rf /var/lib/apt/lists/*
        sed -i "s|http://deb.debian.org|https://deb.debian.org|g" /etc/apt/sources.list /etc/apt/sources.list.d/* 2>/dev/null || true
        install_deps
      }
    fi
    packaging/appimage/build-appimage.sh "$TAG" --assets-dir "$ASSETS"
  ' >/dev/null
docker cp "$stage/." "$container:/workspace"
mkdir -p "$stage/out"
docker start -a "$container"
mkdir -p "$assets"
# Only the AppImages come back; the tarballs are already here.
docker cp "$container:/workspace/$assets/." "$stage/out"
cp "$stage"/out/*.AppImage "$assets/"
cleanup
