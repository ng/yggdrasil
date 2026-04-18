# ADR 0014 — First-class scoped memories

**Status:** accepted
**Date:** 2026-04-18

## Context

Durable notes ("remember that migrations must be backwards-compatible") previously lived as `nodes` rows with `kind=directive`. The similarity retriever surfaced them alongside transcript content. That worked but conflated three different lifecycles:

- **Durable notes** are pinned, edited, expired, scoped to a project or to nothing.
- **Transcript nodes** are write-once and belong to a session's DAG.
- **Digests** are summaries of prior sessions.

Mixing them on one table meant no lifecycle affordances (no pin, no expire, no list-by-scope) and made "show me only my notes" impossible without fishing through every `kind=directive` row.

## Decision

New `memories` table with explicit scope:

```sql
CREATE TYPE memory_scope AS ENUM ('global', 'repo', 'session');
CREATE TABLE memories (
    memory_id       UUID PRIMARY KEY,
    scope           memory_scope NOT NULL,
    repo_id         UUID REFERENCES repos(repo_id),         -- required if scope='repo'
    cc_session_id   TEXT,                                    -- required if scope='session'
    agent_id        UUID,
    text            TEXT NOT NULL,
    embedding       vector(384),                             -- HNSW-indexed
    pinned          BOOLEAN NOT NULL DEFAULT false,
    expires_at      TIMESTAMPTZ,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    ...
);
```

Scope visibility rolls up: global memories surface everywhere; repo memories only when retrieving in their repo; session memories only within their CC session. Pinned memories surface first in both `list` and `search`.

CLI: `ygg memory <create|list|search|pin|unpin|expire|delete> [--scope S]`. Repo scope auto-resolves from cwd; session scope reads `CLAUDE_SESSION_ID`. Inject surfaces top-K matching memories as `[ygg memory | ★ scope | sim=N%]` lines, independent of node retrieval.

`ygg remember` is kept as a back-compat alias that writes to `nodes` as `kind=directive`. New notes should use `ygg memory create`.

## Consequences

- Notes now have real lifecycle: pin/unpin, expire-in, delete.
- Retrieval ranks memories independently of transcript nodes; pinned + narrow-scope memories surface even when node retrieval wouldn't have ranked them.
- Memgraph pane unions both sources — pinned memories appear next to digest nodes in neighbor lookups when they're close in embedding space.
- Separate reaper: `ygg reap --memories` purges expired rows.

## Follow-up

- Task-relevance bump (yggdrasil-38): task metadata fields to influence retrieval ranking the way memories do.
