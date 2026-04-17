# 0001 — Postgres + pgvector as the single source of truth

- **Status**: Accepted
- **Date**: 2026-04-15

## Context

Yggdrasil needs to persist four interrelated concerns:

1. A DAG of conversation nodes with embeddings for similarity search.
2. Short-TTL resource leases with heartbeats and atomic acquire/release.
3. An agent state machine with referential integrity to head nodes and digests.
4. An append-only live event stream for `ygg logs --follow` and the TUI dashboard.

These are not independent — queries routinely join across them ("show me locks held by agents with recent digests containing corrections"). Splitting across data stores would force application-level joins, multi-store transactions, and divergent failure modes.

### Alternatives considered

- **SQLite + a separate vector index** (e.g. `sqlite-vss`, Qdrant, LanceDB). Simpler to run, but the lock contract wants `SELECT … FOR UPDATE`, strong isolation, and `NOTIFY` for live streams — none of which SQLite handles well under concurrent agent load.
- **Postgres + an external vector DB** (Qdrant, Weaviate, Pinecone). Two stores to deploy, two stores to back up, and cross-store transactions impossible. Similarity hits would need a second round-trip to hydrate node bodies. On the embedding workloads we care about, `pgvector` with HNSW is within an order of magnitude of these dedicated engines for our scale — and it's free, already deployed, and joins natively with the rest of our schema.
- **An embedded key-value store** (sled, RocksDB). Maximum performance, minimum ergonomics. No query language, no schema, no migrations story. Wrong trade for an early-stage project where the schema is still moving.
- **Beads on top of Dolt**. [beads](https://github.com/steveyegge/beads) inspired a lot of Yggdrasil's ergonomics — the hook-primed agent workflow with a durable backing store is directly downstream of it. But beads is a heavyweight *issue tracker* with its own opinions about work items, dependencies, and sync; its schema doesn't want to absorb embeddings, locks, or an event stream. [Dolt](https://github.com/dolthub/dolt) underneath beads gives versioned branches and merges, which is beautiful but solves a problem (history as a first-class object) we don't yet have. If we ever need branch/merge semantics on the DAG, wrapping Postgres via logical decoding or migrating to Dolt are both reasonable future moves — but today Dolt's write throughput and operational weight aren't worth the trade.

## Decision

One Postgres instance, with `pgvector` and `uuid-ossp` extensions, is the single source of truth. All of Yggdrasil's state lives in it. The vector index is an HNSW index on `nodes.embedding` with cosine-distance ops.

## Consequences

**Positive**

- One connection pool, one transaction boundary, one backup story.
- `pgvector` performance rivals dedicated vector databases at our scale, and it's free and already deployed.
- Similarity search joins agent metadata in a single query (see `SearchHit` in `src/models/node.rs`).
- `LISTEN`/`NOTIFY` gives us live event delivery without Redis or a message broker.
- `sqlx` compile-time-checked queries keep the schema honest.

**Negative**

- Postgres is a heavier dependency than an embedded DB. We mitigate with `docker-compose up -d`.
- `pgvector` HNSW index rebuilds can be slow on large inserts; we accept this because our write rate is agent-bounded, not user-bounded.
- Single-instance scaling ceiling exists. When we hit it, logical replication or read replicas are well-understood paths forward.

**Future triggers to revisit**

- If node volume exceeds ~10M, re-evaluate pgvector vs. a dedicated vector DB with a node-id cache.
- If we ever want multi-tenant hosted Yggdrasil, revisit isolation (schema-per-tenant vs. row-level).

## References

- [pgvector](https://github.com/pgvector/pgvector) — Postgres vector extension.
- Malkov & Yashunin, *Efficient and Robust Approximate Nearest Neighbor Search Using Hierarchical Navigable Small World Graphs*, [arXiv:1603.09320](https://arxiv.org/abs/1603.09320). The HNSW algorithm behind `pgvector`'s index.
- [Dolt](https://github.com/dolthub/dolt) — versioned SQL database (the substrate beads uses).
- [beads](https://github.com/steveyegge/beads) — issue tracker on Dolt.
