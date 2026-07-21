#!/usr/bin/env bash
set -euo pipefail

forbidden="$(printf '%s%s%s' 'sp' 'roo' 'ty')"
failures=0
fail() { printf 'migration policy: %s\n' "$1" >&2; failures=$((failures + 1)); }

if git grep -Iqi -- "$forbidden" HEAD --; then
  git grep -Ini -- "$forbidden" HEAD -- >&2 || true
  fail 'forbidden legacy identifier found in tracked content'
fi

workflow_matches="$(git grep -nE 'runs-on:[[:space:]]*(ubuntu|windows|macos)-|runs-on:[[:space:]]*.*(ubuntu|windows|macos|arm).*latest|runs-on:[[:space:]]*\$\{\{' HEAD -- '.github/workflows/*.yml' '.github/workflows/*.yaml' || true)"
if [[ -n "$workflow_matches" ]]; then
  printf '%s\n' "$workflow_matches" >&2
  fail 'workflow selects a hosted or unresolved dynamic runner'
fi

if [[ -n "${GITHUB_BASE_REF:-}" ]]; then
  git fetch --no-tags --depth=1 origin "${GITHUB_BASE_REF}" >/dev/null 2>&1 || true
  base="origin/${GITHUB_BASE_REF}"
  commits="$(git rev-list --reverse "$base..HEAD" 2>/dev/null || git rev-list --reverse HEAD~1..HEAD 2>/dev/null || git rev-list --reverse HEAD)"
elif [[ "${GITHUB_EVENT_NAME:-}" == push && "${GITHUB_BEFORE:-}" != 0000000000000000000000000000000000000000 ]]; then
  commits="$(git rev-list --reverse "${GITHUB_BEFORE:-}..HEAD" 2>/dev/null || git rev-list --reverse HEAD~1..HEAD 2>/dev/null || git rev-list --reverse HEAD)"
else
  commits="$(git rev-list --reverse HEAD~1..HEAD 2>/dev/null || git rev-list --reverse HEAD)"
fi
while IFS= read -r commit; do
  [[ -n "$commit" ]] || continue
  metadata="$(git show -s --format='%an%n%ae%n%cn%n%ce%n%B' "$commit")"
  if grep -qi -- "$forbidden" <<<"$metadata"; then
    printf '%s\n' "$commit" >&2
    fail 'forbidden legacy identifier found in commit metadata'
  fi
done <<<"$commits"

(( failures == 0 )) || exit 1
printf 'migration policy passed\n'
