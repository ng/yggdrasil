# ADR 0016 — Autonomous execution: scheduler + durable task runs

**Status:** proposed
**Date:** 2026-04-23
**Supersedes parts of:** ADR 0010 (tasks as beads replacement) — tasks grow an execution layer beneath the semantic layer.
**Related:** ADR 0015 (retrieval scope reduction). This is the positive half of the same pivot: with retrieval scoped down, orchestration becomes the differentiator, and "orchestration" today stops at task tracking. Autonomous execution is the missing layer.

## Context

Yggdrasil's orchestration primitives — `ygg task`, `ygg lock`, `ygg spawn`, workers, worktrees, messaging bus, learnings — are individually working. A human reviewing `ygg status` today sees agents running, locks held, tasks queued. What they do not see, and what does not exist, is anything that picks those tasks up on its own.

Every advancement of the DAG today is human-driven: a human runs `ygg task claim`, runs `ygg spawn`, watches the worker finish, reopens the next task, spawns the next agent. `ygg plan supervise` is the only DAG executor and it is:
- synchronous and tmux-bound (keep it running or it stops),
- manually invoked (`ygg plan supervise <epic-ref>`),
- single-epic-scope (no global scan for ready work),
- retry-free (a failed blocker blocks forever until a human intervenes).

Tasks themselves carry metadata (title, acceptance, priority, deps, labels) but no execution state: no input payload, no output payload, no attempt history, no retry counter, no timeout, no heartbeat. `task_events` exists as an audit trail but has no consumer. The "close reason" string is the only terminal artifact.

The result is that Yggdrasil is a very good whiteboard and a very poor kitchen. Agents can see each other's work; they cannot advance each other's work.

We ran five parallel research threads (2026-04-23) covering durable-execution engines, LLM-agent frameworks, the current codebase surface, Postgres data models for durable execution, and orchestrator eval benchmarks. The reports converge strongly. This ADR records the decisions.

## The two shapes of durable execution

Every serious durable-execution system is one of two shapes:

1. **Journal + deterministic replay.** Temporal, Cadence, Restate, Inngest. The workflow is code whose re-execution reads a recorded event history and short-circuits already-completed steps. Determinism is mandatory; non-determinism is quarantined into "activities" / "steps" whose outputs are memoized.
2. **Database checkpoint + step function.** DBOS, Hatchet, Prefect 3, Dagster, Airflow. Each step's output is written as a row; retry/resume re-enters the orchestrator and asks "what's next?" rather than replaying any code.

For LLM-agent workloads — where every "activity" is a `claude` invocation that takes tens of seconds to tens of minutes, costs real money, and produces stochastic prose — the second shape is strictly easier. The first shape is actively harmful for three reasons:

