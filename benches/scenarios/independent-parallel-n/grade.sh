#!/usr/bin/env bash
# Deterministic grader for Scenario 1. Run from a worktree containing the
# scenario's expected end-state. Exit 0 on pass, non-zero on fail. JSON
# detail goes to stderr for partial reporting.
#
# Pass criteria:
# - All four expected markdown files exist
# - Each contains its required H1 + "## Strategies" headings
# - The git log contains commits with the expected messages

set -euo pipefail

cd "$(dirname "$0")/../.." 2>/dev/null || true
WORKDIR="${1:-$PWD}"
cd "$WORKDIR"

declare -a EXPECTED=(
    "docs/topics/api-retry.md|API retry|docs: add api-retry topic page"
    "docs/topics/db-config.md|Database configuration|docs: add db-config topic page"
    "docs/topics/graphql-errors.md|GraphQL errors|docs: add graphql-errors topic page"
    "docs/topics/test-patterns.md|Test patterns|docs: add test-patterns topic page"
)

failures=()
for entry in "${EXPECTED[@]}"; do
    IFS='|' read -r path heading commit_msg <<< "$entry"
    if [ ! -f "$path" ]; then
        failures+=("missing: $path")
        continue
    fi
    if ! grep -qE "^# $heading\$" "$path"; then
        failures+=("$path: missing H1 \"$heading\"")
    fi
    if ! grep -qE '^## Strategies' "$path"; then
        failures+=("$path: missing \"## Strategies\" section")
    fi
    if [ -d .git ]; then
        # Pull the log into a variable once per check; some shells/repos
        # interact poorly with subshells in the original `cmd | grep` form.
        log_out=$(git log --all --pretty=%B 2>/dev/null || true)
        if ! printf '%s' "$log_out" | grep -qF "$commit_msg"; then
            failures+=("git log missing commit: \"$commit_msg\"")
        fi
    fi
done

if [ ${#failures[@]} -eq 0 ]; then
    echo "{\"passed\":true,\"checks\":${#EXPECTED[@]}}" >&2
    exit 0
fi

printf '{"passed":false,"failures":[' >&2
for i in "${!failures[@]}"; do
    [ "$i" -gt 0 ] && printf ',' >&2
    printf '"%s"' "${failures[$i]//\"/\\\"}" >&2
done
printf ']}\n' >&2
exit 1
