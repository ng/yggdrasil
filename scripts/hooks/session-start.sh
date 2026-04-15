#!/bin/bash
# Claude Code SessionStart hook — register agent, start observer
# Installed by: ygg init

AGENT_NAME="${YGG_AGENT_NAME:-$(basename $(pwd))}"
SESSION_ID="${CLAUDE_SESSION_ID:-$$}"

# Register agent in the DAG (silent, non-blocking)
ygg run --name "$AGENT_NAME" --task "session started" &>/dev/null &

# Write session mapping so other hooks can find it
mkdir -p /tmp/ygg
echo "$AGENT_NAME" > /tmp/ygg/session-$SESSION_ID.agent
