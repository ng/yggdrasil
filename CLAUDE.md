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

## Architecture Overview

- **src/models/**: `agent` (state machine), `node` (DAG ledger with embeddings), `event` (live stream).
- **src/cli/**: one file per subcommand — `prime`, `inject`, `spawn`, `lock`, `interrupt`, `digest`, `status`, `logs`, `observe`.
- **src/lock.rs, src/pressure.rs, src/salience.rs, src/embed.rs**: coordination primitives.
- **migrations/**: Postgres schema with `pgvector` + `uuid-ossp`.
- **Hooks** in `~/.claude/ygg-hooks/` → call `ygg` subcommands at Claude Code lifecycle events.

<!-- BEGIN YGG INTEGRATION v:1 hash:78de7785 -->
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

- **Before editing a resource another agent might touch** (shared file, branch, migration, config), acquire a lock: `ygg lock acquire <path-or-key>`. Release when done. This is Yggdrasil's core contract — bypassing it defeats the coordination layer.
- **For parallel work** that warrants its own context window, prefer `ygg spawn` over the native Task/Agent tool. Spawned agents are tracked in the DB, get their own prime context, and participate in lock/memory coordination.
- **Read `[ygg memory | ...]` injections** at the top of each user turn. They are real context from prior conversations (same or other agents) surfaced by similarity. Treat as relevant unless the content clearly refutes that.
- **Before assuming you're alone**, check `ygg status`. Other agents may hold locks or be mid-task on related work.
- **Task tracking** — use `ygg task` for anything that outlives the current session: creating work, recording dependencies, claiming, closing. Intra-turn checklists can stay in a native TodoList; cross-session work lives in `ygg task`.
- **Durable notes** — `ygg remember "..."` writes a directive node the similarity retriever will surface in future sessions (scoped to the current repo when detectable). Prefer this over scratch `.md` files.
- **Do NOT** use `bd` / beads. This project uses `ygg task` / `ygg remember` instead.

## Session Completion (managed-block duplicate — see canonical block above)

The earlier "Session Completion" section is canonical. The managed
integration block keeps the rules in sync for downstream repos that
install `ygg integrate`. PR-into-main is the default; direct push to
main only for trivial changes.

## Non-Interactive Shell Commands

Some systems alias `cp`/`mv`/`rm` to interactive mode which hangs agents. Use:

```bash
cp -f src dst     mv -f src dst     rm -f file     rm -rf dir     cp -rf src dst
# scp / ssh: -o BatchMode=yes         apt-get: -y         brew: HOMEBREW_NO_AUTO_UPDATE=1
```
<!-- END YGG INTEGRATION -->
