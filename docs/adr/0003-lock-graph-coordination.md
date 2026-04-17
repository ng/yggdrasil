# 0003 — Explicit resource leases over optimistic coordination

- **Status**: Accepted
- **Date**: 2026-04-15

## Context

When multiple autonomous agents share a working tree, they *will* race on the same file, the same branch, the same migration. The failure modes are not subtle:

- Two agents `Edit`ing the same file produces silent truncation (last write wins) or a merge conflict the agent can't resolve without human help.
- Two agents running `cargo fmt` or `git checkout -b` step on each other's output.
- An agent mid-refactor has no way to tell a sibling agent "this file is mine for the next three minutes."

We need a coordination primitive. The question is how tight it should be.

## Decision

Yggdrasil offers **explicit resource leases** via the `locks` table. Any agent can acquire a named lock on an arbitrary string key (`src/db.rs`, `branch:feature/foo`, `topic:auth-refactor`) with a TTL, heartbeat periodically, and release when done. The lock table is Postgres with `UNIQUE (resource_key)`, so acquisition is an atomic `INSERT` with conflict detection. Expired locks are reaped by the `watcher` daemon.

Locks are **advisory**. They coordinate *agents who choose to cooperate*. Nothing in the filesystem or git enforces them — an agent that ignores `ygg lock acquire` can still edit a locked file. We made this trade deliberately: mandatory enforcement would require wrapping `Edit`/`Write`/`Bash` behind a gate, which breaks Claude Code's tool contract and creates a single point of failure.

### Alternatives considered

- **Optimistic coordination (no locks; detect conflicts after)**. Works fine for humans with git, fails for agents who can't judge whether a merge conflict is semantic or trivial. Produces the exact silent-corruption mode we're trying to avoid.
- **OS-level file locks (`flock`)**. Enforced but coarse — locks files, not topics, and can't express "I own this branch." Also brittle across tmux panes and processes that didn't opt in.
- **Mandatory gateway (wrap every tool call)**. Strongest guarantee, but creates a chokepoint, breaks Claude Code's native tools, and fights the ecosystem instead of composing with it.
- **CRDTs / automatic merge**. Beautiful for some workloads (collaborative editors) but code edits aren't commutative and agents aren't humans who can judge conflicts.

## Consequences

**Positive**

- Simple mental model: acquire, do work, release. Timeouts and heartbeats handle agent crashes.
- Locks work over arbitrary string keys, not just files — agents can lease "the CI pipeline" or "the migration numbering space."
- Postgres `UNIQUE` + transactions give us atomic acquisition for free; no distributed lock manager to run.
- Human override is trivial: `ygg lock release <key>` from the TUI resolves any impasse.

**Negative**

- Advisory locks are only as good as the agents that respect them. Uncooperative agents (or badly prompted ones) can still clobber a locked resource. Mitigation: `CLAUDE.md` rules, `ygg prime` imperative output, and lock-hit telemetry to flag violators.
- TTL tuning is real work. Too short and long edits lose their lock mid-task; too long and crashed agents hold resources for minutes. Default 300s with heartbeat every 30s is our current guess.
- Requires Postgres to be up for acquisition. Degraded mode: agents proceed without locks when the DB is unreachable, logged as a warning.

**Future triggers to revisit**

- If lock violations become common in practice, escalate to a wrapping gateway — but only after measuring.
- If we need distributed locks across multiple hosts (unlikely soon), Postgres advisory locks (`pg_try_advisory_lock`) are a drop-in upgrade.

## References

- Lamport, *The Part-Time Parliament* (1998). Paxos — foundational background for why mandatory distributed coordination is hard and why advisory locks with a single authoritative store are a pragmatic compromise.
- Gray & Reuter, *Transaction Processing: Concepts and Techniques* (1993). The classic on lock modes, lease semantics, and why TTL + heartbeat is the standard pattern for crash-tolerant leases.
- [Postgres advisory locks](https://www.postgresql.org/docs/current/explicit-locking.html#ADVISORY-LOCKS) — the native mechanism we'd migrate to for distributed scenarios.
- Shapiro et al., *Conflict-Free Replicated Data Types* (2011). CRDTs — considered and rejected for agent edits (see Alternatives).
