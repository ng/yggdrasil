#!/bin/bash
# Claude Code UserPromptSubmit hook — deliver agent-to-agent messages
# Installed by: ygg init

INPUT=$(cat)
AGENT="${YGG_AGENT_NAME:-$(basename "$(pwd)")}"

# Extract CC session id so every downstream event gets tagged with it.
SID=$(echo "$INPUT" | jq -r '.session_id // empty' 2>/dev/null)
[ -n "$SID" ] && export CLAUDE_SESSION_ID="$SID"

# Inject any unread agent-to-agent messages (ygg msg) and advance the cursor
# so the same batch doesn't resurface. Silent on empty inbox or error.
MSGS=$(ygg msg inbox --agent "$AGENT" 2>/dev/null) || true
if [ -n "$MSGS" ] && [ "$MSGS" != "inbox empty" ]; then
    echo "$MSGS"
    ygg msg mark-read --agent "$AGENT" >/dev/null 2>&1 || true
fi
