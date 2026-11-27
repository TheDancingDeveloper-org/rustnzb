#!/bin/sh
# Tests for ci/tasks/verify-image-pins. POSIX sh; run from the monorepo root:
# sh ci/tests/verify-image-pins.sh
set -eu

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
fails=0

# fresh: rebuild the fixture tree from the real files.
fresh() {
    rm -rf "$work/t"
    mkdir -p "$work/t/ci/tasks" "$work/t/.github/workflows"
    cp "$ROOT/ci/tasks/verify-image-pins" "$ROOT/ci/tasks/lib.sh" "$work/t/ci/tasks/"
    cp "$ROOT/ci/images.lock" "$ROOT/ci/run" "$work/t/ci/"
    cp "$ROOT/Dockerfile" "$work/t/Dockerfile"
    cp "$ROOT"/.github/workflows/*.yml "$work/t/.github/workflows/"
}

run() {
    status=0
    sh "$work/t/ci/tasks/verify-image-pins" >/dev/null 2>"$work/stderr" || status=$?
}

expect() {
    name=$1
    want=$2
    run
    if [ "$want" = pass ] && [ "$status" -eq 0 ]; then
        printf 'ok   %s\n' "$name"
    elif [ "$want" = fail ] && [ "$status" -ne 0 ]; then
        printf 'ok   %s\n' "$name"
    else
        printf 'FAIL %s (status %s)\n' "$name" "$status"
        cat "$work/stderr"
        fails=$((fails + 1))
    fi
}

bad=0000000000000000000000000000000000000000000000000000000000000000

fresh
expect 'real state passes' pass

fresh
sed -i 's/^E2E_IMAGE=\(.*\)@sha256:.*/E2E_IMAGE=\1:latest/' "$work/t/ci/images.lock"
expect 'lock entry without digest fails' fail

fresh
sed -i "s/^ARG CROSS_IMAGE=\(.*\)@sha256:.*/ARG CROSS_IMAGE=\1@sha256:$bad/" "$work/t/Dockerfile"
expect 'Dockerfile digest drift fails' fail

fresh
sed -i "s/^\( *CROSS_IMAGE: .*\)@sha256:.*/\1@sha256:$bad/" "$work/t/.github/workflows/promote-release.yml"
expect 'promote-release digest drift fails' fail

fresh
printf 'x:\n  env:\n    CROSS_IMAGE: ${{ vars.RUSTNZB_CROSS_IMAGE }}\n' >"$work/t/.github/workflows/zz.yml"
expect 'RUSTNZB_CROSS_IMAGE variable reference fails' fail

fresh
printf 'x:\n  image: foo/bar:1.2\n' >"$work/t/.github/workflows/zz.yml"
expect 'mutable workflow image fails' fail

[ "$fails" -eq 0 ] || exit 1
printf 'verify-image-pins tests passed\n'
