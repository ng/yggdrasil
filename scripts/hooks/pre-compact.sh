#!/bin/bash
# Claude Code PreCompact hook — re-inject agent context before compaction
# Installed by: ygg init

INPUT=$(cat)
AGENT="${YGG_AGENT_NAME:-$(basename "$(pwd)")}"
SID=$(echo "$INPUT" | jq -r '.session_id // empty' 2>/dev/null)
[ -n "$SID" ] && export CLAUDE_SESSION_ID="$SID"

# Re-inject agent context so the compacted session starts with full state
ygg prime --agent "$AGENT" 2>/dev/null
