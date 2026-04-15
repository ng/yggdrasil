#!/bin/bash
# Claude Code status bar script for Ygg orchestrator.
# Reads Claude session JSON from stdin, merges with Ygg agent state.
input=$(cat)
SID=$(echo "$input" | jq -r '.session_id // empty')
PCT=$(echo "$input" | jq -r '.context_window.used_percentage // 0' | cut -d. -f1)
COST=$(echo "$input" | jq -r '.cost.total_cost_usd // 0')

if [ -n "$SID" ] && [ -f "/tmp/ygg/agent-$SID.json" ]; then
    YGG=$(cat "/tmp/ygg/agent-$SID.json" 2>/dev/null)
    STATE=$(echo "$YGG" | jq -r '.state // "idle"')
    LOCKS=$(echo "$YGG" | jq -r '.locks // "none"')
    TASK=$(echo "$YGG" | jq -r '.task // ""')
    printf '\033[36m▊\033[0m %s │ ██ %s%% │ locks: %s │ $%s' "$STATE" "$PCT" "$LOCKS" "$COST"
    if [ -n "$TASK" ]; then
        printf ' │ %s' "$TASK"
    fi
    echo
else
    printf '\033[36m▊\033[0m idle │ ██ %s%% │ $%s\n' "$PCT" "$COST"
fi
