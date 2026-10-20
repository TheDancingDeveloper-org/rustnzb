#!/bin/sh
# Refresh packaging/homebrew/Formula/rustnzb.rb for a published release tag.
#
# usage: packaging/homebrew/update-formula.sh v1.5.0
#
# Downloads both Linux release tarballs and the release's own SHA256SUMS into a
# temporary directory, checks each tarball against that file, and only then
# writes the version, URLs and two sha256 lines into the formula. The tarballs
# are never extracted here; their checksums are all that is read.
set -eu

tag=${1:?usage: packaging/homebrew/update-formula.sh TAG (e.g. v1.5.0)}
case "$tag" in
    v[0-9]*.[0-9]*.[0-9]*) ;;
    *) echo "update-formula: tag must look like vX.Y.Z or vX.Y.Z-suffix" >&2; exit 2 ;;
esac

ver=${tag#v}
repo=https://github.com/TheDancingDeveloper-org/rustnzb/releases/download/$tag
amd64=rustnzb-$tag-linux-x86_64.tar.gz
arm64=rustnzb-$tag-linux-aarch64.tar.gz

script_dir=$(CDPATH="" cd -- "$(dirname -- "$0")" && pwd)
formula=$script_dir/Formula/rustnzb.rb
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

curl -fsSL -o "$work/$amd64" "$repo/$amd64"
curl -fsSL -o "$work/$arm64" "$repo/$arm64"
curl -fsSL -o "$work/SHA256SUMS.txt" "$repo/SHA256SUMS-$tag.txt"

verify() {
    file=$1
    expected=$(awk -v f="$file" '$2 == f { print $1 }' "$work/SHA256SUMS.txt")
    test -n "$expected" || {
        echo "update-formula: $file is not listed in the release SHA256SUMS" >&2
        exit 1
    }
    actual=$(sha256sum "$work/$file" | awk '{ print $1 }')
    if [ "$expected" != "$actual" ]; then
        echo "update-formula: checksum mismatch for $file" >&2
        exit 1
    fi
    printf '%s\n' "$actual"
}

x86_hash=$(verify "$amd64")
arm_hash=$(verify "$arm64")

sed -i \
    -e "s|^  version \".*\"$|  version \"$ver\"|" \
    -e "s|^  url \"https://github.com/TheDancingDeveloper-org/rustnzb/releases/download/.*/rustnzb-.*-linux-x86_64.tar.gz\"$|  url \"$repo/$amd64\"|" \
    -e "s|releases/download/.*/rustnzb-.*-linux-aarch64.tar.gz|releases/download/$tag/$arm64|" \
    -e "0,/^  sha256 \"[0-9a-f]*\"$/s||  sha256 \"$x86_hash\"|" \
    -e "0,/^    sha256 \"[0-9a-f]*\"$/s||    sha256 \"$arm_hash\"|" \
    "$formula"

printf 'update-formula: version=%s x86_64=%s aarch64=%s\n' "$ver" "$x86_hash" "$arm_hash"
