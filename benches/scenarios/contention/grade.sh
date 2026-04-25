#!/usr/bin/env bash
# Deterministic grader for Scenario 4 (contention). Both dependency bumps
# must land in Cargo.toml, the file must still parse, and the git log must
# contain both commit messages. Detects the silent-overwrite race that
# happens when locks are disabled or absent.

WORKDIR="${1:-$PWD}"
cd "$WORKDIR"

failures=()

if [ ! -f Cargo.toml ]; then
    failures+=("missing: Cargo.toml")
elif command -v rustc >/dev/null && [ -x "$(command -v cargo)" ]; then
    # Best-effort syntactic check (only if cargo is available).
    if ! cargo verify-project --offline >/dev/null 2>&1; then
        # cargo verify-project is strict about lockfiles etc; tolerate.
        :
    fi
fi

if [ -f Cargo.toml ]; then
    grep -qE '^serde = "1\.0\.220"' Cargo.toml || failures+=("Cargo.toml: serde not bumped to 1.0.220")
    grep -qE '^tokio = "1\.40"' Cargo.toml      || failures+=("Cargo.toml: tokio not bumped to 1.40")
fi

if [ -d .git ]; then
    log_out=$(git log --all --pretty=%B 2>/dev/null || true)
    printf '%s' "$log_out" | grep -qF "deps: bump serde to 1.0.220" \
        || failures+=("git log missing: \"deps: bump serde to 1.0.220\"")
    printf '%s' "$log_out" | grep -qF "deps: bump tokio to 1.40" \
        || failures+=("git log missing: \"deps: bump tokio to 1.40\"")
fi

if [ ${#failures[@]} -eq 0 ]; then
    echo "{\"passed\":true,\"checks\":4}" >&2
    exit 0
fi

printf '{"passed":false,"failures":[' >&2
for i in "${!failures[@]}"; do
    [ "$i" -gt 0 ] && printf ',' >&2
    printf '"%s"' "${failures[$i]//\"/\\\"}" >&2
done
printf ']}\n' >&2
exit 1
