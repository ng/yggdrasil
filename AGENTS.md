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

<!-- BEGIN YGG INTEGRATION v:1 hash:863bd071 -->
## Yggdrasil Coordination

This project uses **Yggdrasil** (`ygg`) for cross-session memory and
coordination. Hooks fire at Claude Code lifecycle events; you do not invoke
them manually. Above each user prompt you will see `[ygg memory | ... ]` lines —
those are real prior context surfaced by similarity.

### Quick Reference

```bash
ygg task ready                              # Unblocked tasks in this repo
ygg task list [--all] [--status <...>]      # All tasks in this repo (or everywhere)
ygg task create "title"                     # New task
ygg task claim <ref>                        # Take a task
ygg task close <ref>                        # Complete a task
ygg task dep <task> <blocker>               # Record dependency
ygg remember "..."                          # Durable note; retriever can surface later

ygg status                                  # Agents + outstanding locks
ygg lock acquire <key> / release <key> / list
ygg spawn --task "..."                      # Parallel agent in a new tmux window
ygg interrupt take-over --agent <name>      # Take over another agent
ygg logs --follow                           # Live event stream
```

### Rules

- Acquire a lock before editing a resource another agent might touch. Release when done.
- Prefer `ygg spawn` over a native Task/Agent tool for parallel work.
- Read `[ygg memory | ...]` hints — real prior context.
- Check `ygg status` before assuming you're working alone.
- Use `ygg task` for cross-session work tracking; `ygg remember` for durable notes.
- Do NOT use `bd` / beads.

## Session Completion

Work is not complete until `git push` succeeds. Release held locks, run quality gates, rebase, push, verify `git status` shows up-to-date.

## Non-Interactive Shell Commands

Some systems alias `cp`/`mv`/`rm` to interactive mode which hangs agents. Use:

```bash
cp -f src dst     mv -f src dst     rm -f file     rm -rf dir     cp -rf src dst
# scp / ssh: -o BatchMode=yes         apt-get: -y         brew: HOMEBREW_NO_AUTO_UPDATE=1
```
<!-- END YGG INTEGRATION -->
