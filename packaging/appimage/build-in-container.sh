#!/usr/bin/env bash
# Run build-appimage.sh inside the CI cross image, installing squashfs-tools
# there, so the runner host needs nothing beyond docker.
#
#   packaging/appimage/build-in-container.sh IMAGE TAG ASSETS_DIR
#
# ASSETS_DIR is relative to the repository root. Files written into it are
# handed back to the invoking user.
set -euo pipefail
image=${1:?image}; tag=${2:?tag}; assets=${3:?assets dir}
docker run --rm -v "$PWD:/workspace" -w /workspace \
  -e TAG="$tag" -e ASSETS="$assets" -e OWNER="$(id -u):$(id -g)" \
  "$image" sh -ec '
    trap "chown -R \"$OWNER\" \"$ASSETS\"" EXIT
    command -v mksquashfs >/dev/null || { apt-get update -qq && apt-get install -y -qq --no-install-recommends squashfs-tools; }
    packaging/appimage/build-appimage.sh "$TAG" --assets-dir "$ASSETS"
  '
