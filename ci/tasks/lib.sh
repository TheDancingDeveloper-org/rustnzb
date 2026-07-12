#!/bin/sh

TASK_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
REPO_ROOT=$(CDPATH= cd -- "$TASK_DIR/../.." && pwd)

task_start() {
    task_name=$1
    shift
    cd "$REPO_ROOT"
    printf 'task=%s\n' "$task_name"
    for tool in "$@"; do
        command -v "$tool" >/dev/null 2>&1 || {
            printf 'required tool is missing from the selected image: %s\n' "$tool" >&2
            exit 127
        }
    done
    command -v rustc >/dev/null 2>&1 && rustc --version
    command -v cargo >/dev/null 2>&1 && cargo --version
    command -v node >/dev/null 2>&1 && node --version
    command -v npm >/dev/null 2>&1 && npm --version
    command -v sccache >/dev/null 2>&1 && sccache --version
    return 0
}

task_target_dir() {
    task_name=$1
    CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-$REPO_ROOT/.ci-output/targets/$task_name}
    export CARGO_TARGET_DIR
    mkdir -p "$CARGO_TARGET_DIR"
}

prepare_placeholder_frontend() {
    rm -rf "$REPO_ROOT/apps/rustnzb/frontend/dist"
    placeholder_dir=$REPO_ROOT/apps/rustnzb/frontend/dist/frontend/browser
    mkdir -p "$placeholder_dir"
    printf '%s\n' '<!DOCTYPE html><html><body><h1>rustnzb</h1></body></html>' \
        > "$placeholder_dir/index.html"
    export RUSTNZB_SKIP_FRONTEND_BUILD=1
}

build_frontend() {
    frontend_dir=$REPO_ROOT/apps/rustnzb/frontend
    rm -rf "$frontend_dir/dist" "$frontend_dir/.angular"
    npm --prefix "$frontend_dir" ci --no-audit --no-fund
    npm --prefix "$frontend_dir" run build -- --configuration=production
    test -s "$frontend_dir/dist/frontend/browser/index.html"
}

frontend_ready_marker() {
    [ -n "${CI_PIPELINE_NUMBER:-}" ] || return 1
    printf '%s/.ci-output/frontend-ready-%s\n' "$REPO_ROOT" "$CI_PIPELINE_NUMBER"
}

publish_frontend() {
    build_frontend
    marker=$(frontend_ready_marker) || return 0
    mkdir -p "$REPO_ROOT/.ci-output"
    printf '%s\n' "${CI_COMMIT_SHA:-unknown}" > "$marker"
}

ensure_frontend() {
    marker=$(frontend_ready_marker 2>/dev/null || true)
    index=$REPO_ROOT/apps/rustnzb/frontend/dist/frontend/browser/index.html
    if [ -n "$marker" ]; then
        if [ -s "$marker" ] && [ -s "$index" ] \
            && [ "$(cat "$marker")" = "${CI_COMMIT_SHA:-unknown}" ]; then
            FRONTEND_BUILT_BY_TASK=false
            export FRONTEND_BUILT_BY_TASK
            return 0
        fi
        printf 'frontend-build did not publish assets for pipeline %s commit %s\n' \
            "$CI_PIPELINE_NUMBER" "${CI_COMMIT_SHA:-unknown}" >&2
        return 1
    fi

    build_frontend
    FRONTEND_BUILT_BY_TASK=true
    export FRONTEND_BUILT_BY_TASK
}

cleanup_frontend() {
    rm -rf "$REPO_ROOT/apps/rustnzb/frontend/dist" "$REPO_ROOT/apps/rustnzb/frontend/.angular"
}

cleanup_task_frontend() {
    [ "${FRONTEND_BUILT_BY_TASK:-false}" != true ] || cleanup_frontend
}

show_sccache_stats() {
    if command -v sccache >/dev/null 2>&1 && [ -n "${RUSTC_WRAPPER:-}" ]; then
        sccache --show-stats || true
    fi
}
