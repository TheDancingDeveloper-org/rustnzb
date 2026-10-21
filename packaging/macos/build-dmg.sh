#!/bin/sh
# Wrap a macOS rustnzb binary in an app bundle and an (unsigned) dmg.
#
# Usage:
#   packaging/macos/build-dmg.sh TAG --binary PATH --arch arm64|x86_64 [--out DIR] [--stage-only]
#
# Writes rustnzb-TAG-macos-ARCH.dmg to --out (default .ci-output/packages).
# --stage-only builds rustnzb.app in --out and skips hdiutil, so the bundle
# layout can be checked on a host that is not macOS.
#
# The bundle and dmg are NOT code-signed or notarised (operator decision).
# Gatekeeper blocks the first launch; docs/RELEASING.md and the README say how
# users open it.
set -eu

usage() { sed -n '2,9p' "$0" >&2; exit 2; }

tag=${1:-}
[ -n "$tag" ] || usage
shift
case "$tag" in v[0-9]*) ;; *) echo "build-dmg: bad tag: $tag" >&2; exit 2 ;; esac
binary=
arch=
out=.ci-output/packages
stage_only=false
while [ $# -gt 0 ]; do
    case "$1" in
        --binary) binary=${2:?}; shift 2 ;;
        --arch) arch=${2:?}; shift 2 ;;
        --out) out=${2:?}; shift 2 ;;
        --stage-only) stage_only=true; shift ;;
        *) usage ;;
    esac
done
case "$arch" in arm64|x86_64) ;; *) usage ;; esac
test -f "$binary" || { echo "build-dmg: missing binary $binary" >&2; exit 1; }
version=${tag#v}
repo_root=$(cd "$(dirname "$0")/../.." && pwd)

stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
app="$stage/rustnzb.app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp "$binary" "$app/Contents/MacOS/rustnzb"
chmod 755 "$app/Contents/MacOS/rustnzb"
cp "$repo_root/apps/rustnzb/config.example.toml" "$app/Contents/Resources/"
cp "$repo_root/desktop/src-tauri/icons/icon.icns" "$app/Contents/Resources/rustnzb.icns"

# The launcher starts the daemon (logging to ~/Library/Logs) and opens the web UI.
cat >"$app/Contents/MacOS/rustnzb-launcher" <<'LAUNCHER'
#!/bin/sh
here=$(cd "$(dirname "$0")" && pwd)
logs="$HOME/Library/Logs/rustnzb"
mkdir -p "$logs"
if ! pgrep -qx rustnzb; then
    "$here/rustnzb" >>"$logs/rustnzb.log" 2>&1 &
    sleep 2
fi
open "http://localhost:9090"
LAUNCHER
chmod 755 "$app/Contents/MacOS/rustnzb-launcher"

cat >"$app/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>rustnzb</string>
  <key>CFBundleDisplayName</key><string>rustnzb</string>
  <key>CFBundleIdentifier</key><string>dev.rustnzb.rustnzb</string>
  <key>CFBundleVersion</key><string>$version</string>
  <key>CFBundleShortVersionString</key><string>$version</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleExecutable</key><string>rustnzb-launcher</string>
  <key>CFBundleIconFile</key><string>rustnzb.icns</string>
  <key>LSMinimumSystemVersion</key><string>11.0</string>
  <key>LSUIElement</key><true/>
</dict>
</plist>
PLIST

mkdir -p "$out"
if [ "$stage_only" = true ]; then
    rm -rf "$out/rustnzb.app"
    cp -R "$app" "$out/"
    printf 'dmg: staged %s/rustnzb.app (hdiutil skipped)\n' "$out"
    exit 0
fi
command -v hdiutil >/dev/null || { echo "build-dmg: hdiutil not found (run on macOS or use --stage-only)" >&2; exit 1; }
ln -s /Applications "$stage/Applications"
dmg="$out/rustnzb-$tag-macos-$arch.dmg"
rm -f "$dmg"
hdiutil create -volname "rustnzb $tag" -srcfolder "$stage" -format UDZO -ov "$dmg" >/dev/null
printf 'dmg: %s\n' "$dmg"
