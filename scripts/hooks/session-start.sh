#!/bin/bash
# Claude Code SessionStart hook — inject agent context
# Installed by: ygg init

AGENT_NAME="${YGG_AGENT_NAME:-$(basename "$(pwd)")}"
SESSION_ID="${CLAUDE_SESSION_ID:-$$}"

# Write session mapping so other hooks can find it
mkdir -p /tmp/ygg
echo "$AGENT_NAME" > /tmp/ygg/session-$SESSION_ID.agent

# Output agent context as markdown — Claude Code injects hook stdout as system context
ygg prime --agent "$AGENT_NAME" 2>/dev/null
