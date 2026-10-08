# ADR 0019 — Managed Postgres and OKF knowledge storage

**Status:** proposed; implementation pending
**Date:** 2026-10-08
**Relates to:** [ADR 0008](0008-shared-db-across-repos.md),
[ADR 0014](0014-scoped-memories.md),
[ADR 0015](0015-retrieval-scope-reduction.md), and
[ADR 0017](0017-learning-capture-and-approval.md).

## Context

Yggdrasil requires users to provision Postgres even for local use. Its scheduler,
watcher, and resource leases depend on PostgreSQL transactions, atomic claims,
and advisory locks. Supporting SQLite alongside Postgres would require maintaining
different SQL and concurrency implementations to remove that installation burden.

Durable notes and learnings also live in Postgres. They should be portable,
inspectable documents while retaining scope, deterministic retrieval, and approval.
Google's [Open Knowledge Format](https://github.com/GoogleCloudPlatform/knowledge-catalog/blob/main/okf/SPEC.md)
provides a Markdown and YAML representation without requiring Google infrastructure.

## Decision

### One coordination engine with two deployment modes

Use **managed local Postgres** by default for new installations, with optional
external Postgres for existing local servers or hosted deployments. Yggdrasil owns
installation and persistent process lifecycle in managed mode. Postgres runs as a
separate server process, not inside the CLI. A short-lived command exiting must
not stop the shared database.

Both modes use the same PostgreSQL schema, SQLx models, and coordination semantics.
Introduce a lifecycle/connection boundary rather than a database-neutral query
layer. Preserve a shared coordination database across repos and worktrees. Existing
external configuration remains external; connection failure must never silently
create or switch to an empty local database.

Hosted connections must preserve sessions for advisory locks. Database hosting
does not itself add multi-host execution or multi-tenant isolation; the current
scheduler and watcher still operate on one execution host's tmux and worktrees.

### OKF is authoritative for durable knowledge

Replace SQL-backed notes and learnings with OKF documents, initially targeting
v0.2. A knowledge interface owns document operations independently of database
availability. Postgres continues to own tasks, runs, leases, events, sessions,
workers, and session handoffs. Usage telemetry may remain in Postgres; lookup
indexes are disposable and must be rebuildable from documents.

Preserve stable knowledge IDs, repo/global scope, creator metadata, file/rule
matching, and pending/active decisions. Use namespaced extension metadata for
Yggdrasil-specific semantics. Worktrees share a corpus binding based on stable
repo identity, not directory basename. Keep deterministic retrieval and the
existing approval policy; imported verification metadata alone cannot activate
a rule. Material changes to approved content or scope require renewed approval.

Knowledge location is independent of database location. Default to a private local
corpus; shared delivery is explicitly configured and must define conflicts,
freshness, and trust. Selecting hosted Postgres must not publish private knowledge.
No Google service, embeddings, or automatic knowledge generation is required.

Replace the source of truth through a verified, reversible cutover. Preserve IDs,
content, scope, and approval state; prevent old clients from continuing SQL writes.
An export alongside active SQL tables is not completion. Backups and rollback must
include current documents and their identity/policy configuration, including edits
made after cutover.

## Alternatives rejected

- **SQLite plus Postgres:** adds a second SQL dialect and coordination implementation.
- **Dolt:** database branching and merging do not justify replacing our existing
  coordination engine for this requirement.
- **Require externally installed Postgres everywhere:** retains local setup friction.
- **Keep knowledge in SQL with OKF exports:** leaves portability dependent on the
  database and creates competing representations.
- **Move coordination into document files:** gives up the transactional substrate
  required by task claims and resource leases.

## Consequences

Local setup becomes application-managed while hosted deployments retain the same
database features. Yggdrasil takes responsibility for binary distribution, process
ownership, upgrades, recovery, and backups. Managed Postgres still consumes server
resources; it is not an in-process embedded engine.

Knowledge becomes readable and transferable independently of Postgres. File writes,
approval invalidation, shared-corpus consistency, and migration compatibility need
explicit guarantees. Database backups alone no longer restore all Yggdrasil state.

This changes the deployment assumption in ADR 0008 and the knowledge persistence
described in ADR 0014, while preserving ADR 0015's retrieval limits and ADR 0017's
approval gate. Detailed implementation plans remain local and untracked. Crate
selection, packaging, commands, transport protocol, rollout sequencing, and tests
are implementation work; this ADR records the architectural decision, not a claim
that the feature is built or installed.
