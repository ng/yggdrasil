# Agent Instructions

This project **is** Yggdrasil — a multi-agent coordination layer. We dogfood it. Refer to the project as Yggdrasil; `ygg` is the CLI binary. See `CLAUDE.md` for the full agent-coordination rules; this file mirrors the essentials for non-Claude agents.

## Yggdrasil Coordination Quick Reference

```bash
ygg task ready                                 # Unblocked tasks in this repo
ygg task list --status open,in_progress        # Comma-separated status filter
ygg task create "title"                        # New task
ygg task claim <ref>                           # Take a task
ygg task close <ref>                           # Complete a task

ygg status                                     # See all agents' state, locks, recent activity
ygg lock acquire <resource-key>                # Lease a shared resource before editing
ygg lock release <resource-key>                # Release when done
ygg spawn --task "..."                         # Spawn a parallel agent in a new tmux window
ygg logs --follow [--kind K] [--session SID]   # Live event stream (filterable)
ygg rollup --days 7                            # Per-repo activity summary
ygg reap --dry-run                             # Preview stale-row cleanup
```

### Rules

- Acquire a lock before editing a resource another agent might touch. Release when done.
- Prefer `ygg spawn` over a native Task/Agent tool for parallel work that warrants its own context.
- Check `ygg status` before assuming you're working alone.
- Use `ygg task` for cross-session work tracking. Intra-turn checklists can stay in a native TodoList.
- Do NOT use `bd` / beads. This project has migrated to Yggdrasil.

## Agent naming

By default the agent name is the cwd basename (`yggdrasil`, `kb-chunking`).
Override with `YGG_AGENT_NAME` to namespace personas or branches:

```bash
YGG_AGENT_NAME="yggdrasil:reviewer"   # role split
YGG_AGENT_NAME="yggdrasil@feat-x"     # branch-scoped window
```

Retire an agent that's no longer in use without losing its history:

```bash
ygg agent list [--all]                         # --all includes archived
ygg agent stale --older-than-days 14           # preview what reap would archive
ygg agent archive <name>                       # hide from live views, keep history
ygg agent unarchive <name>                     # restore
ygg reap --agents --older-than-days 14 [--dry-run]   # bulk, cron-safe
```

## Session Completion

Work is not complete until `git push` succeeds. Release held locks, run quality gates, rebase, push, verify `git status` shows up-to-date.

## Non-Interactive Shell Commands

Use non-interactive flags to avoid hanging on confirmation prompts:

```bash
cp -f src dst     mv -f src dst     rm -f file     rm -rf dir     cp -rf src dst
# scp / ssh: -o BatchMode=yes         apt-get: -y         brew: HOMEBREW_NO_AUTO_UPDATE=1
```

<!-- BEGIN YGG INTEGRATION v:4 hash:3fa7ef6e -->
<!-- markdownlint-disable MD024 -->
## Yggdrasil Coordination

This project uses **Yggdrasil** (`ygg`) for resource coordination and issue
tracking across parallel agents. Hooks fire at Claude Code lifecycle events;
you do not invoke them manually.

### Quick Reference

```bash
ygg task ready                              # Unblocked tasks in this repo
ygg task list [--all] [--status <...>]      # All tasks in this repo (or everywhere)
ygg task create "title"                     # New task
ygg task claim <ref>                        # Take a task
ygg task close <ref>                        # Complete a task
ygg task dep <task> <blocker>               # Record dependency
ygg task dupes [--all]                      # Probable duplicate pairs (string similarity)

ygg status                                  # Agents + outstanding locks
ygg lock acquire <key> / release <key> / list
ygg spawn --task "..."                      # Parallel agent in a new tmux window
ygg interrupt take-over --agent <name>      # Take over another agent
ygg logs --follow                           # Live event stream
```

### Rules

- Acquire a lock before editing a resource another agent might touch. Release when done.
- Prefer `ygg spawn` over a native Task/Agent tool for parallel work.
- Check `ygg status` before assuming you're working alone.
- Use `ygg task` for cross-session work tracking.
- Before `ygg task create`, run `ygg task dupes` to surface near-dups.
- For recurring engineering corrections, `ygg learn add` with a file glob.
- Do NOT use `bd` / beads.

### Ticket body structure

Tickets are read by other agents picking up the work. Bodies have four
sections in this order, separated by blank lines: **Why** (one sentence,
trigger or observation), **What** (one sentence, imperative change),
**Acceptance:** (bulleted testable conditions, no vague verbs — pin
SHAs, paths, commands, numeric thresholds), **Refs:** (optional —
related ticket, ADR, URL).

Be terse in `ygg task create` titles/descriptions/acceptance/notes and
`ygg learn` rules. Drop filler and articles when meaning survives.
Preserve identifiers, paths, commands, numbers, URLs, and modal
keywords (always/never/must/should/cannot/don't) verbatim. Does NOT
apply to commit messages, PR descriptions, or chat — those stay
human-prose.

## Session Completion

Work is not complete until `git push` succeeds. Release held locks, run quality gates, rebase, push, verify `git status` shows up-to-date.

## Non-Interactive Shell Commands

Some systems alias `cp`/`mv`/`rm` to interactive mode which hangs agents. Use:

```bash
cp -f src dst     mv -f src dst     rm -f file     rm -rf dir     cp -rf src dst
# scp / ssh: -o BatchMode=yes         apt-get: -y         brew: HOMEBREW_NO_AUTO_UPDATE=1
```
<!-- markdownlint-enable MD024 -->
<!-- END YGG INTEGRATION -->
