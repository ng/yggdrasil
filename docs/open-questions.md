# Open questions (this is partly an experiment)

> Yggdrasil is a working system, but it is also a hypothesis. This document holds the questions we haven't answered yet, the failure modes we're watching for, and the directions we're considering.

See also: [Design principles](design-principles.md) · [Retrieval and injection](retrieval.md) · [References](references.md)

## The central question

> **Does a shared memory layer across multiple agents help or hurt agentic coding on a team larger than one?**

We don't know yet. The optimistic case is that agents stop re-learning the same lessons, share conventions implicitly, and route around each other via locks — a true multiplier. The pessimistic case is **context pollution**: one agent's half-baked decision becomes another agent's "prior context," similarity hits drag conversations off-course, and the noise floor rises faster than signal.

## Things we're actively watching

- **Precision at the inject threshold.** The default cosine-distance cutoff in `ygg inject` is a guess. Too loose and unrelated sessions bleed into every prompt; too tight and useful context is invisible.
- **Pollution from bad turns.** Corrections ("no, don't do that") are themselves embedded as nodes. A naive similarity hit can resurface the *original mistake* as context. Digests partially mitigate this by summarizing resolved turns, but the raw nodes still exist.
- **Cross-agent interference.** Two agents working on unrelated features may still share enough technical vocabulary for their nodes to rank well against each other. Is that signal or noise?

## Directions under consideration

- **A lightweight classifier** (logistic / small NN over embedding + metadata) that scores *whether a similarity hit is actually useful*, trained on whether the agent who received it referenced it in its next turn. Would live between `ygg inject`'s pgvector query and the hook's stdout.
- **Periodic Claude-in-the-loop scoring.** Occasionally surface candidate injections to a supervising agent for thumbs-up/down, collecting labels for the classifier.
- **Kind-aware retrieval.** Downweight `tool_result` and pre-correction turns; prefer `digest` and `directive` nodes when available.
- **Hybrid retrieval (lexical + semantic).** Postgres gives us `tsvector` and trigram indexes alongside `pgvector`. Union the candidate sets — file-path literal matches *and* semantic neighbors — then re-rank. Often beats either mode alone, and slots in before the classifier in the pipeline. See [Retrieval and injection](retrieval.md).
- **Per-agent attention scopes.** Let an agent opt into a subset of siblings rather than the global pool.
- **Multi-level caching** (embedding → query → negative). Don't ask the same thing twice. Biggest deferrable win; see *Smarter, cheaper, fewer tokens* in [Design principles](design-principles.md).
- **Disclosure gate / habituation.** Track what's already been surfaced and suppress re-disclosure until a token-based cooldown elapses. See *Habituation* in [Design principles](design-principles.md).
- **Epoch reflections** at context thresholds, not only at `Stop`. Yields a richer ledger with less reliance on compaction timing.

If you run Yggdrasil on a team and have data — positive or negative — please open an issue. Negative results are as valuable as positive ones here.

## Known LLM failure modes Yggdrasil touches

Yggdrasil operates on the context window, which makes it directly implicated in several well-documented LLM pathologies. Worth stating explicitly what we're aware of, what we mitigate, and what we might make worse.

### Context rot

LLM performance degrades as context length grows: earlier instructions get diluted, attention spreads thin, and recency bias pulls the model toward whatever was said last regardless of importance. Empirically demonstrated by Liu et al. in *"Lost in the Middle"* ([arXiv:2307.03172](https://arxiv.org/abs/2307.03172)) for retrieval-augmented setups — relevant content placed in the middle of a long context is often ignored even when directly answer-bearing. Industry discussion under labels like "context rot" or "context decay" generalizes the observation to long-running agent sessions where drift and forgetting compound.

Yggdrasil's direct mitigations:

- **Digest on Stop.** Collapses a completed session into a compact summary node, so the *next* session primes on a short, dense representation instead of the full transcript.
- **Pressure telemetry.** `ygg prime` surfaces context usage; agents get a warning past 75% and can proactively digest or flush.
- **Prime-over-concat.** `SessionStart` injects a curated `prime` block instead of replaying the whole transcript.

Yggdrasil's direct risks:

- **Inject adds to the context window.** Every `[ygg memory | …]` line is tokens the model has to attend to. A noisy inject makes context rot *worse*, not better. Precision is not optional here.

### See-sawing / flip-flopping

When an agent is fed conflicting context — two `directive` nodes that disagree, a correction followed by a similarity hit resurfacing the original mistake, or successive prompts that push opposite directions — the model oscillates between positions on consecutive turns rather than resolving them. Loosely documented in practitioner reports; closely related to the *sycophancy* literature (Sharma et al., [arXiv:2310.13548](https://arxiv.org/abs/2310.13548)) where models flip to agree with whichever voice spoke most recently. Our naive similarity retrieval is a textbook way to trigger this: retrieve the pre-correction turn, re-inject it, watch the agent relitigate a settled point.

Yggdrasil's direct mitigations (in place or in progress):

- **Kind-aware retrieval** (roadmap): prefer `digest` and `directive` over raw `user_message`, since digests encode *resolved* state and directives encode *durable* rules.
- **Downweighting pre-correction nodes** (roadmap): the digest pipeline tags corrections; we should use that signal to suppress the superseded turn from similarity hits.
- **Time-decayed salience** (`src/salience.rs`): newer resolutions outrank older contradictions.

### Sycophancy drift

Models bias toward confirming whatever the user or injected context suggests, even when the signal is weak. Relevant because `ygg inject`'s hits are *presented as context*, not as hypotheses — an agent may treat a weak similarity hit as authoritative simply because it appeared. The same classifier idea that fights pollution fights this.

### Cross-agent contamination (Yggdrasil-specific)

Because our memory is global across repos and agents (see [ADR 0008](adr/0008-shared-db-across-repos.md)), a strong opinion formed in one project can echo into unrelated ones. Per-agent attention scopes (listed above as a direction under consideration) are the primary defense.

## The honest summary

Yggdrasil spends compute to push *against* context rot and in some configurations risks *amplifying* see-sawing. The classifier, digest quality, and threshold tuning are the three levers that decide which side wins — and we don't yet have data to say we're winning. This document exists so that when we (or anyone else running this) observe these failure modes, we have shared vocabulary for them.
