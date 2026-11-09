#!/bin/sh
# Tests for ci/check-version.sh's exact tag gate. POSIX sh; run from the
# monorepo root: sh ci/tests/check-version.sh
set -eu

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
mkdir -p "$work/ci" "$work/desktop/src-tauri"
cp "$ROOT/ci/check-version.sh" "$work/ci/check-version.sh"

version=1.7.1-beta.1
fails=0

cat >"$work/Cargo.toml" <<EOF
[workspace.package]
version = "$version"
edition = "2024"
EOF

# The gate requires every version field to carry the workspace version; these
# fixtures keep those checks green so each case isolates the tag comparison.
printf '{\n  "version": "%s",\n}\n' "$version" >"$work/desktop/package.json"
printf '{\n  "version": "%s",\n}\n' "$version" >"$work/desktop/package-lock.json"
printf '[package]\nname = "rustnzb-desktop"\nversion = "%s"\n' "$version" \
    >"$work/desktop/src-tauri/Cargo.toml"
printf '[[package]]\nname = "rustnzb-desktop"\nversion = "%s"\n' "$version" \
    >"$work/desktop/src-tauri/Cargo.lock"

# run [TAG]: run the gate against the fixture, recording status and stderr.
run() {
    status=0
    (cd "$work" && sh ci/check-version.sh "$@") >/dev/null 2>"$work/stderr" || status=$?
}

expect_pass() {
    name=$1
    shift
    run "$@"
    if [ "$status" -eq 0 ]; then
        printf 'ok   %s\n' "$name"
    else
        printf 'FAIL %s: expected pass, got exit %s\n' "$name" "$status" >&2
        sed 's/^/     /' "$work/stderr" >&2
        fails=$((fails + 1))
    fi
}

expect_reject() {
    name=$1
    tag=$2
    run "$tag"
    if [ "$status" -eq 1 ] && grep -q 'does not match workspace version' "$work/stderr"; then
        printf 'ok   %s\n' "$name"
    else
        printf 'FAIL %s: expected exit 1 with a version mismatch, got exit %s\n' \
            "$name" "$status" >&2
        sed 's/^/     /' "$work/stderr" >&2
        fails=$((fails + 1))
    fi
}

# The tag must equal v<workspace version> exactly, pre-release suffix included,
# so a beta tree cannot accept the next beta, the stable tag, an older version
# or a different next version.
expect_pass 'exact pre-release tag' v1.7.1-beta.1
expect_reject 'next pre-release tag' v1.7.1-beta.2
expect_reject 'stable tag from a beta tree' v1.7.1
expect_reject 'older tag' v1.7.0
expect_reject 'next version with a pre-release tag' v1.7.2-beta.1

# No tag: the field checks still run and pass; the tag branch is skipped.
expect_pass 'no tag'

# The real tree must also be internally consistent in no-tag mode.
if (cd "$ROOT" && sh ci/check-version.sh) >/dev/null 2>"$work/stderr"; then
    printf 'ok   real tree, no tag\n'
else
    printf 'FAIL real tree, no tag\n' >&2
    sed 's/^/     /' "$work/stderr" >&2
    fails=$((fails + 1))
fi

if [ "$fails" -ne 0 ]; then
    printf 'check-version tests: %s failed\n' "$fails" >&2
    exit 1
fi
printf 'check-version tests: all passed\n'
