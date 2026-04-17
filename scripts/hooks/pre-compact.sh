#!/bin/bash
# Claude Code PreCompact hook — digest what's about to be lost, then re-inject context
# Installed by: ygg init

INPUT=$(cat)
AGENT="${YGG_AGENT_NAME:-$(basename "$(pwd)")}"
SID=$(echo "$INPUT" | jq -r '.session_id // empty' 2>/dev/null)
[ -n "$SID" ] && export CLAUDE_SESSION_ID="$SID"
TRANSCRIPT=$(echo "$INPUT" | jq -r '.transcript_path // empty' 2>/dev/null)

# 1. Write a digest of the conversation about to be compacted — captures corrections,
#    decisions, and reinforcements before they're lost
if [ -n "$TRANSCRIPT" ] && [ -f "$TRANSCRIPT" ]; then
    ygg digest --agent "$AGENT" --transcript "$TRANSCRIPT" 2>/dev/null
fi

# 2. Re-inject agent context so the compacted session starts with full state
ygg prime --agent "$AGENT" 2>/dev/null
