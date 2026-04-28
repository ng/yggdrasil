# Project Instructions for AI Agents

This project **is** Yggdrasil — a multi-agent coordination layer. We dogfood it: the `ygg` CLI and hooks installed in this repo coordinate the agents working on it. Refer to the project as **Yggdrasil** in prose; `ygg` is the command binary.

## Yggdrasil Agent Coordination

The SessionStart, UserPromptSubmit, Stop, PreCompact, and PreToolUse hooks are active. They auto-prime context, inject similar past nodes, digest transcripts, and track state in Postgres. You will see their output at the top of each session (`<!-- ygg:prime -->`) and above each user prompt (`[ygg memory | <agent> | <age> | sim=<n>%]`).

### Quick Reference

```bash
ygg task ready                              # Unblocked tasks in the current repo
ygg task list [--all] [--status <...>]      # All tasks in this repo (or everywhere)
ygg task create "title" --kind <task|bug|feature|chore|epic> --priority <0-4>
                                            # Priority: 0=critical 1=high 2=med 3=low 4=backlog.
                                            # Accepts "P0".."P4" too. NOT "high"/"medium"/"low".
ygg task claim <ref>                        # Take a task (assign + in_progress)
ygg task show <ref>                         # Full detail for <prefix>-NNN or UUID
ygg task close <ref> [--reason "..."]       # Complete a task
ygg task dep <task> <blocker>               # Record dependency
ygg remember "..."                          # Durable note; similarity retriever can surface later

ygg status                                  # See all agents' state, locks, recent activity
ygg lock acquire <resource-key>             # Lease a shared resource before editing
ygg lock release <resource-key>             # Release when done
ygg lock list                               # See outstanding locks
ygg spawn --task "..."                      # Spawn a parallel agent in a new tmux window
ygg interrupt take-over --agent <name>      # Take over / steer another agent
ygg logs --follow                           # Live event stream
```

### Rules

- **Before editing a resource another agent might touch** (shared file, branch, migration, config), acquire a lock: `ygg lock acquire <path-or-key>`. Release when done. This is Yggdrasil's core contract — bypassing it defeats the coordination layer we're building.
- **For parallel work** that warrants its own context window, prefer `ygg spawn` over the native Task/Agent tool. Spawned agents are tracked in the DB, get their own prime context, and participate in lock/memory coordination.
- **Read `[ygg memory | ...]` injections** at the top of each user turn. They are real context from prior conversations (same or other agents) surfaced by similarity. Treat as relevant unless the content clearly refutes that.
- **Before assuming you're alone**, check `ygg status`. Other agents may hold locks or be mid-task on related work.
- **Task tracking** — use `ygg task` for anything that outlives the current session: creating work, recording dependencies, claiming, closing. Intra-turn checklists can stay in native TaskCreate; cross-session work lives in `ygg task`.
- **Durable notes** — `ygg remember "..."` writes a directive node the similarity retriever will surface in future sessions (scoped to the current repo when detectable). Prefer this over scratch `.md` files.
- **Do NOT** use `bd` / beads. This project uses `ygg task` / `ygg remember` instead.

## Terse for AI-tracking fields

