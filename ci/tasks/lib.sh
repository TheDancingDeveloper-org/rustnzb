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
    wait_for_sccache
    return 0
}

# The shared Redis backend can still be loading its dataset when a runner
# starts.  sccache fails during server startup in that window, before Cargo
# has a chance to run.  Give Redis a bounded amount of time to become usable;
# cache availability must never determine whether validation runs.
wait_for_sccache() {
    [ -n "${RUSTC_WRAPPER:-}" ] || return 0
    command -v sccache >/dev/null 2>&1 || return 0

    attempts=${SCCACHE_REDIS_READY_ATTEMPTS:-15}
    delay=${SCCACHE_REDIS_READY_DELAY_SECS:-2}
    case "$attempts" in
        ''|*[!0-9]*|0) attempts=15 ;;
    esac
    case "$delay" in
        ''|*[!0-9]*) delay=2 ;;
    esac

    attempt=1
    while [ "$attempt" -le "$attempts" ]; do
        # --show-stats can succeed without starting the cache server, even
        # while Redis is returning BusyLoadingError.  Start the server itself
        # so this probe exercises the exact path Cargo/rustc will use.
        if sccache --start-server >/dev/null 2>&1; then
            return 0
        fi
        if [ "$attempt" -lt "$attempts" ]; then
            printf 'sccache cache backend is not ready (attempt %s/%s); retrying in %ss\n' \
                "$attempt" "$attempts" "$delay" >&2
            sleep "$delay"
        fi
        attempt=$((attempt + 1))
    done

    printf 'sccache cache backend did not become ready; continuing without sccache\n' >&2
    unset RUSTC_WRAPPER
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
