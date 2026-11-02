Spawn a detached Yggdrasil agent in its own tmux window and git worktree. It outlives this session, so use it only for long-lived or detached work. For parallel work inside this session, use the native Agent tool instead (in the background, with `isolation: "worktree"` when it edits files); Yggdrasil tracks those subagents too.

Run `ygg spawn --task "$ARGUMENTS"` to spawn the agent with the given task description. Display the output.
