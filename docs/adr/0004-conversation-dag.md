# 0004 — Conversation as a typed DAG, not a flat transcript

- **Status**: Accepted
- **Date**: 2026-04-15

## Context

A Claude Code session is usually stored as a flat JSONL transcript — one line per turn, append-only. That works fine for resume and replay, but it's a poor substrate for the things Yggdrasil wants to do:

1. **Similarity search across agents**: "Has anyone — this agent or another — talked about this before?" A flat per-session file can't answer that without a global index.
2. **Parent/child relationships**: Tool calls belong to the assistant message that invoked them; results belong to the tool call. A flat log erases that structure.
3. **Partial replay and pruning**: Digesting a session should rewrite the head node, not the whole log.
4. **Cross-agent citation**: When `ygg inject` surfaces a prior node, we want to link back to the exact turn, not a line range in a file.

## Decision

Every semantic unit in a conversation is a **typed node** in a Postgres table:

```
node_kind := user_message | assistant_message | tool_call | tool_result
           | digest | directive | system | human_override
```

Nodes have `parent_id` (for tool_call → tool_result, etc.), `agent_id` (owning agent), an `ancestors` UUID array (materialized path for cheap subtree queries), and an `embedding` column (pgvector). The resulting structure is a DAG — mostly tree-shaped per session, with similarity-derived edges bridging sessions and agents.

The canonical transcript is derivable from the DAG; the DAG is not derivable from the transcript without a lot of guessing. So the DAG is the source of truth, and the transcript is a projection.

### Alternatives considered

- **Flat JSONL per session, indexed separately**. The Claude Code default. Works, but every cross-session query has to rebuild structure on the fly, and `parent_id` relationships must be inferred. We'd spend our time writing transcript parsers instead of coordination primitives.
- **Event-sourced log, derive state**. Conceptually clean but slow to query, and materializing views on every read is expensive when the `ygg inject` hook fires on every user prompt.
- **Store only summaries / digests, discard raw turns**. Cheaper storage but destroys the information needed for high-quality similarity retrieval. Embeddings of summaries lose the specific phrasing that makes a hit useful.

## Consequences

**Positive**

- `UserPromptSubmit` can run a single query: "find nodes across all agents whose embedding is within cosine distance 0.4 of this prompt" → join agent metadata → emit directives. No transcript parsing.
- `Stop` hook can write a `Digest` node as a *sibling* that summarizes a range, leaving raw turns intact for later high-fidelity replay.
- Tool calls and their results are structurally linked, so `ygg observe` can distinguish "the call" from "the result" without heuristics.
- Pruning strategies are first-class: we can drop `tool_result` nodes older than N turns while keeping `user_message` and `assistant_message` for recovery.

**Negative**

- Every turn is a row. High write volume during active coding sessions. Mitigated by batching in `ygg observe`.
- Schema migrations are now load-bearing. We pay the complexity of `sqlx::migrate!`.
- Recovering a human-readable transcript requires a join + ordering — slightly more work than `cat session.jsonl`. `ygg observe --print` materializes on demand.

**Future triggers to revisit**

- If the `nodes` table ever exceeds ~10M rows on a single host, partitioning by `agent_id` or `created_at` is the obvious move.
- If a different agent harness (Codex, Cursor, Gemini CLI) wants to plug in, the node schema is the stable interface — they just need to write nodes in the right shape.

## References

- Packer et al., *MemGPT: Towards LLMs as Operating Systems*, [arXiv:2310.08560](https://arxiv.org/abs/2310.08560). Digest-as-node and recall-from-archive directly inspired by MemGPT's hierarchical memory.
- Park et al., *Generative Agents: Interactive Simulacra of Human Behavior*, [arXiv:2304.03442](https://arxiv.org/abs/2304.03442). The memory stream + reflection hierarchy is a close ancestor of our node-kind taxonomy.
- Lewis et al., *Retrieval-Augmented Generation for Knowledge-Intensive NLP Tasks*, [arXiv:2005.11401](https://arxiv.org/abs/2005.11401).
- Event sourcing (Fowler, *Event Sourcing*, 2005). Considered and rejected as too slow for hook-latency-sensitive queries — see Alternatives.
