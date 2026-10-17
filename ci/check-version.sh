#!/bin/sh
# Single-source version gate.
#
# The workspace version in the root Cargo.toml ([workspace.package]) is the
# only place a version number is typed. Every other version field must equal
# it, and an optional release tag must be that version with an optional
# pre-release suffix (v1.6.0 or v1.6.0-beta.1 both match 1.6.0).
#
# usage: ci/check-version.sh [TAG]
set -eu

TASK_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
REPO_ROOT=$(CDPATH= cd -- "$TASK_DIR/.." && pwd)
cd "$REPO_ROOT"

workspace=$(sed -n '/^\[workspace\.package\]/,/^\[/{s/^version = "\(.*\)"/\1/p;}' Cargo.toml | head -n1)
test -n "$workspace" || { echo "check-version: no [workspace.package] version in Cargo.toml" >&2; exit 1; }

failed=0
expect() {
    # expect FILE LABEL ACTUAL
    if [ "$3" != "$workspace" ]; then
        printf 'check-version: %s is %s, workspace version is %s\n' "$2" "$3" "$workspace" >&2
        failed=1
    fi
}

# First top-level "version" key in a JSON file.
json_version() {
    sed -n 's/^  "version": "\(.*\)",\{0,1\}$/\1/p' "$1" | head -n1
}

# version = "..." in the [package] table of a Cargo manifest.
cargo_package_version() {
    sed -n '/^\[package\]/,/^\[/{s/^version = "\(.*\)"/\1/p;}' "$1" | head -n1
}

# version of a named package in a Cargo.lock.
lock_version() {
    awk -v name="$2" '
        /^\[\[package\]\]/ { found = 0 }
        $0 == "name = \"" name "\"" { found = 1 }
        found && /^version = / { gsub(/"/, "", $3); print $3; exit }
    ' "$1"
}

expect desktop/package.json desktop/package.json "$(json_version desktop/package.json)"
expect desktop/package-lock.json desktop/package-lock.json "$(json_version desktop/package-lock.json)"
expect desktop/src-tauri/Cargo.toml desktop/src-tauri/Cargo.toml "$(cargo_package_version desktop/src-tauri/Cargo.toml)"
expect desktop/src-tauri/Cargo.lock "desktop/src-tauri/Cargo.lock rustnzb-desktop" "$(lock_version desktop/src-tauri/Cargo.lock rustnzb-desktop)"

if [ "$#" -ge 1 ] && [ -n "$1" ]; then
    tag=$1
    base=${tag%%-*}
    if [ "$base" != "v$workspace" ]; then
        printf 'check-version: tag %s does not match workspace version %s\n' "$tag" "$workspace" >&2
        failed=1
    fi
fi

if [ "$failed" -ne 0 ]; then
    echo "check-version: bump the workspace version in Cargo.toml, or the desktop files, to match" >&2
    exit 1
fi

printf 'check-version: all version fields match %s\n' "$workspace"
