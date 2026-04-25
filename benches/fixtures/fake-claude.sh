#!/usr/bin/env bash
# Fake `claude -p` for bench tests. Reads a prompt on stdin, parses out the
# expected file path, heading, body, and commit message from the
# manifest.toml format used by Scenario 1, then writes the file and commits.
#
# This is the test substitute for `claude`. It lets the bench harness run
# end-to-end in CI without burning real API tokens. Real Claude is invoked
# the same way; the fake just simulates a successful agent.
#
# Exit 0 on success. Emits a JSON usage block on stdout so parse_usage()
# fills tokens_in/out/cache + usd in BenchTaskResult.

# Note: deliberately NOT using `set -e` / pipefail. head -1 on a long pipe
# causes SIGPIPE upstream, which pipefail treats as fatal. We check vars
# explicitly instead.

prompt=$(cat)

# Extract path, heading, body, commit. Each is best-effort; we sanity-check
# below. Use awk where it's clearer than grep+sed pipes.
path=$(printf '%s\n' "$prompt" | awk '/Create the file [^ ]+/{
    for (i=1; i<=NF; i++) if ($i == "file") { print $(i+1); exit }
}')
heading=$(printf '%s\n' "$prompt" | awk '/^# /{print substr($0, 3); exit}')
body=$(printf '%s\n' "$prompt" | awk '
    /^```/ { fence = !fence; next }
    fence { print }
')
commit=$(printf '%s\n' "$prompt" | tr '\n' ' ' \
    | awk 'match($0, /Commit with message[ ]+"[^"]+"/) {
        s = substr($0, RSTART, RLENGTH);
        if (match(s, /"[^"]+"/)) print substr(s, RSTART+1, RLENGTH-2);
    }')

if [ -z "$path" ] || [ -z "$body" ] || [ -z "$commit" ]; then
    cat <<JSON
{"error":"could not parse prompt","path":"$path","commit":"$commit"}
JSON
    exit 2
fi

mkdir -p "$(dirname "$path")"
printf '%s\n' "$body" > "$path"

git add "$path" 2>/dev/null
git commit -q -m "$commit" 2>/dev/null

# Emit a usage block consistent with claude -p --output-format json so
# parse_usage extracts something. Made-up but plausible.
cat <<JSON
{
  "type": "result",
  "subtype": "success",
  "is_error": false,
  "result": "wrote $path",
  "usage": {
    "input_tokens": 200,
    "output_tokens": 50,
    "cache_read_input_tokens": 1000
  },
  "total_cost_usd": 0.0009
}
JSON
exit 0
