#!/bin/bash
# Claude Code PreToolUse hook: auto-acquire locks before file edits.
# Install in .claude/settings.json under hooks.PreToolUse
#
# When Claude Code is about to Edit/Write a file, this hook calls
# ygg lock acquire for the file path. If the lock is held by another
# agent, it blocks the tool use.

# Read the hook input from stdin
INPUT=$(cat)
TOOL=$(echo "$INPUT" | jq -r '.tool_name // empty')
FILE=$(echo "$INPUT" | jq -r '.tool_input.file_path // .tool_input.path // empty')

# Only act on file-modifying tools
case "$TOOL" in
    Edit|Write|NotebookEdit)
        if [ -z "$FILE" ]; then
            exit 0
        fi

        # Get agent name from env (set by ygg spawn)
        AGENT="${YGG_AGENT_NAME:-unknown}"

        # Try to acquire lock
        RESULT=$(ygg lock acquire "$FILE" --agent "$AGENT" 2>&1)
        EXIT_CODE=$?

        if [ $EXIT_CODE -ne 0 ]; then
            # Lock held by another agent — block the tool use
            echo "BLOCK: $RESULT"
            echo "Another agent holds the lock on $FILE. Wait or use: ygg lock list"
            exit 2
        fi
        ;;
esac

exit 0
