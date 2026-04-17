# 0010 — Tasks: beads replacement inside Yggdrasil

- **Status**: Accepted
- **Date**: 2026-04-17
- **Depends on**: [ADR 0009](0009-repos-and-sessions-first-class.md) (repos must exist first)
- **Relates to**: [ADR 0006](0006-dogfood-drop-beads.md) (why we dropped beads in this repo)

## Context

[ADR 0006](0006-dogfood-drop-beads.md) dropped `bd` / beads from this repo on the reasoning that the dogfooding value of making Yggdrasil's gaps hurt outweighed the feature loss. Over the next weeks the gap that hurt most was **durable cross-session task tracking**: beads gave us `bd create → bd ready → bd claim → bd close`, dependency graphs, and a place for acceptance criteria / design / notes. Native `TaskCreate` covers the current conversation; nothing else did.

Two paths forward:

1. Put `bd` back.
2. Build a minimal task system inside Yggdrasil that uses the same DB.

We went with (2). Yggdrasil already has the hard parts — one global Postgres, per-repo identity (ADR 0009), agents as first-class rows — so tasks slot in as a small schema extension rather than a parallel system. One source of truth, one query surface, and the same retrieval/inject layer sees task content as it would any other node.

## Decision

A small tasks subsystem inside Yggdrasil, scoped to repos per ADR 0009, with a deliberate subset of beads' feature surface:

```
tasks
  task_id      UUID PK
  repo_id      UUID FK repos
  seq          INT                   -- per-repo sequence
  title, description, acceptance, design, notes   -- TEXT
  kind         ENUM(task, bug, feature, chore, epic)
  status       ENUM(open, in_progress, blocked, closed)
  priority     SMALLINT (0..4)       -- 0 = critical
  created_by   UUID FK agents
  assignee     UUID FK agents NULL
  human_flag   BOOL
  created_at, updated_at, closed_at, close_reason
  UNIQUE (repo_id, seq)

task_deps       -- task depends on blocker
  task_id      FK
  blocker_id   FK
  PK (task_id, blocker_id)
  CHECK (task_id <> blocker_id)

task_labels     -- many-to-many over (task_id, label)
task_events     -- audit trail: kind + payload + agent_id
task_seq        -- per-repo next-seq counter
```

Task identifiers are `<repo.task_prefix>-<seq>` (e.g. `yggdrasil-42`), matching the beads ergonomic. Sequences are allocated atomically inside a transaction.

### What we shipped in v1

- `ygg task create` with `--kind/--priority/--acceptance/--design/--notes/--label`.
- `ygg task list [--all] [--status <...>]`.
- `ygg task ready` — open/in-progress tasks in the current repo with no unsatisfied blockers.
- `ygg task blocked` — the complement.
- `ygg task show <ref>` — full view with description, acceptance, design, notes, labels, deps.
- `ygg task claim <ref>` — assign to agent, set in-progress, write event.
- `ygg task close <ref> [--reason …]`.
- `ygg task status <ref> <status> [--reason …]` — general-purpose transition.
- `ygg task update <ref> --title/--description/--priority/…`.
- `ygg task dep <task> <blocker>` / `ygg task undep` — with cycle detection via recursive CTE.
- `ygg task label <ref> <label>`.
- `ygg task stats [--all]`.
- `ygg remember "..."` — durable directive node, repo-scoped when possible; `--list` to browse.
- `ygg prime` surfaces up to five ready tasks for the current repo.

### What we deliberately skipped in v1

- **Sync / branches.** Beads uses Dolt for git-style branch/merge of issue state. Yggdrasil is single-user local today; when we federate, Postgres logical replication is the obvious path.
- **Formulas** (workflow templates). Nice, not essential.
- **`defer --until`, `supersede`, `orphans`, `stale`, `preflight`, `lint`, `doctor`.** Pure reporters; schema supports them; command surface deferred until we feel the lack.
- **`bd human`** as a command surface. The `human_flag` column exists; the command doesn't yet.
- **Actor overrides** (`--actor` / `BEADS_ACTOR`). Every event carries the agent_id we can identify; impersonation isn't worth the footgun in v1.

### Alternatives considered

- **Keep beads alongside.** Would work. Two databases, two mental models, two sync stories. Against the "one source of truth" grain that made Yggdrasil worth building.
- **Subsume beads by embedding its Dolt DB.** Too much surface area for a reimplementation; Dolt's merge semantics are lovely but orthogonal to our current needs.
- **Task fields as JSONB on a generic "work_items" table.** Flexible, unscoped, and turns every query into a JSONB path expression. Fine for a prototype; bad for something we want retrieval to reason about.

## Consequences

**Positive**

- One schema, one connection pool, one operational surface.
- Tasks are reachable from the same similarity layer as memory. An open task and a prior conversation about it show up in the same `[ygg memory | …]` block if relevant.
- Cycle-safe dependency graph via a recursive CTE; no cron job needed to detect broken invariants.
- Prime surfaces `ready` tasks in-context. Agents see work-to-do without being asked.

**Negative**

- We own issue tracking now. Bugs, schema evolution, reporting — all ours.
- v1 feature set is narrower than beads. Anything in the "skipped" list that turns out to matter is work to add.
- Mirroring beads' `--validate` / `--acceptance` workflow will require the `ygg task lint` command that doesn't yet exist.
- The audit trail (`task_events`) will grow unboundedly; we'll need a retention policy before it matters.

**Future triggers to revisit**

- If a team ever uses this, sync is the first ADR we'll need.
- If label-space fragmentation emerges, a `labels` registry with descriptions becomes worthwhile.
- If a task ever needs to live in two repos simultaneously (shared monorepo tool), `tasks.repo_id` becomes `task_repos` m:n. Not today.

## References

- [beads](https://github.com/steveyegge/beads) — the feature surface and ergonomics we're cloning selectively.
- [ADR 0006](0006-dogfood-drop-beads.md) — the decision that made this necessary.
- [ADR 0009](0009-repos-and-sessions-first-class.md) — the repo dimension that made this possible.
