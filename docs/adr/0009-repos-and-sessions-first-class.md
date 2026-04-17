# 0009 — Repos and sessions as first-class dimensions

- **Status**: Accepted
- **Date**: 2026-04-17
- **Supersedes (partially)**: [ADR 0008](0008-shared-db-across-repos.md) — the "one global DB" part stands; the "basename of `pwd` as the sole identity" part is replaced here.

## Context

[ADR 0008](0008-shared-db-across-repos.md) committed to one Postgres across every repo a user works in, and to auto-keying agents by the basename of the current working directory. That was enough to bootstrap — it wasn't enough to build a beads-style issue tracker on.

Two concrete failures surfaced:

1. **Basename collisions.** `~/Documents/GitHub/foo/backend` and `~/Documents/GitHub/bar/backend` both resolve to the agent `backend`. Any retrieval scoped to "this repo" is impossible because the repo isn't a thing the schema knows about.
2. **Persona vs. workspace conflation.** "The developer who is working" and "the project being worked on" were the same column (`agent_name`). You couldn't ask "what has *this persona* learned across every repo?" separately from "what do we know about *this repo* regardless of who worked on it?"

For beads replacement (see [ADR 0010](0010-tasks-beads-replacement.md)) this is a hard blocker: beads issues belong to a project, and `(agent_name = basename(pwd))` has no clean place to put that association.

## Decision

Introduce two new first-class tables and thread them through the existing ones:

```
repos
  repo_id         UUID PK
  canonical_url   TEXT UNIQUE NULL    -- git remote.origin.url
  name            TEXT                 -- display name (derivable from URL or dir)
  task_prefix     TEXT UNIQUE          -- "yggdrasil", "route-53" — used for task IDs
  local_paths     TEXT[]               -- where this repo has been seen on this host
  metadata        JSONB
  created_at, updated_at

sessions
  session_id      UUID PK
  agent_id        UUID FK agents
  repo_id         UUID FK repos NULL   -- NULL for non-git / scratch sessions
  started_at, ended_at
  metadata        JSONB
```

Plus additive columns on existing tables:

```
nodes:  + session_id UUID FK NULL
        + repo_id    UUID FK NULL
```

Resolution order when a hook fires:
1. If inside a git work tree: `canonical_url` from `remote.origin.url`, fall back to the toplevel basename.
2. Else: the absolute path of the working directory is the repo's identity; `task_prefix` comes from slugifying the basename.
3. First time we see a repo, register it and allocate a `task_prefix`.
4. Subsequent sessions look the repo up and reuse the existing row (appending the local path to `local_paths` if it's a new location).

`agent_name` keeps its current meaning as a persona/workspace key, but it is no longer the thing that determines the project a task lives in. Default hook behavior stays as `basename(pwd)` for now — the change is additive, not breaking — but the design no longer assumes agent = repo.

### Alternatives considered

- **Switch agent to `git rev-parse --show-toplevel | basename`**. Fixes collisions. Doesn't disentangle persona from project. Half-measure.
- **Repo as just another tag on nodes**. Maximally flexible; no constraints; queries get messy; nowhere clean to hang task prefixes.
- **Abandon the basename default entirely.** Would break existing hook scripts and in-flight agents. Not worth the disruption for the incremental benefit.

## Consequences

**Positive**

- Repo is the natural scoping unit for issues (ADR 0010) and for per-repo retrieval filtering.
- Basename collisions go away (canonical URL wins; local paths accumulate harmlessly).
- Session boundaries become queryable — "what did this session cover?" is now a first-class question.
- Prime output can surface ready tasks for the current repo without guessing identity.

**Negative**

- Two new tables, two new FKs, one more migration to keep compatible.
- The hook layer now has to detect git context. Non-git directories work, but with reduced features (no canonical URL; task prefix derived from pwd basename).
- Backward compatibility: `nodes.session_id` and `nodes.repo_id` are nullable, so pre-migration nodes still validate. Queries that want repo-scoped answers have to tolerate the NULLs.
- Renamed repos (new remote URL) look like new repos today. Manual merge is possible via SQL; a `ygg repo alias` command is a reasonable future addition.

**Future triggers to revisit**

- If `basename`-only fallback keeps producing collisions for non-git work, fold the absolute path into the default identity.
- If multi-remote repos (gerrit-style) show up, generalize `canonical_url` into a small `repo_aliases` table.
- If we ever federate across machines, `repo_id` is the stable key that can cross host boundaries; `local_paths` is the dimension that can't.

## References

- Extends and partially supersedes [ADR 0008](0008-shared-db-across-repos.md).
- Consumed by [ADR 0010](0010-tasks-beads-replacement.md).
