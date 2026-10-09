# Yggdrasil

Yggdrasil is a multi-agent coordination layer for AI coding agents. It gives fleets of Claude Code instances (or any CLI-driven agent) the infrastructure they need to work in the same codebase without colliding: resource locking, task tracking with dependency graphs, a scheduler for autonomous task execution, a real-time TUI dashboard, and inter-agent messaging. Built in Rust, backed by PostgreSQL. The CLI binary is `ygg`.

> **Knowledge storage:** ADR 0015 removed embeddings, pgvector and similarity retrieval. `remember` and `learn` provide scoped notes and deterministic rules. The compatibility implementation supports explicit migration to authoritative Open Knowledge Format (OKF) files; existing SQL knowledge stays selected until that migration. [Implementation and release status](docs/managed-postgres-okf-status.md).

---

## Install

**From source** (use the stable Rust toolchain, as CI does):

```bash
cargo install --path .
```

**From GitHub releases:** coming soon.

Native CI produces [online and offline candidate bundles](src/db/README.md#native-onlineoffline-bundles)
for macOS arm64/Intel and GNU Linux x86_64. Public release, macOS quarantine and
clean-machine dependency qualification remain open.

**Homebrew:** coming soon.

## Requirements

- **Stable Rust** (build from source only; the crate uses edition 2024).
- **PostgreSQL:** managed mode installs pinned PostgreSQL 16.15 during `ygg init`;
  external mode uses your configured server. CI validates external PostgreSQL 16
  and 18. Managed candidate platforms are macOS arm64/Intel and GNU Linux x86_64;
  other targets must use external mode.

## Quick Start

```bash
# 1. Initialize managed Postgres, or use your existing external configuration
ygg init

# 2. Create a task
ygg task create "my first task" --kind task --priority 2

# 3. Spawn an agent to work on it
ygg spawn --task "do something"

# 4. Open the TUI dashboard to watch your fleet
ygg dashboard

# 5. Check fleet state from the command line
ygg status

# Optional: one-line status for Codex prompts, hooks, or panes
ygg status --format codex
```

An existing `DATABASE_URL` keeps external mode selected. Without a URL or explicit
mode, `ygg init` installs and initializes managed PostgreSQL. Explicit managed
mode plus an external URL is a configuration error. Connection failures never
switch databases. Hooks may start an initialized cluster but never download or
upgrade it.

User configuration belongs in `~/.config/ygg/config.toml` and the legacy user
`.env`, with `YGG_CONFIG_DIR` or `XDG_CONFIG_HOME` overrides. Repository `.env`
files are not loaded. Start from [config.example.toml](config.example.toml) or
[.env.example](.env.example); environment values override equivalent settings.
Keep database data and knowledge outside worktrees. See the
[database operator guide](src/db/README.md) for lifecycle, TLS, owner credentials,
combined backups and validated deployment moves, and the
[knowledge guide](src/knowledge/README.md) before cutover or rollback.

## Architecture

```text
+------------------+         +-------------------------------+
| Agent CLI        |         |          PostgreSQL           |
| providers        |         |                               |
|                  |         |  agents   (state machine)     |
| Claude Code      |--hooks->|  events   (live stream)       |
| Codex CLI        |--hooks->|  locks    (semantic leases)   |
|                  |         |  tasks    (tracking + deps)   |
|                  |--ygg--->|  task_runs(scheduler runs)    |
+------------------+         +-------------------------------+
        |                                |
        v                                v
   tmux windows                 +------------------+
   (one per agent)              |   TUI Dashboard  |
                                |   (ratatui)      |
                                +------------------+
```

Hooks (installed by `ygg init` as native `ygg hook <event>` handlers) currently
fire at Claude Code lifecycle events. Codex CLI integration is being added at
the same hook boundary; see [docs/codex-integration.md](docs/codex-integration.md).

- **SessionStart / PreCompact** -> `ygg prime` -- emits agent context as markdown
- **UserPromptSubmit** -> delivers unread agent-to-agent messages, records token stats
- **Stop** -> `ygg run capture-outcome` + `ygg stop-check` -- records task-run outcome, blocks premature worker exit
- **PreToolUse** -> `ygg lock` / `ygg agent-tool` -- enforces resource leases, records tool usage

Managed database installations also run `ygg db serve` to keep PostgreSQL alive between CLI invocations. The optional `ygg watcher` handles heartbeats and lock expiry; `ygg scheduler` dispatches work. External PostgreSQL remains operator-managed.

## Why Yggdrasil Exists

Running one agent in a terminal is easy. Running three to seven is taxing but common. Beyond that, things break: too many windows to watch, too much overlap on shared files, too much context lost to compaction, too much prior conversation that never resurfaces.

Yggdrasil focuses on the parts that are hard to get in one place:

- A shared **lock graph** so agents don't clobber each other mid-edit.
- **Task tracking** with a dependency DAG, epic rollups, and a scheduler that dispatches ready tasks to spawned agents.
- **Inter-agent messaging** delivered at the recipient's next turn.
- **Live event streams** and a TUI dashboard for humans watching multiple agents at once.

One deliberate design choice: Yggdrasil is **global per user**, not per repo. One Postgres instance backs every repo you work in; agents are auto-keyed by the basename of the current working directory. The trade-off is documented in [ADR 0008](docs/adr/0008-shared-db-across-repos.md) and [Open questions](docs/open-questions.md).

## Subcommand Reference

| Command     | Purpose                                                                 |
|-------------|-------------------------------------------------------------------------|
| `init`      | Initialize managed Postgres or check external access, migrate, install hooks.                          |
| `up`        | Launch the tmux dashboard (default when run bare).                     |
| `dashboard` | Launch the TUI dashboard directly.                                      |
| `status`    | Quick text output of agent + system state; `--format codex` emits one line. |
| `db`        | Database lifecycle, diagnostics, combined backup, validated restore, deployment switching and offline verification; [operator guide](src/db/README.md#combined-operator-backups). |
| `migrate`   | Run database migrations.                                                |
| `spawn`     | Spawn a new agent in a tmux window, registered in the DB.               |
| `task`      | Task tracking: `create / list / ready / claim / close / dep / show / dupes`. |
| `run`       | Task-run lifecycle: `claim / heartbeat / finalize / show / list / capture-outcome`. |
| `scheduler` | Autonomous task-DAG scheduler: `run / tick / status / dry-run / backfill`. |
| `lock`      | Acquire / release / list / heartbeat resource locks.                    |
| `learn`     | Scoped learnings: deterministic rule capture matched by file glob.      |
| `remember`  | Create, list and delete scoped notes.                                  |
| `knowledge` | Browse OKF, inspect migration, explicitly cut over/roll back private knowledge, and recover shared drafts. |
| `prime`     | Hook: emits agent context as Markdown.                                  |
| `msg`/`chat`| Agent-to-agent messaging on the events bus.                             |
| `interrupt` | Human overrides: take-over, hand-back.                                  |
| `logs`      | Live event stream (stdout).                                             |
| `watcher`   | Background daemon: heartbeats, lock expiry.                             |
| `recover`   | Recover orphaned agents stuck in active states.                         |
| `rollup`    | Per-repo activity summary over a time window.                           |
| `reap`      | Purge stale locks / sessions. Safe to cron.                             |
| `bar`       | Claude Code statusline generator (context pressure, cache rate, spend). |
| `agent-tool`| Hook: record the tool an agent is about to call.                        |
| `hook`      | Native Claude Code lifecycle hook handlers.                             |

Codex CLI notes, including a sample `[tui].status_line` and the compact
`ygg status --format codex` output, live in
[docs/codex-integration.md](docs/codex-integration.md).

## Project Layout

```text
src/
  cli/          one file per subcommand
  models/       agent, event, task, task_run -- sqlx types + repos
  stats/        token accounting, telemetry
  tui/          dashboard views (ratatui)
  config.rs     env loading
  db.rs         sqlx pool + migrations runner
  executor.rs   RTK-proxied command execution
  interrupt.rs  human-override primitives
  lock.rs       LockManager -- acquire/release/heartbeat
  scheduler.rs  autonomous task-DAG scheduler
  status.rs     status aggregation
  tmux.rs       tmux window management
  watcher.rs    background daemon
migrations/     Postgres schema
docs/           prose docs + ADRs
```

## Build from Source

```bash
cargo build --release            # build the ygg binary
cargo test --lib                  # database-independent library tests
# Full integration suite: use only an isolated test database (see CONTRIBUTING.md)
make install                     # build + install to ~/.local/bin/ygg
```

## Further Reading

- [Orchestration runtime](docs/orchestration.md) -- scheduler, task runs, payload flow, lock integration, failure semantics.
- [Eval benchmarks](docs/eval-benchmarks.md) -- `ygg bench` scenarios, Tier-A metrics, METR-style methodology.
- [ADR 0015](docs/adr/0015-retrieval-scope-reduction.md) -- why the retrieval/embedding layer was removed.
- [Managed PostgreSQL and OKF status](docs/managed-postgres-okf-status.md) -- milestone evidence and remaining release gates.
- [Open questions](docs/open-questions.md) -- the shared-memory hypothesis, named LLM failure modes.
- [Architecture Decision Records](docs/adr/) -- one ADR per non-obvious design choice. Some pre-0015 ADRs (0001, 0002, 0004, 0011, 0012) and design docs (`retrieval.md`, `design-principles.md`) describe the removed retrieval layer and are kept as historical records.

## License

MIT.
