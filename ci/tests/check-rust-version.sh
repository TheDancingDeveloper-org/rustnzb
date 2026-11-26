#!/bin/sh
# Tests for ci/check-rust-version.sh's published-member floor gate. POSIX sh;
# run from the monorepo root: sh ci/tests/check-rust-version.sh
#
# The fixtures are dependency-free workspaces so `cargo metadata --offline`
# needs no registry, matching how the gate itself runs.
set -eu

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
mkdir -p "$work/ci"
cp "$ROOT/ci/check-rust-version.sh" "$work/ci/check-rust-version.sh"

fails=0

# write_member <name> <manifest body after name/version/edition>
write_member() {
    mkdir -p "$work/crates/$1/src"
    printf 'pub fn f() {}\n' >"$work/crates/$1/src/lib.rs"
    printf '[package]\nname = "%s"\nversion = "0.1.0"\nedition = "2024"\n%s' \
        "$1" "$2" >"$work/crates/$1/Cargo.toml"
}

# reset <members...> — a fresh workspace with the given member directories,
# each inheriting the floor.
reset() {
    rm -rf "$work/crates" "$work/Cargo.toml"
    printf '[workspace]\nresolver = "3"\nmembers = [' >"$work/Cargo.toml"
    sep=
    for m in "$@"; do
        printf '%s"crates/%s"' "$sep" "$m" >>"$work/Cargo.toml"
        sep=', '
    done
    printf ']\n\n[workspace.package]\nedition = "2024"\nrust-version = "1.100"\n' \
        >>"$work/Cargo.toml"
    for m in "$@"; do
        write_member "$m" 'rust-version.workspace = true
'
    done
}

# run — the gate against the current fixture: status in $status, stderr in
# $work/stderr.
run() {
    status=0
    (cd "$work" && sh ci/check-rust-version.sh) >/dev/null 2>"$work/stderr" || status=$?
}

expect_pass() {
    name=$1
    run
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
    run
    if [ "$status" -eq 1 ] && grep -q 'not at the workspace floor' "$work/stderr"; then
        printf 'ok   %s\n' "$name"
    else
        printf 'FAIL %s: expected exit 1 with a floor violation, got exit %s\n' \
            "$name" "$status" >&2
        sed 's/^/     /' "$work/stderr" >&2
        fails=$((fails + 1))
    fi
}

# Every member inheriting the floor passes.
reset one two
expect_pass 'all members inherit the floor'

# A trailing TOML comment on the root rust-version line is not part of the floor.
reset one two
sed 's/^rust-version = "1.100"$/rust-version = "1.100" # the floor/' "$work/Cargo.toml" >"$work/Cargo.toml.new"
mv "$work/Cargo.toml.new" "$work/Cargo.toml"
grep -q '1.100" # the floor' "$work/Cargo.toml"
expect_pass 'root rust-version with a trailing comment'

# A published member with no rust-version at all is the failure the gate exists
# for: it would silently drop Clippy's MSRV-gated lints for that crate.
reset one two
write_member two 'description = "no floor"
'
expect_reject 'published member omits rust-version'

# A published member pinned to a different floor is a second copy that drifts.
reset one two
write_member two 'rust-version = "1.99"
'
expect_reject 'published member declares a different floor'

# An unpublished crate makes no promise to a consumer and is exempt.
reset one two
write_member two 'publish = false
'
expect_pass 'unpublished member is exempt'

# The real tree must also pass.
if (cd "$ROOT" && sh ci/check-rust-version.sh) >/dev/null 2>"$work/stderr"; then
    printf 'ok   real tree\n'
else
    printf 'FAIL real tree\n' >&2
    sed 's/^/     /' "$work/stderr" >&2
    fails=$((fails + 1))
fi

if [ "$fails" -ne 0 ]; then
    printf 'check-rust-version tests: %s failed\n' "$fails" >&2
    exit 1
fi
printf 'check-rust-version tests: all passed\n'
