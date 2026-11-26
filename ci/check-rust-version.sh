#!/bin/sh
# Rust-version floor gate.
#
# The workspace compiler floor is the only place the number is typed, in the
# root Cargo.toml under [workspace.package]. Every published workspace member
# must carry exactly that floor: members inherit it with
# `rust-version.workspace = true`, and a member that repeats a literal must
# repeat the same one. A crate the workspace does not publish (publish = false)
# makes no promise to a consumer and is exempt.
#
# The floor is read from the manifest, never hard-coded a second time. A
# published member that omits rust-version, or names a different one, fails the
# gate — and this is the check that keeps a missing floor from silently
# disabling Clippy's MSRV-gated lints for that crate.
#
# Reads only the workspace manifests, so it runs offline.
#
# usage: ci/check-rust-version.sh
set -eu

TASK_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
REPO_ROOT=$(CDPATH= cd -- "$TASK_DIR/.." && pwd)
cd "$REPO_ROOT"

# rust-version = "..." in the [workspace.package] table of the root manifest.
floor=$(sed -n '/^\[workspace\.package\]/,/^\[/{s/^rust-version *= *"\(.*\)"/\1/p;}' Cargo.toml | head -n1)
test -n "$floor" || { echo "check-rust-version: no [workspace.package] rust-version in Cargo.toml" >&2; exit 1; }

command -v cargo >/dev/null 2>&1 || { echo "check-rust-version: cargo is required" >&2; exit 127; }
command -v jq >/dev/null 2>&1 || { echo "check-rust-version: jq is required" >&2; exit 127; }

metadata=$(cargo metadata --no-deps --format-version 1 --offline) \
    || { echo "check-rust-version: cargo metadata failed" >&2; exit 1; }

# A package is published unless it sets `publish = false`, which cargo metadata
# reports as an empty array. `null` (the default) and a non-empty allowlist are
# both published. Every published member must name the floor exactly.
offenders=$(printf '%s\n' "$metadata" | jq -r --arg floor "$floor" '
    .packages[]
    | select((.publish | type) != "array" or (.publish | length) > 0)
    | select(.rust_version != $floor)
    | "\(.name) declares \(.rust_version // "no rust-version")"')

if [ -n "$offenders" ]; then
    printf 'check-rust-version: published member(s) not at the workspace floor %s:\n' "$floor" >&2
    printf '%s\n' "$offenders" | sed 's/^/  /' >&2
    printf 'check-rust-version: add rust-version.workspace = true to the member, or set it to %s\n' "$floor" >&2
    exit 1
fi

printf 'check-rust-version: every published workspace member declares the floor %s\n' "$floor"
