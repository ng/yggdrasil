#!/bin/bash
# Claude Code Stop hook — capture task-run outcome + enforce stop-check
# Installed by: ygg init
#
# Note: the authoritative Stop handler is `ygg hook stop` (installed into
# settings.json by `ygg init`); this script is a manual/compat fallback.

INPUT=$(cat)
AGENT="${YGG_AGENT_NAME:-$(basename "$(pwd)")}"
SID=$(echo "$INPUT" | jq -r '.session_id // empty' 2>/dev/null)
[ -n "$SID" ] && export CLAUDE_SESSION_ID="$SID"

# ADR 0016 / yggdrasil-97: capture commits + branch into the agent's latest
# running task_run row, then heuristically transition the run terminal so
# manual-mode (no scheduler) still produces useful run history. Idempotent
# and silent on agents without a bound run. Skip with YGG_RUN_CAPTURE=0.
if [ "${YGG_RUN_CAPTURE:-1}" != "0" ]; then
    ygg run capture-outcome --agent "$AGENT" 2>/dev/null
fi

# Spawned-worker enforcement: blocks session end if the claimed task is still
# in_progress, the worktree has uncommitted changes, or commits are unpushed.
# Silent on the primary interactive session. `YGG_STOP_CHECK=0` disables.
ygg stop-check --agent "$AGENT" 2>/dev/null

exit 0
