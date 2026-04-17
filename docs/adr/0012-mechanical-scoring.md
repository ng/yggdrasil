# 0012 — Mechanical scoring for `ygg inject` candidates

- **Status**: Accepted
- **Date**: 2026-04-18
- **Relates to**: [ADR 0011](0011-relevance-classifier.md), [Retrieval and injection](../retrieval.md), [Open questions](../open-questions.md)

## Context

The LLM classifier shipped in [ADR 0011](0011-relevance-classifier.md) runs at the end of the retrieval pipeline to gate candidates. At 1B-parameter local models it doesn't discriminate well and mostly rubber-stamps; at larger models the wall-time cost per user turn gets uncomfortable.

Separately, cosine distance alone misses a lot of signal we already have in the DB:

- The kind of the node (a `digest` is higher-quality evidence than a raw `user_message`)
- The age of the node (a three-day-old directive probably beats a three-month-old one)
- Whether the node is from the current repo, current agent, current session
- How often the same node has already been surfaced this session
- Lexical overlap with the query (pgvector plus `tsvector` in one DB)

These are deterministic, zero-latency, and easier to tune than a chat model's verdict. The ADR 0011 roadmap anticipated this — it's time to promote the work.

## Decision

Add a **mechanical scoring pass** before the LLM classifier. A small weighted-sum function combines features we already have; candidates are **re-ranked** by the score, and only the worst cases are dropped. The LLM classifier (ADR 0011) becomes an optional overlay rather than the primary gate; it's off by default until a more capable model is available.

### Scoring function

Each candidate gets a multiplicative score:

```
score = cosine
      · kind_boost[node_kind]          (directive=1.3, digest=1.2, user_message=1.0, assistant=0.9, tool_result=0.8)
      · age_weight(age_days)           (half-life decay, default 14 days)
      · repo_weight(same_repo)         (1.0 same, 0.85 cross-repo)
      · agent_weight(same_agent)       (1.0 same, 0.95 cross-agent)
      · freshness_bonus(surface_count) (small penalty for repeats within session)
```

All weights are env-configurable. Defaults are intentionally gentle — "bias toward deferring and letting it through" is the user-stated design principle. Multiplicative form means a candidate only gets badly penalized when several negative signals align.

### Keep policy: rank, don't gate

Default behavior is **re-rank then cap at max-hits**, not **filter below threshold**. Specifically:

- Score every candidate.
- Sort by score descending.
- Keep the top `YGG_MECH_MAX_HITS` (default 8 — the same cap we already use for cosine top-k).
- Additionally drop anything with score < `YGG_MECH_MIN_SCORE` (default 0.05, very permissive — only kicks in for truly weak candidates).

This means in the typical case the only user-visible change is *order* — stronger signals rise, weaker ones fall, but they all survive unless extremely weak. Matches the user's "incremental savings are savings" framing.

### Relationship to the LLM classifier

Pipeline after this ADR:

```
pgvector top-k (recall)
  → mechanical score (rank + soft cap)
  → optional LLM classifier (precision overlay, off by default)
  → stdout emit
```

The LLM classifier operates on the mechanically-scored survivors, so it has fewer candidates to rate and those candidates are already better ordered. When `YGG_CLASSIFIER=off` (new default) the mechanical score is the only filter, matching today's latency profile.

### Alternatives considered

- **Filter threshold as primary gate.** What the LLM classifier does now. Brittle — a bad threshold hides useful context silently. Rank-and-cap degrades gracefully.
- **Learned scorer (logistic on the same features).** Needs labels we don't have yet. Ship the hand-tuned weighted sum first, collect labels via the event log, train later.
- **Replace cosine with a cross-encoder reranker.** Better quality but new dependency (ONNX runtime / candle), new model to ship. Mechanical scoring ships today with zero new deps.
- **Kind-only boosting.** Too narrow — recency and cross-repo penalties matter as much as kind in practice.

## Consequences

**Positive**

- Zero-latency: microseconds of Rust per candidate.
- Deterministic and debuggable — every knob is an env var.
- No new dependencies.
- Makes the LLM classifier truly optional; retrieval quality no longer depends on a flaky 1B model rubber-stamping.
- Event log captures the full component vector per candidate, so tuning is a human-in-the-loop exercise and labels accumulate passively for a future learned scorer.

**Negative**

- The weights are hand-tuned initially. They'll be wrong. That's fine because they're cheap to adjust.
- Multiplicative form means a candidate with one bad feature (e.g. 180 days old) gets crushed even if cosine is high. This is intentional — age decay is real — but may need per-feature floor clamps to avoid overshooting.
- No semantic understanding — two candidates with identical cosine/kind/age/repo but one being off-topic and one on-topic score identically. That's the classifier's job, layered on top.

**Future triggers to revisit**

- Collect enough `ScoringDecision` events to train a logistic regression. Compare hand-tuned weights to learned weights.
- If the multiplicative form causes hot spots where one feature dominates, switch to additive + sigmoid or per-feature weight clamps.
- If hybrid retrieval ships (yggdrasil-8), expose BM25 score as another feature in the product.

## References

- ADR 0011 (LLM classifier) — now layered on top of this.
- `src/salience.rs` (existing recency utilities — generalize and pull into the new scoring module).
- *Open questions* § "Known LLM failure modes" — mechanical scoring attacks the pollution-risk side of that analysis directly.
