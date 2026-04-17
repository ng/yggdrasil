# 0006 — Dogfood Yggdrasil; remove beads directives from this repo

- **Status**: Accepted
- **Date**: 2026-04-17

## Context

Yggdrasil began as a companion to [beads](https://github.com/gastownhall/beads) — the idea was "beads for tasks, Yggdrasil for agent coordination, they ride together." During development we installed beads' `bd prime` hooks and the standard beads directive block in `CLAUDE.md` / `AGENTS.md`.

Over several weeks of working on this repo, a pattern emerged: beads' directives were extremely effective at getting agents to actually *use* beads — the imperative prose ("Use `bd` for ALL task tracking", "SESSION CLOSE PROTOCOL", inline command examples) primed Claude to reach for `bd` unprompted. Yggdrasil's `prime` output was a passive status dashboard; agents rarely reached for `ygg lock` or `ygg spawn` on their own. The coordination layer we were building was present but invisible.

Two realizations:

1. The imperative-prose technique beads uses is *the* mechanism by which tool adoption happens in agent workflows. Status dashboards don't change behavior; rules do.
2. Running *both* directive blocks crowded the agent's context with competing instructions about where work lives ("use bd" vs. "use ygg"). As the authors of Yggdrasil, we want to feel our own tool's gaps first — those gaps are the roadmap.

## Decision

In **this repository only**, we remove all beads directives from `CLAUDE.md` and `AGENTS.md` and the `bd prime` hook from `.claude/settings.json`. We rewrite `ygg prime` output to be imperative, borrowing the technique from beads (lists "When to use ygg" rules, not just status). Intra-session task tracking falls back to Claude Code's native TaskCreate — Yggdrasil has no tasks table by design.

This is a dogfooding decision, not a repudiation of beads. Beads remains excellent for issue tracking in other repos; Yggdrasil composes with it there.

### Alternatives considered

- **Keep both beads and Yggdrasil directives**. What we had. Confused agents and masked Yggdrasil's behavioral gaps.
- **Add a `tasks` table to Yggdrasil**. Reproduce beads' best feature. Rejected for now — it would pull us off the coordination mission and duplicate something beads already does well. Native TaskCreate covers the intra-session case; cross-session task continuity is a real gap we may close later with a separate ADR.
- **Only rewrite `ygg prime`, leave the beads block alone**. Half-measure. The whole point is to experience Yggdrasil without beads' crutch.

## Consequences

**Positive**

- Yggdrasil's UX gaps hurt us first — we're now the primary signal for what's missing.
- The imperative `ygg prime` output measurably changes agent behavior (locks acquired, spawns used, memory hits read) — we validate this pattern as we go.
- Less competing context in every session.

**Negative**

- We lose beads' excellent dependency graph for this project's own backlog. Intra-session TaskCreate is a weaker replacement for multi-session work.
- Onboarding contributors who know beads will hit the missing tracker. We mitigate with a clear `CLAUDE.md` note.
- The memory layer (`[ygg memory | ...]` injections) now carries more load — prior-conversation recall has to substitute for some of what beads' issue trail provided.

**Future triggers to revisit**

- If we find ourselves reinventing beads' functionality inside `ygg` over time, we should either adopt beads back or formally absorb its design (with credit in a new ADR).
- If cross-session task continuity becomes the dominant pain point, file an ADR for a `tasks` table and build it.

## References

- [beads](https://github.com/steveyegge/beads) — the system whose directive technique we adopt and whose tracker we temporarily set aside.
- Beck, *Test-Driven Development: By Example* (2002). Dogfooding as a development discipline — use the tool you're building, on the tool you're building, so its gaps hurt you first.
