# ADR 0020 — Native subagents for in-session parallel work

**Status:** accepted
**Date:** 2026-11-01
**Narrows:** ADR 0007 (tmux as substrate). tmux stays the substrate for detached agents; it is no longer the default for parallel work inside a session.
**Relates to:** ADR 0016 (autonomous execution — the scheduler still dispatches through `ygg spawn`).

## Context

Until now every piece of guidance Yggdrasil emits (`ygg prime`, the managed
CLAUDE.md/AGENTS.md block, the README, this repo's own instructions) told
agents to prefer `ygg spawn` over Claude Code's native Agent tool for
parallel work. That made sense when the native tool could not isolate a
working copy or run in the background.

It can now. The Agent tool runs subagents in the background, isolates them in
their own git worktree (`isolation: "worktree"`), and reports the result back
to the parent. Meanwhile `ygg spawn` (a tmux window with `claude` typed in via
`send-keys`) failed twice in real use on the same day:

1. Each spawn creates a fresh worktree under `.ygg/worktrees/<name>`, and every
   spawned session stalled on Claude Code's workspace-trust dialog. The agents
   sat idle at 0 tokens for two hours with nothing reporting the stall.
2. A task containing an apostrophe broke the shell quoting of the typed
   command line, so `claude` never started.

The scheduler (`src/scheduler.rs`, `dispatch_ready`) cannot use native
subagents: it runs with no parent session to host them. Long-lived agents that
must outlive the session that started them have the same constraint.

## Decision

- **In-session parallel work uses the native Agent tool** (background,
  `isolation: "worktree"` when it edits files). This is the default the
  guidance now teaches.
- **`ygg spawn` is for detached work only**: agents that must outlive the
  current session, and runs the scheduler dispatches. It stays, and is made
  reliable for those cases (task passed through a file rather than typed into a
  shell, trust-dialog detection).
- **Yggdrasil tracks native subagents** through Claude Code's `SubagentStart`
  and `SubagentStop` hooks. Each subagent is a row in `subagents`, a child of
  the parent agent, visible in `ygg status` and the TUI dashboard with its type,
  description, state and token totals. Subagents are not rows in `agents`:
  they are ephemeral (one per Agent tool call) and would swamp agent lists,
  reaping and stats.

### Alternatives considered

- **Keep `ygg spawn` as the default and fix it.** Fixing quoting and trust is
  worthwhile (and done) for the detached case, but a tmux window per parallel
  task is heavier than a background subagent, needs tmux, and cannot hand its
  result back into the parent's context. The native tool does all of that.
- **Drop `ygg spawn` entirely.** The scheduler has no parent session, so it
  needs a detached launcher. Removing it would remove autonomous execution.
- **Register each subagent as an `agents` row.** Reuses `ygg status`, locks and
  `agent_stats` for free, but subagent ids are per-call and short-lived; the
  agent table would accumulate hundreds of one-shot identities, and the
  watcher's reap logic assumes agents map to sessions or tmux windows.

## Consequences

- Subagent tool calls are attributed to the **parent** agent: locks taken by a
  subagent's Edit/Write belong to the parent, so sibling subagents do not lock
  each other out. A subagent in its own worktree edits different absolute
  paths, so its locks do not collide with the main checkout either.
- Claude Code fires the parent's `Stop` hook while background subagents are
  still running. The Stop hook therefore keeps the parent's locks and session
  open while it has live subagents (running, and seen within the last 30
  minutes).
- `SubagentStart` carries no description. The description comes from the
  parent's `PreToolUse` on the Agent tool and is matched to the first
  unclaimed call of the same `subagent_type` in that session. Parallel calls of
  the same type can swap descriptions.
- ADR 0007 still holds for what it decided: detached agents are tmux windows.
  Its premise that parallel agents in general need tmux no longer does.
