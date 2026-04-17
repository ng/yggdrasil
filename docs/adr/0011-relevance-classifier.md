# 0011 — Relevance classifier for `ygg inject` top-k

- **Status**: Accepted
- **Date**: 2026-04-18
- **Relates to**: [Retrieval and injection](../retrieval.md), [Open questions](../open-questions.md), [LLM usage](../llm-usage.md)

## Context

`ygg inject` today uses pure cosine-distance thresholding to decide which of the top-k pgvector candidates reach the agent's context as `[ygg memory | ... ]` lines. Cosine distance is a solid recall filter but a coarse precision filter: *near in embedding space* ≠ *useful for this turn*. The dense-retrieval literature is unambiguous that a cross-encoder reranker moves precision 2-5× over cosine alone.

The pollution risk section of [Open questions](../open-questions.md) and the LLM-usage research doc both identify this as the highest-leverage single improvement we can make to the retrieval pipeline. Without precision gating, every new "similar-enough" hit steals tokens from the agent's useful context window and can trigger the see-sawing / sycophancy failure modes.

## Decision

Add a zero-shot pairwise relevance classifier between the pgvector top-k query and the stdout emission in `ygg inject`. The classifier runs locally via Ollama's `/api/generate` endpoint against a small chat model (default `llama3.2:1b`, configurable via `YGG_CLASSIFIER_MODEL`). For each candidate, we prompt:

```
You are rating whether a past memory is relevant to the user's current prompt.
Respond with JSON only: {"relevant": 0 or 1, "score": 0.0-1.0}

Current prompt: <prompt>
Past memory: <candidate snippet>
```

Ollama's `format: json` option forces structured output. We parse `{relevant, score}` and gate on `score >= threshold` (default 0.55, configurable via `YGG_CLASSIFIER_THRESHOLD`). Candidates are scored in parallel via `futures::join_all` so wall latency stays bounded by the slowest single call rather than summed.

Failure modes:
- **Model unavailable / Ollama down**: short health check at the top of `classify_batch`; on failure we emit a `classifier_bypassed` event and pass the top-k through unfiltered. Correctness is not affected — we fall back to the current behavior.
- **Invalid JSON from the model**: default to `score = 1.0` (fail-open). The alternative is fail-closed (suppress the hit), but that compounds the recall-loss case with a retrieval-model failure.
- **Latency budget blown** (> 2s total): a `tokio::time::timeout` wraps the whole classify pass; on timeout we emit all candidates unfiltered with an event flag.

Disable entirely with `YGG_CLASSIFIER=off` for debugging or measurement baselines.

### Alternatives considered

- **Keep cosine-only with a tighter threshold.** Moves both recall and precision together — you can't get one without the other. We want to widen pgvector recall (more candidates surfaced by raw similarity) and tighten the agent-facing gate *separately*.
- **Train a dedicated cross-encoder (e.g. `ms-marco-MiniLM`)**. Better numbers in theory, new dependency, no training pipeline, and we don't have labelled data. Not justified for v1 — revisit once we've collected the passive labels described below.
- **Collect labels and train a logistic head on the embedding pair**. Same data problem. We can't train what we haven't logged.
- **Ask the agent itself to decide via an MCP tool.** Violates the "don't ask the expensive substrate to do what the cheap substrate can do" principle. Rejected per [Design principles § Substrate separation](../design-principles.md).

### Data-collection side-effect

Every classifier decision is logged as a `classifier_decision` event with payload `{candidate_id, prompt_snippet, score, kept}`. Every subsequent user turn that *references* a kept candidate is implicitly a positive label; every candidate the agent ignores is an implicit negative. That's the passive-labelling pipeline that unblocks a dedicated classifier down the road — but we ship the zero-shot version first and collect labels in parallel.

## Consequences

**Positive**

- Pollution risk drops substantially: weak-similarity hits get filtered even when they survive the cosine cutoff.
- No new dependencies — reuses the Ollama instance we already run.
- Falls back cleanly to current behavior if classification breaks.
- Passive label collection starts immediately, feeding a future trained classifier.
- `ygg logs --follow` gets a per-candidate score column, making threshold tuning a human-in-the-loop exercise.

**Negative**

- Adds one chat-model call per candidate per user turn. On CPU Ollama with `llama3.2:1b`, that's ~60-150ms per call; parallelized with top-k=8, wall clock lands somewhere around 100-300ms added to every inject.
- Requires pulling a chat model (~1GB) at `ygg init` time. Handled gracefully if the pull fails — classifier bypass is the fallback.
- Score values are not calibrated across model versions. Swapping models can shift the useful threshold; we version the classifier model in event payloads so historical data stays interpretable.
- Zero-shot scoring is noisier than a trained classifier. The passive-label pipeline exists to close that gap over time.

**Future triggers to revisit**

- If classifier wall latency exceeds ~500ms in practice, switch from per-candidate calls to a single batched prompt that rates all k candidates at once.
- Once we have a few thousand labelled pairs from live traffic, train a dedicated cross-encoder and compare.
- If the classifier becomes the thing users tune most, expose it via `ygg classifier stats` / `ygg classifier replay <prompt>` so the feedback loop can live in the CLI.

## References

- Docs: [LLM usage § 1](../llm-usage.md).
- Retrieval-reranking literature: Nogueira & Cho, *Passage Re-ranking with BERT* ([arXiv:1901.04085](https://arxiv.org/abs/1901.04085)); MS MARCO cross-encoder checkpoints.
- Ollama structured outputs: https://github.com/ollama/ollama/blob/main/docs/api.md#request-with-structured-outputs
