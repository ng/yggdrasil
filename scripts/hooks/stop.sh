#!/bin/bash
# Claude Code Stop hook — digest session transcript, extract corrections + summary
# Installed by: ygg init

INPUT=$(cat)
AGENT="${YGG_AGENT_NAME:-$(basename "$(pwd)")}"
TRANSCRIPT=$(echo "$INPUT" | jq -r '.transcript_path // empty' 2>/dev/null)

if [ -n "$TRANSCRIPT" ] && [ -f "$TRANSCRIPT" ]; then
    ygg digest --agent "$AGENT" --transcript "$TRANSCRIPT" 2>/dev/null
fi

exit 0
