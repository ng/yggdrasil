#!/bin/bash
# Claude Code Stop hook — digest session transcript, extract corrections + summary
# Installed by: ygg init

INPUT=$(cat)
AGENT="${YGG_AGENT_NAME:-$(basename "$(pwd)")}"
SID=$(echo "$INPUT" | jq -r '.session_id // empty' 2>/dev/null)
[ -n "$SID" ] && export CLAUDE_SESSION_ID="$SID"
TRANSCRIPT=$(echo "$INPUT" | jq -r '.transcript_path // empty' 2>/dev/null)

if [ -n "$TRANSCRIPT" ] && [ -f "$TRANSCRIPT" ]; then
    ygg digest --agent "$AGENT" --transcript "$TRANSCRIPT" --stop 2>/dev/null
fi

# Spawned-worker enforcement: blocks session end if the claimed task is still
# in_progress, the worktree has uncommitted changes, or commits are unpushed.
# Silent on the primary interactive session. `YGG_STOP_CHECK=0` disables.
ygg stop-check --agent "$AGENT" 2>/dev/null

exit 0
