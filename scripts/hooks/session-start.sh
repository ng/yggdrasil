#!/bin/bash
# Claude Code SessionStart hook — inject agent context
# Installed by: ygg init

INPUT=$(cat)
AGENT_NAME="${YGG_AGENT_NAME:-$(basename "$(pwd)")}"
SESSION_ID="${CLAUDE_SESSION_ID:-$$}"
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
