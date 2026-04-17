#!/bin/bash
# Claude Code PreToolUse hook — record tool use for dashboard + auto-acquire locks

INPUT=$(cat)
TOOL=$(echo "$INPUT" | jq -r '.tool_name // empty')
FILE=$(echo "$INPUT" | jq -r '.tool_input.file_path // .tool_input.path // empty')
AGENT="${YGG_AGENT_NAME:-$(basename "$(pwd)")}"

# Record tool use for every tool (not just edits) so the dashboard State column
# reflects "tool: Bash" instead of forever reading "idle".
if [ -n "$TOOL" ]; then
    ygg agent-tool "$TOOL" --agent "$AGENT" >/dev/null 2>&1 || true
fi

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
