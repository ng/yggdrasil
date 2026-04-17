#!/bin/bash
# Claude Code SessionStart hook — inject agent context
# Installed by: ygg init

INPUT=$(cat)
AGENT_NAME="${YGG_AGENT_NAME:-$(basename "$(pwd)")}"
# Prefer session_id from the hook payload; fall back to env, then shell pid.
SID_FROM_INPUT=$(echo "$INPUT" | jq -r '.session_id // empty' 2>/dev/null)
SESSION_ID="${SID_FROM_INPUT:-${CLAUDE_SESSION_ID:-$$}}"
[ -n "$SESSION_ID" ] && export CLAUDE_SESSION_ID="$SESSION_ID"
TRANSCRIPT=$(echo "$INPUT" | jq -r '.transcript_path // empty' 2>/dev/null)

# Write session mapping so other hooks can find it
mkdir -p /tmp/ygg
echo "$AGENT_NAME" > /tmp/ygg/session-$SESSION_ID.agent

# Output agent context as markdown — Claude Code injects hook stdout as system context
if [ -n "$TRANSCRIPT" ]; then
    ygg prime --agent "$AGENT_NAME" --transcript "$TRANSCRIPT" 2>/dev/null
else
    ygg prime --agent "$AGENT_NAME" 2>/dev/null
fi