When writing content that only agents consume — `ygg task create`
titles/descriptions/acceptance/design/notes, `ygg remember`, `ygg memory
create` — be terse. Drop filler (really/just/basically/actually/very).
Drop articles (`a`/`an`/`the`) when meaning survives. Prefer one sentence
per field where content allows. **Preserve verbatim**: identifiers
(snake_case, CamelCase), paths, commands, numbers, URLs, and modal
keywords (always/never/must/should/cannot/don't/may/shall).

Does **NOT** apply to commit messages, PR descriptions, code comments,
or chat responses — those are human-facing and full fidelity is correct.

## Session Completion

This repo is **public**. Default to **PRs into `main`**, not direct pushes.

Work is NOT complete until your branch is pushed and (where applicable) a PR is open.

1. Run quality gates if code changed (`cargo test`, `cargo check --all-targets`, `cargo fmt --check`).
2. Release any locks you still hold (`ygg lock list` → `ygg lock release <key>`).
3. Branch + PR (the default):
   ```bash
   git checkout -b <topic-branch>
   git push -u origin <topic-branch>
   gh pr create --base main --fill          # see CONTRIBUTING.md for the body template
   ```
4. **Direct push to `main` is acceptable for trivial changes only**: typos, generated-artifact updates, single-file docs edits a maintainer would rubber-stamp. When in doubt, open a PR.
5. Reference any related tasks (`yggdrasil-NNN`) in the PR body so the rollup updates.

**Never** stop before the branch is pushed and the PR is open. **Never** say "ready to push when you are" — push the branch and open the PR yourself.

## Non-Interactive Shell Commands

Some systems alias `cp`/`mv`/`rm` to interactive mode which hangs agents. Use:

```bash
cp -f src dst     mv -f src dst     rm -f file     rm -rf dir     cp -rf src dst
# scp / ssh: -o BatchMode=yes         apt-get: -y         brew: HOMEBREW_NO_AUTO_UPDATE=1
```

## Build & Test

```bash
cargo build --release        # Build the ygg binary
cargo test                   # Run tests (requires Postgres via docker-compose)
docker-compose up -d         # Start Postgres + pgvector
ygg migrate                  # Run migrations
make install                 # Build + install to ~/.local/bin/ygg + verify
make reinstall               # Re-sign + verify the existing install (recovery)
```

**macOS install gotcha:** `cp -f target/release/ygg ~/.local/bin/ygg` over a
running binary invalidates the Gatekeeper / codesign cache. The first
invocation after that **silently SIGKILLs** (no error, just exit 137 / "ygg
status" hangs). Use `make install` — it copies to a sibling tmp path, atomic
`mv`, re-signs ad-hoc, and runs a 5s `--version` smoke. If you already hit
this and the installed binary hangs, run `make reinstall` to re-sign without
rebuilding.

**DB pool sizing:** `YGG_DB_POOL` overrides the sqlx pool's
`max_connections` (default 32). Bump it when running fleets >50 agents;
the scheduler, watcher, TUI, and per-agent hooks all draw from this
pool, and connection-wait latency manifests as spurious tick lag.

## Architecture Overview

- **src/models/**: `agent` (state machine), `node` (DAG ledger with embeddings), `event` (live stream).
- **src/cli/**: one file per subcommand — `prime`, `inject`, `spawn`, `lock`, `interrupt`, `digest`, `status`, `logs`, `observe`.
- **src/lock.rs, src/pressure.rs, src/salience.rs, src/embed.rs**: coordination primitives.
- **migrations/**: Postgres schema with `pgvector` + `uuid-ossp`.
- **Hooks** in `~/.claude/ygg-hooks/` → call `ygg` subcommands at Claude Code lifecycle events.

<!-- BEGIN YGG INTEGRATION v:3 hash:1628b57b -->
## Yggdrasil Agent Coordination

This project uses **Yggdrasil** (`ygg`) for cross-session memory, resource
coordination, and issue tracking. The SessionStart, UserPromptSubmit, Stop,
PreCompact, and PreToolUse hooks are active — they auto-prime context, inject
similar past nodes, digest transcripts, and track state in Postgres. You will
see their output at the top of each session (`<!-- ygg:prime -->`) and above
each user prompt (`[ygg memory | <agent> | <age> | sim=<n>%]`).

### Quick Reference

```bash
ygg task ready                              # Unblocked tasks in the current repo
ygg task list [--all] [--status <...>]      # All tasks in this repo (or everywhere)
ygg task create "title" --kind <k> --priority <0-4>   # See priority/kind values below
ygg task claim <ref>                        # Take a task (assign + in_progress)
ygg task show <ref>                         # Full detail for <prefix>-NNN or UUID
ygg task close <ref> [--reason "..."]       # Complete a task
ygg task dep <task> <blocker>               # Record dependency
ygg remember "..."                          # Durable note; similarity retriever can surface later
```

### Task field values (important — no guessing)

- `--priority <0..4>` — **0 = critical, 1 = high, 2 = medium, 3 = low, 4 = backlog**.
  Also accepts `P0`..`P4`. Do NOT pass strings like "high" / "medium" / "low".
- `--kind <task|bug|feature|chore|epic>` — one of these five. Default is `task`.
- `--status <open|in_progress|blocked|closed>` — for filtering / transitions.
- `--label <a,b,c>` — comma-separated labels. Repeatable.
- `<ref>` is either `<prefix>-<N>` (e.g. `yggdrasil-42`) or a UUID.

### Ticket body structure

Tickets are read by other agents picking up the work. Bodies have **four
sections in this order**, separated by blank lines. No PR-prose walls.

1. **Why** — one sentence. The trigger or observation that justifies the
   work. Cite the source: `Adversarial review:`, `Codebase audit:`,
   `Bench scenario X:`, `Research thread Y:`, `Incident on <date>:`.
2. **What** — one sentence. The concrete change. Use imperative voice.
3. **Acceptance:** — a bulleted list of testable conditions. Each bullet
   is something an autonomous agent can verify when claiming the task as
   done. Avoid vague verbs ("improve", "consider"); pin SHAs, file paths,
   commands, numeric thresholds.
4. **Refs:** *(optional)* — research thread tag, related ticket
   (`yggdrasil-NN`), external URL, ADR number.

Example:

```text
Adversarial review: src/db.rs max_connections(10) starves a fleet of
50+ active agents.

Bump default to 32 and accept YGG_DB_POOL env override.

Acceptance:
- src/db.rs default = 32; YGG_DB_POOL parses to u32, falls back on error
- CLAUDE.md documents the knob in the Build & Test section
- cargo check --all-targets clean

Refs: yggdrasil-141, adversarial-review note 2026-04-23
```

### Terse for AI-tracking fields

When writing content that only agents consume — `ygg task create`
titles/descriptions/acceptance/design/notes, `ygg remember`,
`ygg memory create` — be terse. Drop filler (really/just/basically/
actually/very). Drop articles (`a`/`an`/`the`) when meaning survives.
Prefer one sentence per field where content allows. **Preserve
verbatim**: identifiers (snake_case, CamelCase), paths, commands,
numbers, URLs, and modal keywords (always/never/must/should/cannot/
don't/may/shall).

Does **NOT** apply to commit messages, PR descriptions, code comments,
or chat responses — those are human-facing and full fidelity is correct.

Example:
```bash
ygg task create "fix migration ordering" --kind bug --priority 1 --label migrations,sqlx

ygg status                                  # See all agents' state, locks, recent activity
ygg lock acquire <resource-key>             # Lease a shared resource before editing
ygg lock release <resource-key>             # Release when done
ygg lock list                               # See outstanding locks
ygg spawn --task "..."                      # Spawn a parallel agent in a new tmux window
ygg interrupt take-over --agent <name>      # Take over / steer another agent
ygg logs --follow                           # Live event stream
```

### Rules

- **Before editing a resource another agent might touch** (shared file, branch, migration, config), acquire a lock: `ygg lock acquire <path-or-key>`. Release when done. This is Yggdrasil's core contract — bypassing it defeats the coordination layer.
- **For parallel work** that warrants its own context window, prefer `ygg spawn` over the native Task/Agent tool. Spawned agents are tracked in the DB, get their own prime context, and participate in lock/memory coordination.
- **Read `[ygg memory | ...]` injections** at the top of each user turn. They are real context from prior conversations (same or other agents) surfaced by similarity. Treat as relevant unless the content clearly refutes that.
- **Before assuming you're alone**, check `ygg status`. Other agents may hold locks or be mid-task on related work.
- **Task tracking** — use `ygg task` for anything that outlives the current session: creating work, recording dependencies, claiming, closing. Intra-turn checklists can stay in a native TodoList; cross-session work lives in `ygg task`.
- **Durable notes** — `ygg remember "..."` writes a directive node the similarity retriever will surface in future sessions (scoped to the current repo when detectable). Prefer this over scratch `.md` files.
- **Do NOT** use `bd` / beads. This project uses `ygg task` / `ygg remember` instead.

### Vector memory: how to actually use it

Yggdrasil stores embeddings on tasks, memories, learnings, and recent
conversation nodes. The retriever auto-surfaces relevant ones as
`[ygg memory | ...]` lines above each user prompt — that's the
**passive** path. There are four **active** patterns you should reach
for explicitly:

1. **Before `ygg task create`** → run `ygg task dupes --limit 5` (or
   `--all` for cross-repo). If any pair surfaces above sim ≈ 0.85,
   prefer claiming/extending the existing task over filing a new one.
   Keeps the corpus from accreting near-duplicates.

2. **Before tackling a hard problem** → `ygg memory search "<topic>"`
   returns top hits ranked by cosine similarity across global / repo /
   session memories. If a directive landed last week that applies, the
   retriever finds it; you avoid re-solving.

3. **When you discover a non-obvious rule** → `ygg remember "<rule>"`.
   One-sentence directive. Preserve identifiers, paths, modal verbs
   verbatim. Examples that earn their keep:
   - `ygg remember "always rebuild after src/db.rs edits — sqlx caches the schema"`
   - `ygg remember "scheduler retries fire AFTER finalize, not before — see PR #112"`

4. **For engineering corrections** → `ygg learn add` with a file glob.
   Unlike `remember`, learnings re-fire deterministically when an agent
   touches a matching file. Use for "every time someone edits X, also
   check Y" rules.

**Anti-patterns** (don't write these — they pollute the retriever):
- Narration of what you just did ("I refactored foo.rs"). The PR/commit
  carries that.
- Per-task scratch ("trying option A first"). Use a TodoList.
- Speculation ("might want to revisit this"). Wait until it matters.

`ygg trace` shows what the retriever actually surfaced for the last
turn — useful when an injection looks off; tells you whether the
problem is in the corpus, the embedder, or the scoring.

## Session Completion

Work is NOT complete until `git push` succeeds.

1. Run quality gates if code changed (tests, linters, build/type-check).
2. Release any locks you still hold (`ygg lock list` → `ygg lock release <key>`).
3. Push:
   ```bash
   git pull --rebase
   git push
   git status  # MUST show "up to date with origin"
   ```
4. If push fails, resolve and retry until it succeeds.

**Never** stop before pushing; **never** say "ready to push when you are" — you push.

## Non-Interactive Shell Commands

Some systems alias `cp`/`mv`/`rm` to interactive mode which hangs agents. Use:

```bash
cp -f src dst     mv -f src dst     rm -f file     rm -rf dir     cp -rf src dst
# scp / ssh: -o BatchMode=yes         apt-get: -y         brew: HOMEBREW_NO_AUTO_UPDATE=1
```
<!-- END YGG INTEGRATION -->
