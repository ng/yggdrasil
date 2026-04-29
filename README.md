# Yggdrasil

Yggdrasil is a multi-agent coordination layer for AI coding agents. It gives fleets of Claude Code instances (or any CLI-driven agent) the infrastructure they need to work in the same codebase without colliding: cross-session memory with vector-indexed retrieval, resource locking, task tracking with dependency graphs, a real-time TUI dashboard, scheduled task execution, and inter-agent messaging. Built in Rust, backed by PostgreSQL + pgvector. The CLI binary is `ygg`.

---

## Install

**From source** (requires Rust 1.75+):

```bash
cargo install --path .
```

**From GitHub releases:** coming soon.

**Homebrew:** coming soon.

## Requirements

- **Rust 1.75+** (build from source only)
- **PostgreSQL 14+** with the [pgvector](https://github.com/pgvector/pgvector) extension
- **Ollama** (local embedding model for vector memory)

## Quick Start

```bash
# 1. Bootstrap everything: Postgres check, Ollama model pull, migrations, hooks
ygg init

# 2. Create a task
ygg task create "my first task" --kind task --priority 2

# 3. Spawn a Claude Code agent to work on it
ygg spawn --task "do something"

# 4. Open the TUI dashboard to watch your fleet
ygg dashboard

# 5. Check fleet state from the command line
ygg status
```

## Architecture

```text
+------------------+         +-------------------------------+
|   Claude Code    |         |    PostgreSQL + pgvector       |
|                  |         |                               |
|  SessionStart  --+--ygg-->|  agents   (state machine)     |
|  UserPromptSubmit+-prime-->|  nodes    (DAG + embeddings)  |
|  Stop          --+-inject->|  events   (live stream)       |
|  PreCompact    --+-digest->|  locks    (semantic leases)   |
|  PreToolUse    --+--lock-->|  tasks    (tracking + deps)   |
+------------------+         +-------------------------------+
        |                                |
        v                                v
+------------------+         +------------------+
|  Ollama (local)  |         |   TUI Dashboard  |
|  embedding model |         |   (ratatui)      |
+------------------+         +------------------+
```

Hooks in `~/.claude/ygg-hooks/` fire at Claude Code lifecycle events and shell out to `ygg` subcommands:

- **SessionStart / PreCompact** -> `ygg prime` -- emits agent context as markdown
- **UserPromptSubmit** -> `ygg inject` -- writes a prompt node, retrieves similar context by embedding
- **Stop** -> `ygg digest` -- extracts corrections and sentiment into a digest node
- **PreToolUse** -> `ygg lock` / `ygg agent-tool` -- enforces resource leases, records tool usage

There is no long-running daemon other than the optional `ygg watcher` (heartbeats, lock expiry, digest triggers). Everything else runs as one-shot CLI invocations.

## Why Yggdrasil Exists

Running one agent in a terminal is easy. Running three to seven is taxing but common. Beyond that, things break: too many windows to watch, too much overlap on shared files, too much context lost to compaction, too much prior conversation that never resurfaces.

Yggdrasil focuses on the parts that are hard to get in one place:

- A shared **lock graph** so agents don't clobber each other mid-edit.
- A **memory layer** that surfaces prior-conversation context by similarity -- across sessions and across repositories.
- **Context-pressure telemetry** that fires a digest before compaction.
- **Live event streams** for humans watching multiple agents at once.

One deliberate design choice: Yggdrasil is **global per user**, not per repo. One Postgres instance backs every repo you work in; agents are auto-keyed by the basename of the current working directory. Foundational knowledge is shared, not isolated. The trade-off is pollution risk from cross-repo hits. See [ADR 0008](docs/adr/0008-shared-db-across-repos.md) and [Open questions](docs/open-questions.md).

## Subcommand Reference

| Command     | Purpose                                                                 |
|-------------|-------------------------------------------------------------------------|
| `init`      | Bootstrap: Postgres check, Ollama model pull, migrations, hooks.       |
| `up`        | Launch the tmux dashboard (default when run bare).                     |
| `dashboard` | Launch the TUI dashboard directly.                                      |
| `status`    | Quick text output of agent + system state.                              |
| `migrate`   | Run database migrations.                                                |
| `spawn`     | Spawn a new agent in a tmux window, registered in the DB.               |
| `run`       | Start an agent run loop.                                                |
| `task`      | Task tracking: `create / list / ready / claim / close / dep / show`.    |
| `memory`    | Scoped memories: `create / list / search / pin / unpin / expire / delete`. |
| `remember`  | Write a directive node (shorthand for `memory create --scope repo`).    |
| `lock`      | Acquire / release / list / heartbeat resource locks.                    |
| `inject`    | Hook: writes prompt node, emits similar-context directives.             |
| `prime`     | Hook: emits agent context as Markdown.                                  |
| `digest`    | Hook: extracts corrections/sentiment into a digest node.                |
| `observe`   | Ingest an existing Claude Code session transcript.                      |
| `interrupt` | Human overrides: take-over, pause, resume.                              |
| `logs`      | Live event stream (stdout).                                             |
| `watcher`   | Background daemon: heartbeats, lock expiry, digest triggers.            |
| `recover`   | Recover orphaned agents stuck in active states.                         |
| `rollup`    | Per-repo activity summary over a time window.                           |
| `reap`      | Purge stale locks / sessions / memories. Safe to cron.                  |
| `trace`     | Per-turn pipeline inspection: embed, retrieve, score, emit.             |
| `eval`      | Retrieval effectiveness summary (hit rate, cache hit %).                |
| `forget`    | Retroactive redaction of a node or pattern from history.                |
| `bar`       | Claude Code statusline generator (context pressure, cache rate, spend). |
| `agent-tool`| Hook: record the tool an agent is about to call.                        |

## Project Layout

```text
src/
  cli/          one file per subcommand
  models/       agent, node, event -- sqlx types + repos
  analytics/    similarity, pressure, salience aggregates
  stats/        token accounting, telemetry
  tui/          dashboard views (ratatui)
  config.rs     env loading
  db.rs         sqlx pool + migrations runner
  embed.rs      Ollama HTTP client
  executor.rs   agent run loop
  interrupt.rs  human-override primitives
  lock.rs       LockManager -- acquire/release/heartbeat
  ollama.rs     embedding model interface
  pressure.rs   context-pressure estimation
  salience.rs   memory ranking
  status.rs     status aggregation
  tmux.rs       tmux window management
  watcher.rs    background daemon
migrations/     Postgres schema
docs/           prose docs + ADRs
```

## Build from Source

```bash
docker-compose up -d             # Postgres 16 + pgvector, Ollama
cargo build --release            # build the ygg binary
cargo test                       # run tests (requires Postgres)
make install                     # build + install to ~/.local/bin/ygg
```

## Further Reading

- [Orchestration runtime](docs/orchestration.md) -- scheduler, task runs, payload flow, lock integration, failure semantics.
- [Retrieval and injection](docs/retrieval.md) -- why embeddings, what gets injected, sequence diagrams.
- [Eval benchmarks](docs/eval-benchmarks.md) -- `ygg bench` scenarios, Tier-A metrics, METR-style methodology.
- [Design principles](docs/design-principles.md) -- substrate separation, cache taxonomy, forgetting.
- [Open questions](docs/open-questions.md) -- the shared-memory hypothesis, named LLM failure modes.
- [Architecture Decision Records](docs/adr/) -- one ADR per non-obvious design choice.

## License

MIT.
