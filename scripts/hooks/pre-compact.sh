#!/bin/bash
# Claude Code PreCompact hook — re-inject agent context before compaction
# Installed by: ygg init

AGENT_NAME="${YGG_AGENT_NAME:-$(basename "$(pwd)")}"

# Output agent context so it survives compaction
ygg prime --agent "$AGENT_NAME" 2>/dev/null
