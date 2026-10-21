#!/bin/sh
# Build rustnzb AppImages from the release's Linux tarballs.
#
# Usage:
#   packaging/appimage/build-appimage.sh TAG --assets-dir DIR [--out DIR]
#
# Reads rustnzb-TAG-linux-{x86_64,aarch64}.tar.gz from --assets-dir and writes
# rustnzb-TAG-{x86_64,aarch64}.AppImage to --out (default: --assets-dir). The
# binary inside is the one already shipped in the tarball; nothing is rebuilt.
#
# An AppImage is the type2 runtime followed by a squashfs of the AppDir, so
# this needs only mksquashfs (squashfs-tools) and curl. The runtimes are pinned
# to an AppImage/type2-runtime release by SHA-256; bump version and both hashes
# together. The AppImages are not GPG-signed (no code signing, by decision).
set -eu

RUNTIME_VERSION=20251108
RUNTIME_SHA256_x86_64=2fca8b443c92510f1483a883f60061ad09b46b978b2631c807cd873a47ec260d
RUNTIME_SHA256_aarch64=00cbdfcf917cc6c0ff6d3347d59e0ca1f7f45a6df1a428a0d6d8a78664d87444

usage() { sed -n '2,6p' "$0" >&2; exit 2; }

tag=${1:-}
[ -n "$tag" ] || usage
shift
case "$tag" in v[0-9]*) ;; *) echo "build-appimage: bad tag: $tag" >&2; exit 2 ;; esac
assets=
out=
while [ $# -gt 0 ]; do
    case "$1" in
        --assets-dir) assets=${2:?}; shift 2 ;;
        --out) out=${2:?}; shift 2 ;;
        *) usage ;;
    esac
done
[ -n "$assets" ] || usage
out=${out:-$assets}
command -v mksquashfs >/dev/null || { echo "build-appimage: mksquashfs not found (install squashfs-tools)" >&2; exit 1; }

repo_root=$(cd "$(dirname "$0")/../.." && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
mkdir -p "$out"

for arch in x86_64 aarch64; do
    tarball="$assets/rustnzb-$tag-linux-$arch.tar.gz"
    test -f "$tarball" || { echo "build-appimage: missing $tarball" >&2; exit 1; }

    case "$arch" in
        x86_64) want=$RUNTIME_SHA256_x86_64 ;;
        aarch64) want=$RUNTIME_SHA256_aarch64 ;;
    esac
    runtime="$work/runtime-$arch"
    curl -fsSL -o "$runtime" \
        "https://github.com/AppImage/type2-runtime/releases/download/$RUNTIME_VERSION/runtime-$arch"
    printf '%s  %s\n' "$want" "$runtime" | sha256sum -c - >/dev/null \
        || { echo "build-appimage: runtime-$arch checksum mismatch" >&2; exit 1; }

    appdir="$work/AppDir-$arch"
    mkdir -p "$appdir/usr/bin" "$appdir/usr/share/rustnzb"
    tar -xzf "$tarball" -C "$work"
    cp "$work/rustnzb-$tag/rustnzb" "$appdir/usr/bin/rustnzb"
    cp "$work/rustnzb-$tag/config.example.toml" "$appdir/usr/share/rustnzb/"
    rm -rf "$work/rustnzb-$tag"
    cp "$repo_root/desktop/src-tauri/icons/128x128.png" "$appdir/rustnzb.png"
    ln -s rustnzb.png "$appdir/.DirIcon"
    cat >"$appdir/rustnzb.desktop" <<DESKTOP
[Desktop Entry]
Type=Application
Name=rustnzb
Comment=Usenet NZB downloader with a web UI on http://localhost:9090
Exec=rustnzb
Icon=rustnzb
Terminal=true
Categories=Network;FileTransfer;
DESKTOP
    cat >"$appdir/AppRun" <<'APPRUN'
#!/bin/sh
here=$(dirname "$(readlink -f "$0")")
exec "$here/usr/bin/rustnzb" "$@"
APPRUN
    chmod 755 "$appdir/AppRun" "$appdir/usr/bin/rustnzb"

    image="$out/rustnzb-$tag-$arch.AppImage"
    mksquashfs "$appdir" "$work/squashfs-$arch" -root-owned -noappend -comp zstd -quiet >/dev/null
    cat "$runtime" "$work/squashfs-$arch" >"$image"
    chmod 755 "$image"
    printf 'appimage: %s\n' "$image"
done
