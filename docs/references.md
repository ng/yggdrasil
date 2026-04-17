# References & inspirations

Yggdrasil stands on prior work. In rough order of influence.

## Tooling we compose with or draw directly from

- **[beads](https://github.com/steveyegge/beads)** — Steve Yegge's AI-native issue tracker. The imperative hook-primed agent workflow, the `prime` subcommand pattern, the instinct that agent UX lives in `CLAUDE.md`/`AGENTS.md` — all downstream of beads. Yggdrasil's `ygg prime` imperative output in particular is a direct lift of the technique. See [ADR 0006](adr/0006-dogfood-drop-beads.md) for our decision to dogfood Yggdrasil without beads in this repo.
- **[gastown](https://github.com/steveyegge/gastown)** — companion project for supervising agent *processes*. Informed our thinking on what Yggdrasil should *not* be (we're memory + coordination, not a process supervisor). *URL best-effort; please correct if wrong.*
- **[Dolt](https://github.com/dolthub/dolt)** — versioned SQL database. We considered Dolt as a substrate (see [ADR 0001](adr/0001-postgres-pgvector-single-store.md)) and may revisit if branchable DAG history becomes a requirement.
- **[pgvector](https://github.com/pgvector/pgvector)** — the Postgres vector extension we rely on for similarity search.
- **[Ollama](https://ollama.com/)** — local model runtime; provides our embeddings. See [ADR 0002](adr/0002-local-ollama-embeddings.md).
- **[Claude Code hooks](https://docs.anthropic.com/en/docs/claude-code/hooks)** — the integration surface Yggdrasil plugs into. See [ADR 0005](adr/0005-shell-hook-integration.md).
- **[tmux](https://github.com/tmux/tmux)** — the multiplexer everything runs inside. See [ADR 0007](adr/0007-tmux-as-substrate.md).

## Research on agent memory, retrieval, and coordination

The "does shared memory across agents help or hurt?" question in [Open questions](open-questions.md) is not a new one. Relevant literature:

- **Packer et al., *MemGPT: Towards LLMs as Operating Systems*** (2023). [arXiv:2310.08560](https://arxiv.org/abs/2310.08560). The digest/recall/paging model — promote salient turns, demote stale ones — directly inspired our Stop-hook digest design.
- **Park et al., *Generative Agents: Interactive Simulacra of Human Behavior*** (2023). [arXiv:2304.03442](https://arxiv.org/abs/2304.03442). The memory-stream + reflection pattern with recency/importance/relevance scoring is a close ancestor of what our inject threshold tuning wants to become.
- **Wang et al., *Voyager: An Open-Ended Embodied Agent with Large Language Models*** (2023). [arXiv:2305.16291](https://arxiv.org/abs/2305.16291). Voyager's growing skill library is an existence proof that accumulated agent memory *can* compound positively — a useful optimistic data point against the pollution-risk pessimistic case.
- **Malkov & Yashunin, *Efficient and Robust Approximate Nearest Neighbor Search Using Hierarchical Navigable Small World Graphs*** (2016). [arXiv:1603.09320](https://arxiv.org/abs/1603.09320). The HNSW algorithm behind our pgvector index.
- **Lewis et al., *Retrieval-Augmented Generation for Knowledge-Intensive NLP Tasks*** (2020). [arXiv:2005.11401](https://arxiv.org/abs/2005.11401). The foundational RAG work our `ygg inject` hook is a specialization of.
- **Wang et al., *MiniLM: Deep Self-Attention Distillation for Task-Agnostic Compression of Pre-Trained Transformers*** (2020). [arXiv:2002.10957](https://arxiv.org/abs/2002.10957). The distillation approach behind the MiniLM base.
- **Reimers & Gurevych, *Sentence-BERT*** (2019). [arXiv:1908.10084](https://arxiv.org/abs/1908.10084). The sentence-embedding training approach that produced `all-MiniLM-L6-v2`, the model we actually run.
- **Liu et al., *Lost in the Middle: How Language Models Use Long Contexts*** (2023). [arXiv:2307.03172](https://arxiv.org/abs/2307.03172). Foundational for why digest-and-prune matters more than big-context bragging rights — relevant content in the middle of a long window gets ignored. Directly motivates our digest pipeline.
- **Sharma et al., *Towards Understanding Sycophancy in Language Models*** (2023). [arXiv:2310.13548](https://arxiv.org/abs/2310.13548). Closest formal treatment of the "flip-flop to agree with latest input" pattern that a noisy memory layer can trigger.

If you think we've failed to credit prior art, please open an issue — missed citations are always corrected.
