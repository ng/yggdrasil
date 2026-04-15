#!/bin/bash
# Claude Code PreToolUse hook — auto-acquire locks before file edits

INPUT=$(cat)
TOOL=$(echo "$INPUT" | jq -r '.tool_name // empty')
FILE=$(echo "$INPUT" | jq -r '.tool_input.file_path // .tool_input.path // empty')

# Only lock on file-modifying tools
case "$TOOL" in
    Edit|Write|NotebookEdit)
        [ -z "$FILE" ] && exit 0

        AGENT="${YGG_AGENT_NAME:-$(basename $(pwd))}"

        # Try to acquire lock (non-blocking — fail silently if ygg not running)
        RESULT=$(ygg lock acquire "$FILE" --agent "$AGENT" 2>&1) || true

        # If lock is held by another agent, warn but don't block
        if echo "$RESULT" | grep -q "locked by"; then
            echo "ygg: $RESULT" >&2
        fi
        ;;
esac

exit 0
