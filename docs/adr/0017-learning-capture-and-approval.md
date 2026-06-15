# ADR 0017 — Learnings: approval gate + agent-proposed capture

**Status:** proposed
**Date:** 2026-06-14
**Extends:** ADR 0014 (scoped memories), ADR 0010 (tasks). Adds a lifecycle to the `learnings` table.
**Constrained by:** ADR 0015 (retrieval scope reduction). This ADR exists to add *capture* without reopening the corpus-poisoning failure that 0015 closed.

## Context

`ygg learn` captures durable, file-scoped rules and surfaces them deterministically — SQL predicates on `(repo_id, file_glob, rule_id)`, no embeddings (see `src/models/learning.rs`). Learnings fire in two places today:

- **task claim** — rules whose `file_glob` matches a path mentioned in the task body (`surface_for_files`).
- **file edit** — the `PreToolUse` hook surfaces file-scoped rules when an agent edits a matching file, deduped per session (yggdrasil-180).

Retrieval is solved. **Capture is not.** Every learning today is hand-written via `ygg learn create`. In practice agents almost never do this unprompted, so the corpus stays thin — the retrieval machinery is starved of rules to retrieve. The obvious fix is to capture learnings automatically from the corrections agents receive (the user pushes back, the agent adjusts — that adjustment is a durable rule). That is precisely the move ADR 0015 forbade:

> Corpus poisoning risk: anything written to the retriever (digests, summaries) fed back into future retrievals → silent drift.

So we have a standoff. Manual-only capture keeps the corpus honest but thin and unused. Automatic capture fills the corpus but reintroduces the drift that motivated 0015's teardown of the embedding layer. We want the fill rate of automatic capture with the safety of manual capture.

## Decision

Two changes, the first of which makes the second safe.

### Decision 1 — Approval gate on learnings

Add a lifecycle to `learnings`. A learning is `pending` or `active`. **Only `active` learnings are ever surfaced** (`surface_for_files`, `surface_for_edit`, and `ygg learn list` default to `status = 'active'`). `pending` learnings exist in the table, are visible only via `ygg learn pending`, and fire on nothing until promoted.

```sql
ALTER TABLE learnings ADD COLUMN status TEXT NOT NULL DEFAULT 'active'
  CHECK (status IN ('pending', 'active'));
ALTER TABLE learnings ADD COLUMN source TEXT NOT NULL DEFAULT 'manual'
  CHECK (source IN ('manual', 'proposed'));
ALTER TABLE learnings ADD COLUMN approved_at  TIMESTAMPTZ;
ALTER TABLE learnings ADD COLUMN approved_by  UUID REFERENCES agents(agent_id);
CREATE INDEX idx_learnings_status ON learnings (status) WHERE status = 'pending';
```

`DEFAULT 'active'` preserves today's behavior exactly: `ygg learn create` still produces an immediately-firing learning. Nothing about the existing manual path changes. The vocabulary deliberately mirrors ADR 0016's task-approval columns (`approved_at` / `approved_by_agent_id`, the `awaiting_approval` state) so the two gates read the same.

CLI surface:

```
ygg learn pending [--all]            # list status='pending', newest first
ygg learn approve <id>               # status → active, stamp approved_at/by
ygg learn reject  <id> [--reason]    # delete (or status='rejected'; see below)
ygg learn create "..." [--pending]   # manual create can opt into the gate
```

### Decision 2 — Agent-proposed capture (not model-extracted)

Capture is **agent-proposed, human-approved.** The agent that just received a correction writes the rule itself via a new verb:

```
ygg learn propose "<rule>" --file-glob "<glob>" [--rule-id <id>] [--context "<why>"]
```

`propose` is `create` with `status='pending'` and `source='proposed'`. The proposing agent already holds the full context of the correction — it does not need a summarizer. Crucially, **ygg runs no model to do this.** ADR 0015 removed the local Ollama generator precisely because a second, weaker model corrupted identifiers (the `KIND_BOSTON` bug). We do not bring one back. The only intelligence in the loop is the agent already in the loop.

