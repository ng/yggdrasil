# Agent Instructions

This project **is** Yggdrasil — a multi-agent coordination layer. We dogfood it. Refer to the project as Yggdrasil; `ygg` is the CLI binary. See `CLAUDE.md` for the full agent-coordination rules; this file mirrors the essentials for non-Claude agents.

## Yggdrasil Coordination Quick Reference

```bash
ygg task ready                              # Unblocked tasks in this repo
ygg task create "title"                     # New task
ygg task claim <ref>                        # Take a task
ygg task close <ref>                        # Complete a task
ygg remember "..."                          # Durable note; retriever can surface later

ygg status                                  # See all agents' state, locks, recent activity
ygg lock acquire <resource-key>             # Lease a shared resource before editing
ygg lock release <resource-key>             # Release when done
ygg lock list                               # See outstanding locks
ygg spawn --task "..."                      # Spawn a parallel agent in a new tmux window
ygg interrupt take-over --agent <name>      # Take over / steer another agent
ygg logs --follow                           # Live event stream
```

### Rules

- Acquire a lock before editing a resource another agent might touch. Release when done.
- Prefer `ygg spawn` over a native Task/Agent tool for parallel work that warrants its own context.
- Read `[ygg memory | ...]` hints injected above user prompts — they are real prior context.
- Check `ygg status` before assuming you're working alone.
- Use `ygg task` for cross-session work tracking. Intra-turn checklists can stay in a native TodoList.
- Use `ygg remember "..."` for durable notes (scoped to the current repo).
- Do NOT use `bd` / beads. This project has migrated to Yggdrasil.

## Session Completion

Work is not complete until `git push` succeeds. Release held locks, run quality gates, rebase, push, verify `git status` shows up-to-date.

## Non-Interactive Shell Commands

Use non-interactive flags to avoid hanging on confirmation prompts:

```bash
cp -f src dst     mv -f src dst     rm -f file     rm -rf dir     cp -rf src dst
# scp / ssh: -o BatchMode=yes         apt-get: -y         brew: HOMEBREW_NO_AUTO_UPDATE=1
```
