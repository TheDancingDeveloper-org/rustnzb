#!/bin/sh
# Refresh packaging/aur/PKGBUILD for a published release tag.
#
# usage: packaging/aur/update-pkgbuild.sh v1.6.0-beta.3
#
# Downloads the linux-x86_64 tarball and the release's own SHA256SUMS into a
# fresh temporary directory, checks the tarball against that file, and only
# then writes pkgver and sha256sums into the PKGBUILD. The tarball is never
# extracted here; the checksum is the only thing read from it.
set -eu

tag=${1:?usage: packaging/aur/update-pkgbuild.sh TAG (e.g. v1.6.0-beta.3)}
case "$tag" in
    v[0-9]*.[0-9]*.[0-9]*) ;;
    *) echo "update-pkgbuild: tag must look like vX.Y.Z or vX.Y.Z-suffix" >&2; exit 2 ;;
esac

ver=${tag#v}
pkgver=$(printf '%s' "$ver" | sed 's/-/_/')
repo=https://github.com/TheDancingDeveloper-org/rustnzb/releases/download/$tag
asset=rustnzb-$tag-linux-x86_64.tar.gz

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

curl -fsSL -o "$work/$asset" "$repo/$asset"
curl -fsSL -o "$work/SHA256SUMS.txt" "$repo/SHA256SUMS-$tag.txt"

expected=$(awk -v f="$asset" '$2 == f { print $1 }' "$work/SHA256SUMS.txt")
test -n "$expected" || { echo "update-pkgbuild: $asset is not listed in the release SHA256SUMS" >&2; exit 1; }
actual=$(sha256sum "$work/$asset" | awk '{ print $1 }')
if [ "$expected" != "$actual" ]; then
    echo "update-pkgbuild: checksum mismatch for $asset" >&2
    exit 1
fi

pkgbuild="$script_dir/PKGBUILD"
sed -i \
    -e "s/^pkgver=.*/pkgver=$pkgver/" \
    -e "s/^pkgrel=.*/pkgrel=1/" \
    -e "s/^sha256sums=.*/sha256sums=('$actual')/" \
    "$pkgbuild"
printf 'update-pkgbuild: pkgver=%s sha256=%s\n' "$pkgver" "$actual"
