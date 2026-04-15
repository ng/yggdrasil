#!/bin/bash
# Claude Code UserPromptSubmit hook — inject directives near the cursor
# This is the agent-ways "progressive disclosure" pattern

AGENT="${YGG_AGENT_NAME:-$(basename $(pwd))}"

# Get relevant directives from ygg (similarity search + salience governor)
# ygg inject returns text to prepend to the conversation
DIRECTIVES=$(ygg inject --agent "$AGENT" 2>/dev/null) || true

if [ -n "$DIRECTIVES" ]; then
    echo "$DIRECTIVES"
fi
