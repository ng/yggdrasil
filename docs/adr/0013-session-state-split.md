# ADR 0013 — Split agent identity from CC session state

**Status:** accepted
**Date:** 2026-04-18
**Supersedes part of [0009-repos-and-sessions-first-class.md](0009-repos-and-sessions-first-class.md)**

## Context

Before this change, `agents.current_state / head_node_id / context_tokens / metadata` were keyed on `agent_name`, which the hooks derived from `$(basename $(pwd))`. Two parallel Claude Code sessions in the same repo collapsed onto the same `agents` row and raced on every mutation — last write wins, state readouts were wrong, and `head_node_id` could point at a node that didn't belong to the session that read it.

Yggdrasil already had a `sessions` table from [ADR 0009](0009-repos-and-sessions-first-class.md), but it stored only `(agent_id, repo_id, started_at, ended_at)` — nothing live.

## Decision

Move per-session state to the `sessions` table. `agents` keeps the long-lived identity fields (`agent_name`, `metadata`). Columns added:

- `sessions.cc_session_id TEXT UNIQUE` — Claude Code's own session id. Maps 1:1 onto a sessions row via UPSERT.
- `sessions.current_state agent_state` — session-level state machine.
- `sessions.head_node_id UUID` — session-scoped conversation DAG head.
- `sessions.context_tokens INT` — rolling per-session token estimate.
- `sessions.last_tool TEXT` — last tool invoked (surfaces in dashboard State column).
- `sessions.updated_at TIMESTAMPTZ` — for live/stale detection and the reaper.
- `events.session_id UUID` — FK for session-scoped analytics.

Hooks export `CLAUDE_SESSION_ID`; every ygg subcommand that records state (`inject`, `digest`, `agent-tool`) resolves the current session from that env var, UPSERTs into `sessions`, and calls `SessionRepo::force_state()`. The agents row is still updated in parallel as a convenience for single-session display.

The dashboard surfaces a `×N` badge in the NAME column when an agent has >1 live session, so the old "3 sessions collapsed onto one row" confusion is visible.

## Consequences

- Parallel CC sessions in the same repo stop stomping each other's state.
- Session analytics (per-session token totals, per-session event streams) now possible.
- Session lifecycle needs explicit `end()` — added on the Stop hook path via `ygg digest --stop`, plus `ygg reap` for abandoned sessions.
- Agent state displayed on the dashboard is now "most-recent session's state" rather than a single truth; this is acceptable because the `×N` badge discloses when it's ambiguous.

## Follow-up

- Phase B: migrate `head_node_id` / `context_tokens` off `agents` entirely once all readers move to the session-scoped fields.
- A dedicated Sessions pane for drill-down (file as a follow-up task).
