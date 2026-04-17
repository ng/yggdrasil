# 0008 — One global database across repos; agents auto-keyed by working directory

- **Status**: Accepted
- **Date**: 2026-04-17

## Context

Beads and gastown are both **per-repo** — each repository has its own `.beads/` directory with its own Dolt database, its own issue namespace, its own sync story. That is the right call for issue tracking: issues are scoped to a project, and cross-project issue bleed would be noise, not signal.

Yggdrasil is not issue tracking. It is **agent memory and coordination**, and memory behaves differently from issues:

- The author works across many repositories in a day. Conventions learned in one project (how to handle migrations, how to write Rust async, which brew packages need `HOMEBREW_NO_AUTO_UPDATE`) apply to all of them.
- Similarity retrieval across the entire corpus is *the* primary value proposition. Siloing memory per repo collapses recall quality in proportion to how many repos the user works in.
- Locks are mostly intra-repo, but cross-repo cases exist (e.g. a shared monorepo tool, a credentials file at `~/.aws/credentials`, a global `Cargo` target cache) and benefit from a shared namespace.
- Digests — the "what we learned last session" summaries — compound best when they can cite nodes from anywhere.

## Decision

Yggdrasil uses **one Postgres database**, shared across every repo the user works in. Agents are auto-keyed by the basename of the current working directory:

```bash
AGENT="${YGG_AGENT_NAME:-$(basename "$(pwd)")}"
```

Working in `~/Documents/GitHub/yggdrasil` → agent `yggdrasil`. Working in `~/Documents/GitHub/route-53` → agent `route-53`. No manual setup, no per-repo init step for memory scoping — the hooks handle it.

Similarity queries during `ygg inject` scan the **global** node table, not a repo-scoped slice. The inject directive surfaces which agent a hit came from (`[ygg memory | route-53 | 1d ago | sim=48%]`) so the receiving agent can judge relevance, but the retrieval itself is unified.

### Alternatives considered

- **Per-repo database** (the beads/gastown pattern). Familiar, scopes cleanly, zero cross-project pollution. But cross-repo recall is impossible without a federation layer, and the operational cost of a Postgres instance per repo is absurd. Ruled out early.
- **One DB, but per-repo agent namespaces and no cross-repo similarity**. A halfway house: unified storage, isolated retrieval. Loses the cross-repo learning benefit we explicitly want.
- **One DB, with repo as a first-class dimension on queries**. What we effectively have via agent-name-as-basename. A future refinement: add a `repo_root` column and surface it in the dashboard. Not needed yet — the basename convention has been sufficient.
- **Per-user global DB with multi-tenant partitioning**. Relevant only if we ever host Yggdrasil. Today the DB is single-user and local.

## Consequences

**Positive**

- Zero-config cross-repo memory. The user works in a new repo for the first time; its hooks fire with a fresh agent name; prior conversations from *other* repos become retrievable immediately via similarity.
- Single operational surface. One `docker-compose up -d`, one backup, one migration path.
- Cross-agent locks are possible for truly shared resources (`~/.aws/credentials`, global dependency caches).
- The dashboard shows every agent the user has ever worked with — a useful reminder of what's in-flight.

**Negative**

- **Pollution risk is higher than per-repo**. A bad embedding from an irrelevant project can surface in the wrong context. This is the *central* open question driving the research direction in the README — see [Open questions](../../README.md#open-questions-this-is-partly-an-experiment).
- The basename-of-pwd heuristic collides when two repos share a directory name (e.g. `foo/backend` and `bar/backend`). Workaround today: set `YGG_AGENT_NAME` explicitly in the shell. A future revision should use the git toplevel basename or a content hash.
- Privacy: one DB means any agent session can retrieve any past session. Not a problem for a single-user local setup; would be a serious concern in a shared or hosted deployment.
- Backups are all-or-nothing; there's no "back up just this project's memory."

**Future triggers to revisit**

- If cross-repo pollution becomes the dominant complaint, the fix is *not* to silo per repo — it's to improve the relevance classifier (see README open questions). Siloing is a hammer; we want a scalpel.
- If we ever host Yggdrasil for teams, revisit schema-level or DB-level isolation per tenant.
- If `basename`-collisions become frequent, switch to `git rev-parse --show-toplevel | basename` or a canonical repo identifier.

## References

- [beads](https://github.com/steveyegge/beads) — the per-repo design this ADR explicitly diverges from, for sound reasons specific to issue tracking.
- Park et al., *Generative Agents* ([arXiv:2304.03442](https://arxiv.org/abs/2304.03442)). The memory-stream model is inherently global per-agent; Yggdrasil extends that one step further to global per-user.
- Bender et al., *On the Dangers of Stochastic Parrots* (2021). The pollution risk we're flagging is a specific instance of the broader critique — unfiltered retrieval from a mixed corpus can amplify noise. Worth keeping in mind.