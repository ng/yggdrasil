# 0002 — Local Ollama embeddings over API-hosted models

- **Status**: Accepted
- **Date**: 2026-04-15

## Context

Yggdrasil embeds every conversation node — user messages, assistant messages, tool calls and results, digests — to support similarity search. At steady state that's hundreds of embeddings per agent per hour, and many thousands across a dogfooding session.

Two pressures push against each other:

1. **Quality**: large frontier embedding models (OpenAI `text-embedding-3-large`, Voyage, Cohere) produce noticeably better retrieval on technical prose.
2. **Privacy and cost**: every user prompt and tool result passes through the embed pipeline. Shipping them to an external API means handing a vendor the full, unredacted content of every agent conversation, plus a per-call bill that scales with session density.

## Decision

Use **local Ollama** as the embedding provider, with a small general-purpose sentence-embedding model — currently `all-minilm` (all-MiniLM-L6-v2), 384 dimensions, ~22M parameters. The `src/embed.rs` client speaks HTTP to `localhost:11434`; `docker-compose.yml` runs Ollama next to Postgres; `ygg init` pulls the model on first run.

### Alternatives considered

- **OpenAI / Voyage / Cohere embedding APIs**. Best quality, but data-exfiltration by design, rate limits block burst ingest, and cost scales with dogfooding intensity — exactly the workload we want to grow.
- **In-process embeddings via ONNX Runtime or Candle**. No extra process to run, but pulls a multi-hundred-MB dependency into the ygg binary, complicates builds across platforms, and blocks the async runtime on CPU-heavy inference. Ollama is a cleaner process boundary.
- **Skip embeddings entirely, use keyword search (tsvector)**. Cheap but useless for the actual retrieval patterns we care about — "prior context semantically similar to this user prompt" is not a keyword query.

## Consequences

**Positive**

- Zero data egress. Every agent's transcript stays on the host.
- No per-call billing. Dogfood as hard as we want.
- Fixed 384-dim vector keeps `pgvector` HNSW indexes compact.
- Process isolation — Ollama crashes don't take down `ygg`.

**Negative**

- Retrieval quality is lower than frontier models. We compensate by tuning the similarity threshold per hook (`inject` is strict; dashboard browse is loose).
- Ollama must be running for ingest to succeed. The embed pipeline degrades gracefully — missing embeddings don't block node writes, but similarity queries miss them until a backfill job runs.
- GPU availability matters on high-volume hosts. On CPU-only boxes, an `ygg observe` replay on a long transcript can take minutes. Acceptable today.

**Future triggers to revisit**

- If retrieval quality becomes the dominant complaint, evaluate a self-hosted frontier model (e.g. `bge-large`) before moving to an API.
- If we ever offer a hosted Yggdrasil, the privacy equation changes and an API provider with strong contractual guarantees becomes viable.

## References

- [Ollama](https://ollama.com/) — local model runtime.
- Wang et al., *MiniLM: Deep Self-Attention Distillation for Task-Agnostic Compression of Pre-Trained Transformers*, [arXiv:2002.10957](https://arxiv.org/abs/2002.10957). The base architecture.
- Reimers & Gurevych, *Sentence-BERT: Sentence Embeddings using Siamese BERT-Networks*, [arXiv:1908.10084](https://arxiv.org/abs/1908.10084). The sentence-embedding training approach that produced `all-MiniLM-L6-v2`.
- Lewis et al., *Retrieval-Augmented Generation for Knowledge-Intensive NLP Tasks*, [arXiv:2005.11401](https://arxiv.org/abs/2005.11401). Foundational RAG work our inject hook specializes.
- [MTEB leaderboard](https://huggingface.co/spaces/mteb/leaderboard) — empirical basis for the quality gap between local and frontier embedding models.
