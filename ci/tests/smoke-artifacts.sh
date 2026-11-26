#!/bin/sh
# Negative tests for ci/tasks/smoke-artifacts. Run from the repo root:
# sh ci/tests/smoke-artifacts.sh
set -eu
ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)
task=$ROOT/ci/tasks/smoke-artifacts
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
fails=0

expect_fail() {
    name=$1
    shift
    if "$@" >"$work/out" 2>&1; then
        printf 'FAIL %s: expected failure\n' "$name" >&2
        fails=$((fails + 1))
    else
        printf 'ok   %s\n' "$name"
    fi
}

expect_fail missing-binary sh "$task" "$work/does-not-exist"

# A binary that stays up but never serves /api/health must fail, boundedly.
printf '#!/bin/sh\nexec sleep 30\n' >"$work/silent"
chmod +x "$work/silent"
RUSTNZB_SMOKE_ATTEMPTS=2 expect_fail never-healthy sh "$task" "$work/silent"

# A binary that exits at once must fail without waiting out the window.
printf '#!/bin/sh\nexit 3\n' >"$work/dies"
chmod +x "$work/dies"
expect_fail exits-early sh "$task" "$work/dies"

[ "$fails" -eq 0 ] || exit 1
printf 'smoke-artifacts tests passed\n'