The "automatic" feeling comes from a **nudge**, not a generator: the `Stop` hook (and a CLAUDE.md directive) reminds an agent, when it wrap-ups a session in which it was corrected, to record the durable rule with `ygg learn propose`. Whether to propose, and what to write, stays with the agent. Proposals accumulate in the pending queue; a human triages with `ygg learn pending` → `approve`.

This is the same shape as conversation-derived learnings in other review tools — propose from context, gate behind approval — but with generation kept in the agent that has the context rather than a separate corruptible model.

## Why this is consistent with ADR 0015

0015's failure mode was a **closed loop**: text written to the retriever was automatically read back into future retrievals, so errors compounded silently. The approval gate **breaks the loop**: nothing `proposed` is ever retrieved until a human promotes it to `active`. The automatic step (propose) and the retrieved step (active) are separated by a mandatory human decision. Concretely:

- No embeddings, no similarity, no scoring — retrieval stays the exact `(repo, file_glob, rule_id)` predicate 0015 left in place.
- No generator model inside ygg — proposals are authored by the agent, not synthesized by a daemon.
- No auto-promotion — `pending → active` is only ever a human (or explicitly-authorized lead-agent) action.

The corpus can only be poisoned by something that auto-writes *and* auto-reads. We keep auto-write (cheap, high-recall) and make read require approval (the precision control). That is the reconciliation.

## Alternatives rejected

- **Auto-capture straight to `active`.** Maximum fill rate, but it is exactly 0015's closed loop — a bad correction becomes a firing rule with no human in between. Rejected; this is the thing 0015 tore out.
- **Local-model extraction of learnings from transcripts.** Reintroduces the Ollama generator 0015 removed for identifier corruption. Rejected; the agent already has better context than any post-hoc summarizer.
- **Semantic dedup of proposals.** Tempting for keeping the pending queue clean, but it needs embeddings. Rejected; reuse the existing token-set string similarity (`ygg task dupes`) if dedup is needed at all.
- **Stay manual-only (status quo).** Honest corpus, but it stays thin and the retrieval machinery goes unused. Rejected; this is the problem statement.
- **`rejected` as a tombstone vs hard delete.** Keeping rejected proposals as `status='rejected'` prevents the same bad proposal from being re-proposed and re-triaged forever. Leaning tombstone, but it is a queue-hygiene detail, not load-bearing for the decision — deferred to implementation.

## Consequences

- Existing learnings and `ygg learn create` are unchanged (`DEFAULT 'active'`). Pure additive migration; revertible by dropping the columns.
- A new triage surface (`ygg learn pending`/`approve`/`reject`) — small, mirrors the task-approval CLI.
- The pending queue is a new thing a human must tend. If it is never triaged, proposals simply never fire — fail-safe, not fail-open.
- Sets up a future dashboard pane (pending count, approve/reject inline) without committing to one now.

## Rollout

1. **M1 — Gate schema + read-path filter.** Add columns; teach `surface_for_files`/`surface_for_edit`/`list` to filter `status='active'`. No behavior change (everything defaults active).
2. **M2 — Triage CLI.** `ygg learn pending` / `approve` / `reject`; `--pending` flag on `create`.
3. **M3 — `ygg learn propose`.** The capture verb (`pending` + `source='proposed'`).
4. **M4 — Stop-hook nudge + CLAUDE.md directive.** The behavioral push that fills the queue. Ship last, after the gate and triage exist, so proposals always have somewhere safe to land.

Each milestone is independently revertible. M4 is a prompt/nudge change, not schema.

## Relationship to prior ADRs

- **Extends** ADR 0014 (scoped memories / learnings) with a lifecycle.
- **Constrained by** ADR 0015 (retrieval scope reduction) — the approval gate is the mechanism that lets capture coexist with 0015's no-poisoning rule.
- **Mirrors** ADR 0016 (autonomous execution) — reuses its approval vocabulary (`approved_at` / `approved_by`, a gated state) for consistency.