- **Replay is meaningless for LLM turns.** Claude Code keeps its own conversation history. Asking the orchestrator to "replay" a workflow that contains an LLM means either stubbing past LLM outputs (faking the agent's memory) or invoking the LLM again (paying twice for the same result).
- **History saturation.** Temporal's own community reports history bloat on LLM payloads — diffs, transcripts, tool results. Every re-executed activity copy ends up in history.
- **Determinism is a constraint we don't get anything for.** Our "workflow code" is data (task rows), not Python/Go/TS. There is no workflow function to be deterministic in the first place.

Checkpoint-shaped is the right fit. Within that family, DBOS is the closest reference — Postgres-native, no separate orchestration service, schema that maps almost directly onto what Yggdrasil already has.

## Decisions

### D1. Adopt a DBOS-shaped checkpoint model

Our unit of durability is the task row plus an `attempts` row per execution. The source of truth for "where are we" is Postgres rows. We do not have any Yggdrasil code that needs to be replayable.

The `events` table remains an audit log. It is not replayed to reconstruct state. It is a read-only record of what happened, consumed by `ygg logs`, the TUI, and the messaging bus.

### D2. One scheduler daemon, authoritative

Exactly one `ygg scheduler` process is the authority for task → attempt → spawn → reconcile. It runs as a long-lived daemon (or as a tmux-hosted agent of itself, see Consequences).

Its tick loop, at 1–5 second intervals (with LISTEN/NOTIFY wake-ups for zero-latency dispatch of new ready work):

1. Claim N ready tasks via `SELECT … FOR UPDATE SKIP LOCKED` (DBOS pattern).
2. For each claimed task: insert an `attempts` row, `ygg spawn` an agent, bind attempt→agent→worker.
3. Reconcile terminal attempts: read outcome from hook-written state, close the attempt, release locks, advance downstream tasks.
4. Enforce deadlines, heartbeats, retry backoff, and fingerprint-based loop detection.

The scheduler contains no LLM calls. It is pure Rust + SQL. This separation is the single most important design decision: **the durable kernel must be deterministic even when the work it dispatches is not.** LLMs live exclusively inside spawned agents.

### D3. Workers are passive; the scheduler owns transitions

Workers (Claude Code processes in tmux windows) run, produce output, and stop. They do not drive state transitions. The existing `Stop` hook writes outcome data into the attempt row (exit code, commits, result summary); the scheduler reads that data on its next tick and moves the attempt to a terminal state.

This centralization is defensive. Every distributed-status-update system we surveyed eventually breaks on this (Airflow retries, CrewAI manager drift, Claude agent-teams lead-shuts-down-early). The scheduler being the only writer of terminal state is cheap to build and cheap to reason about.

### D4. Task / attempt split, modeled on DBOS

- **`tasks`** is the *workflow-level* row. It stays the semantic unit: title, acceptance, deps, kind, priority, status. New columns: `runnable`, `max_attempts`, `current_attempt_id`, `deadline`, `result_blob_ref`.
- **`task_runs`** (synonym: `attempts`) is the *execution-level* row. One per attempt. Carries state, reason, heartbeat, agent/worker/session binding, retry metadata, input/output/error payloads (small) + blob refs (large), commit SHAs, branch, PR URL.

Full schema in [`docs/design/task-runs.md`](../design/task-runs.md).

### D5. State machine

Task status stays semantic (`open | in_progress | blocked | closed | awaiting_children`). Run state is distinct and finer:

```
scheduled → ready → running → { succeeded | failed | crashed | cancelled }
                                            ↓ retryable + attempt < max
                                        retrying → (new attempt)
                                            ↓ attempt = max
                                         poison (requires human)
```

`failed` and `crashed` are distinct (Prefect's distinction): `failed` = the agent reported a semantic failure (tests didn't pass, acceptance unmet); `crashed` = infra-level failure (tmux window gone, heartbeat expired, process killed). Retry policies differ.

The invariant the scheduler enforces is **at-most-one-active-attempt per task**, not exactly-once completion. Two concurrent agents on the same task is expensive and destructive; one attempt succeeding after an earlier one crashed is the normal case and is fine.

### D6. Payload strategy: inline small, reference large

Small, structured metadata (commit SHAs, task IDs, counts, learnings) lives inline as JSONB on `task_runs.input` / `.output` / `.error`. Discipline: if a field exceeds 16 KiB, it belongs out-of-band.

Large payloads (diffs, logs, tool-use transcripts) go to a content-addressed blob store under `.ygg/blobs/<sha256>` with the hash stored in the `task_runs` row. Dedup is free; git-like mental model.

For code specifically, the blob is usually redundant: **the commit SHA is the claim-check.** A 40-byte TEXT pointer references the entire tree, subject to git's own content addressing. For code-producing agents, we store SHAs, not diffs.

### D7. Dynamic task emission + `awaiting_children`

Agents spawn child tasks during execution. Children reference the parent via `parent_task_id`. The parent transitions `running → awaiting_children` when its own work is done but children remain live. The scheduler transitions `awaiting_children → closed` when all children hit terminal state (Hatchet pattern).

Fan-in is implemented via `ygg task wait <child-ids>` which blocks on a Postgres LISTEN channel with a polling fallback.

Epic completion is scoped: `epic_done = ∀ task ∈ epic : task.status ∈ { closed, cancelled }`. There is no global completion signal; Yggdrasil is a cross-repo coordinator, there's always potentially more work.

### D8. Three approval levels mapping to Claude Plan Mode

Per-task configurable:

| Level | Behavior | When to use |
|---|---|---|
| `auto` (default) | Scheduler spawns immediately when ready. | Low-risk repetitive work. |
| `approve_plan` | Scheduler spawns the agent in Plan Mode. Agent writes a plan, blocks on `task.approved_at IS NOT NULL`. Human `ygg task approve <ref>` unblocks. | Migrations, config, cross-cutting edits. |
| `approve_completion` | Scheduler spawns freely; agent cannot close its own task, drops `awaiting_review`. Human closes. | PRs, prod deploys, high-stakes output. |

This reuses Claude Code's existing Plan Mode rather than inventing a parallel approval UX. Emergency override: `YGG_AUTO_APPROVE=1` env on the scheduler, logged prominently to the events table.

### D9. Hard caps at every level

Without caps, autonomous = infinite-cost. Enforced by the scheduler:

- **Per-task:** `max_attempts` (default 3), `timeout_ms` (default 1h), `max_tool_calls` (propagated to Claude Code harness).
- **Per-epic:** `max_depth` (recursive child spawn cap), `max_total_tasks`, `budget_usd`.
- **Scanner-level:** `max_concurrent_agents` (global), `max_agents_per_repo`, `max_agents_per_host`.

Loop detection via fingerprint: a task's fingerprint is `sha256(title | last N attempt outcomes)`. If the same fingerprint appears N times within a window, the scheduler refuses further attempts and marks the task `poison` with reason=`loop_detected`.

### D10. Three agent roles (Devin pattern, optional)

Tasks may declare an `agent_role`: `planner` (read-only + `ygg task create`, decomposes epics), `executor` (read + write code), `critic` (read + produce review artifact, can reopen). The scheduler dispatches to spawning profiles that scope the subagent's tool allowlist and prompt.

This is optional — the MVP dispatches everything to `executor`. Roles get introduced when the planner escape-hatch materializes (D11).

### D11. LLM planner is an escape hatch, not the kernel

Declarative DAGs are the default. An epic can opt into `plan_strategy: llm` metadata, which causes the scheduler to spawn a `planner` agent on first-see. The planner reads the epic's design text, emits child tasks with deps via `ygg task create`, and exits. The scheduler picks up the children as it would any other ready work.

This splits the frail part (LLM does planning) from the durable part (scheduler is deterministic Rust). Microsoft's AutoGen→MAF pivot ("stop letting a manager LLM decide who speaks; use typed graph edges") is industry precedent.

### D12. Eval suite is a separate subcommand (`ygg bench`)

`ygg eval` is a dashboard over live events (and will be gutted as Phase 4 of ADR 0015 lands). `ygg bench` runs scripted scenarios against three baselines (vanilla-single, vanilla-tmux, ygg-N) and produces comparable numbers. Entangling the two entangles dashboard code with subprocess orchestration.

Tier A metrics: wall-clock, pass^k (k=4, tau2-bench), token-cost, agent-hours, DAG completion rate, lock-wait p50/p95, rework rate. Headline graph: METR-style 50%-time-horizon logistic fit.

Full spec in [`docs/eval-benchmarks.md`](../eval-benchmarks.md).

## What we explicitly reject

- **Deterministic workflow replay** (Temporal, Cadence, Restate, Inngest pattern). Wrong fit for LLM decisions.
- **A separate orchestration service** (Temporal cluster, Hatchet, Prefect server, Dagster webserver+daemon split, Airflow, Conductor, Step Functions). Postgres is the bus.
- **Predeclared static DAGs only**. Dynamic spawn is table stakes for agent work.
- **JSON/YAML DSL for workflow definition**. Workflows are data (task rows), not a DSL.
- **Push-based per-step HTTP invocation** (Inngest). We run tmux-hosted long processes; pull fits.
- **LangGraph-style in-process agent state graphs**. Useful inside a single agent run, orthogonal to cross-agent coordination.
- **Exactly-once completion semantics**. At-most-one-active-attempt is what locks already give us; "exactly-once" is expensive and buys little.
- **Trajectory-match grading** in evals. Anthropic's own finding: identical starting points produce different valid paths. End-state grading only.
- **LLM-as-judge for pass/fail** in evals. LLM-judge produces a quality score to *complement* a binary deterministic grader, never to replace it.

## Consequences

### What we gain

- Agents advance each other's work without human intervention for the 80% of work that doesn't need approval.
- Rework, retry, and poison become observable — today they're invisible.
- Epic throughput scales with `max_concurrent_agents` instead of with a human's wrist.
- Eval numbers become trend-able; "did the last scheduler change regress coordination latency?" has an answer.
- Failure modes ("one agent stuck, blocking six downstream") surface via the dashboard instead of getting discovered by a human noticing silence.

### What we give up

- **The TUI becomes more load-bearing.** If the scheduler stops, nothing advances. We already depend on `watcher.rs` for lock reap and worker reconciliation; the scheduler is the same shape with more responsibility. Mitigation: scheduler is idempotent on restart (everything is state on rows); a stopped scheduler is noisy but not catastrophic.
- **The escape-hatch temptation.** Once the scheduler runs, it will be tempting to do the LLM-planner, DSL-for-workflows, etc. Hold the line. Every escape hatch gets its own ADR and its own opt-in.
- **Schedule storm risk.** Bad config could spawn many agents at once. Caps mitigate but require care.
- **Approval fatigue.** If too many tasks default to `approve_plan`, humans stop reading. Keep defaults liberal; tighten per-task where stakes warrant.

### What stays unchanged

- All existing task CLI commands continue to work (`ygg task create/claim/close/dep/list/show/ready`).
- All existing lock, spawn, interrupt, status, logs, dashboard commands unchanged.
- Shared-DB semantics (ADR 0008), tmux substrate (ADR 0007), session-per-worker (ADR 0013), learnings (already structured).
- The pivot described in ADR 0015 proceeds on its own cadence; this ADR adds new substrate and does not re-open the retrieval question.

## Migration plan (five reversible steps)

**M1 — Add `task_runs` + `tasks` columns (reversible, zero-behavior-change).**
Pure schema addition. `runnable` defaults to FALSE; nothing auto-schedules yet.

**M2 — Backfill synthetic runs for in-progress tasks (idempotent script).**
One run row per live `tasks.status = in_progress` + its live worker. Closed tasks optionally get a terminal run derived from git state.

**M3 — Teach CLI to write runs (manual-mode parity).**
`ygg task claim` opens a run. `ygg task close` closes the current run. `ygg spawn` writes a scheduled run before launching tmux. `ygg task show` prints run history. No scheduler yet; behavior identical to today for a human driver.

**M4 — Ship `ygg scheduler` + flip `runnable=true` per-task opt-in.**
The daemon is real; only tasks explicitly marked `runnable` get auto-dispatched. Everything else is manual.

**M5 — Default `runnable=true` for new tasks of kind `task|bug|feature|chore`.**
Epics stay manual-runnable (they opt in via `plan_strategy`). After one week of no regression.

M1–M4 are all reversible via commit-revert. M5 is a behavior flip, not a schema change, so also reversible. No irreversible steps in this ADR.

## Follow-up

Child tasks under the epic track each phase. The scheduler MVP is ~300 LOC of Rust (durable-execution research estimate). Blob store, attempts table migration, and tests are separate tasks. The eval suite (`ygg bench`) is a parallel track — scaffold + Scenario 1 runnable in the MVP window, remaining scenarios incremental.

## Relationship to prior ADRs

- **Extends** ADR 0010 (tasks as beads replacement) — adds an execution layer beneath the semantic layer.
- **Complements** ADR 0015 (retrieval scope reduction) — with retrieval scoped down, execution becomes the value.
- **Depends on** ADR 0007 (tmux substrate), ADR 0008 (shared DB), ADR 0013 (session state split), ADR 0003 (locks) — all the primitives the scheduler composes.
- **Does not supersede** ADR 0011 (relevance classifier) or ADR 0014 (scoped memories) — those are on ADR 0015's retirement track.

## Sources

Research threads run 2026-04-23 surveyed DBOS, Temporal/Cadence, Restate, Inngest, Dagster, Airflow, Prefect, Ray, Conductor, Step Functions, Hatchet; LangGraph, CrewAI, AutoGen, OpenAI Swarm/Agents SDK, Claude Agent SDK + agent-teams, claude-swarm, LlamaIndex Workflows, Pydantic AI/DSPy, Mastra, smolagents, SWE-agent, OpenHands, Devin; SWE-bench Verified, SWE-Lancer, MLE-bench, OSWorld, Terminal-Bench 2.0, METR, GAIA, AgentBench, tau2-bench, MultiAgentBench, MASLab, Anthropic's own multi-agent research system. Full source list at the end of [`docs/design/task-runs.md`](../design/task-runs.md) and [`docs/eval-benchmarks.md`](../eval-benchmarks.md).
