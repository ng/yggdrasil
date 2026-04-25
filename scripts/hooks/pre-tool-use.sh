#!/bin/bash
# Claude Code PreToolUse hook — record tool use for dashboard + auto-acquire locks

INPUT=$(cat)
TOOL=$(echo "$INPUT" | jq -r '.tool_name // empty')
FILE=$(echo "$INPUT" | jq -r '.tool_input.file_path // .tool_input.path // empty')
SID=$(echo "$INPUT" | jq -r '.session_id // empty' 2>/dev/null)
[ -n "$SID" ] && export CLAUDE_SESSION_ID="$SID"
AGENT="${YGG_AGENT_NAME:-$(basename "$(pwd)")}"

# Record tool use for every tool (not just edits) so the dashboard State column
# reflects "tool: Bash" instead of forever reading "idle".
if [ -n "$TOOL" ]; then
    ygg agent-tool "$TOOL" --agent "$AGENT" >/dev/null 2>&1 || true
fi

# ADR 0016 / yggdrasil-99: bump heartbeat_at on this agent's running task_run
# so the scheduler doesn't reap us as crashed. No-op when the agent has no
# bound run (manual sessions, primary interactive shell, etc).
ygg run heartbeat --agent "$AGENT" >/dev/null 2>&1 || true

# Only lock on file-modifying tools
case "$TOOL" in
    Edit|Write|NotebookEdit)
        [ -z "$FILE" ] && exit 0

        RESULT=$(ygg lock acquire "$FILE" --agent "$AGENT" 2>&1) || true

        # If lock is held by another agent, warn but don't block
        if echo "$RESULT" | grep -q "locked by"; then
            echo "ygg: $RESULT" >&2
        fi
        ;;
esac

exit 0
