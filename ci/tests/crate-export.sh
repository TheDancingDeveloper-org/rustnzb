#!/bin/sh
# Tests for ci/crate-export.sh's manifest rewrite. POSIX sh; run from the
# monorepo root: sh ci/tests/crate-export.sh
set -eu

lib=$(pwd)/ci/crate-export.sh
. "$lib"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
cd "$work"
fails=0

cat >Cargo.toml <<'TOML'
[workspace]
members = []

[workspace.dependencies]
tokio = { version = "1", features = ["full"] }
utoipa = { version = "5", features = ["axum_extras", "chrono"] }
rust-par2 = { version = "0.1.3" }
md-5 = "0.11"
serde = { version = "1", default-features = false }
mock-nntp-server = { path = "crates/mock-nntp-server" }
TOML

# expect <name> <input> <expected output>
expect() {
    printf '%s\n' "$2" >m.toml
    if ! crate_export_manifest m.toml nzb-x 2>err.txt; then
        printf 'FAIL %s: rewrite failed: %s\n' "$1" "$(cat err.txt)"; fails=$((fails + 1)); return
    fi
    if [ "$(cat m.toml)" != "$3" ]; then
        printf 'FAIL %s:\n  got:  %s\n  want: %s\n' "$1" "$(cat m.toml)" "$3"; fails=$((fails + 1)); return
    fi
    printf 'ok   %s\n' "$1"
}

# refuse <name> <input> <stderr fragment>
refuse() {
    printf '%s\n' "$2" >m.toml
    if crate_export_manifest m.toml nzb-x 2>err.txt; then
        printf 'FAIL %s: accepted, wrote: %s\n' "$1" "$(cat m.toml)"; fails=$((fails + 1)); return
    fi
    if ! grep -qF "$3" err.txt; then
        printf 'FAIL %s: wrong error: %s\n' "$1" "$(cat err.txt)"; fails=$((fails + 1)); return
    fi
    printf 'ok   %s\n' "$1"
}

expect dotted 'tokio.workspace = true' 'tokio = { version = "1", features = ["full"] }'
expect inline-bare 'utoipa = { workspace = true }' 'utoipa = { version = "5", features = ["axum_extras", "chrono"] }'
expect inline-bare-string 'md-5 = { workspace = true }' 'md-5 = "0.11"'
expect inline-features-merge 'utoipa = { workspace = true, features = ["uuid", "chrono"] }' \
    'utoipa = { version = "5", features = ["axum_extras", "chrono", "uuid"] }'
expect inline-features-new 'rust-par2 = { workspace = true, features = ["simd"] }' \
    'rust-par2 = { version = "0.1.3", features = ["simd"] }'
expect inline-string-features 'md-5 = { workspace = true, features = ["asm"], optional = true }' \
    'md-5 = { version = "0.11", features = ["asm"], optional = true }'
expect inline-optional 'tokio = { optional = true, workspace = true }' \
    'tokio = { version = "1", features = ["full"], optional = true }'
expect inline-override 'serde = { workspace = true, default-features = true }' \
    'serde = { version = "1", default-features = true }'
expect self-nested 'nzb-x = { workspace = true }' 'nzb-x = { path = ".." }'
expect plain 'anyhow = "1"' 'anyhow = "1"'
refuse inline-path 'mock-nntp-server = { workspace = true }' 'the workspace entry is a path dependency'
refuse inline-missing 'nope = { workspace = true, optional = true }' '[workspace.dependencies] has no nope'
refuse other-inheritance 'version.workspace = true' 'has no version'
refuse inheritance-multiline 'utoipa = { workspace = true,' 'unsupported workspace inheritance'

[ "$fails" -eq 0 ] || { printf '%s failure(s)\n' "$fails"; exit 1; }
