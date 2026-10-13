# Contributing to Yggdrasil

Thanks for your interest. Yggdrasil is in active development; the public API and schema move quickly, but the contribution flow is meant to be friendly.

## Workflow

1. **Branch from `main`.** Use a short, descriptive name (`scheduler-fanout`, `bench-scenario-3`, `fix-lock-race`).
2. **Work in small, focused commits.** Match the style of recent log: imperative, lowercase, area-prefixed (`scheduler:`, `bench:`, `tui:`, `docs:`). One commit can span multiple files; bundling related work is fine.
3. **Open a PR into `main`.** CI runs `cargo fmt --check`, `cargo check --all-targets`, `cargo test` against isolated PostgreSQL 16 and 18 service containers. Clippy runs advisory while we burn down existing warnings.
4. **Reference any related tasks** (`yggdrasil-NNN`) in the PR description so the rollup updates.
5. **Squash or rebase merges** are both fine; no merge commits into `main` please.

Direct pushes to `main` are reserved for trivial fixes (typos, generated artifacts) at the maintainer's discretion. Default to a PR.

## Setting up

```bash
make install                      # cargo build --release && copy to ~/.local/bin
ygg init                          # managed init, or existing external configuration
ygg up                            # tmux dashboard
```

Use stable Rust, matching CI. Managed initialization installs the pinned native
PostgreSQL distribution on the candidate platforms; an existing `DATABASE_URL`
continues to select external PostgreSQL. User configuration examples are
[config.example.toml](config.example.toml) and [.env.example](.env.example).
Never point integration tests at the database initialized for your ordinary work.
See the [operator guide](src/db/README.md) and
[release-gate ledger](docs/managed-postgres-okf-status.md) for supported evidence.

## Tests

- **Library tests** are fast and don't need Postgres: `cargo test --lib`.
- **Integration tests** require an isolated Postgres at `DATABASE_URL`. CI supplies PostgreSQL 16/18 service containers. Point `DATABASE_URL` at your disposable test cluster, clear unrelated `YGG_DATABASE_OWNER_URL`, `YGG_DB_MODE`, `YGG_CONFIG_DIR` and `YGG_DATA_DIR` overrides, and run `cargo test -- --test-threads=1`. Tests mutate schema and rows; never use an operator database. The legacy Compose file creates database/user/password `ygg`/`ygg`/`ygg`; its default URL is `postgres://ygg:ygg@localhost:5432/ygg`, and its named volume persists data. Reusing that volume does not provide test isolation.
- **Deployment/OKF contracts**: `cargo test --test database_config --test okf_documents` needs no database. `cargo test --test knowledge_contracts` compares legacy retrieval and JSON fixtures against the migrated database. These fixtures live in `tests/fixtures/knowledge/`; do not run database tests against a user installation.
- **Bench tests** use a fake `claude` binary at `benches/fixtures/fake-claude.sh` so they run in CI without API tokens. Real `ygg bench` runs invoke the real `claude` CLI; set `YGG_BENCH_CLAUDE_BIN` to override.

Deployment publication fault tests run without PostgreSQL:
`cargo test --lib config::switch::tests`. With `YGG_TEST_PG_ARCHIVE` set to the
verified native pinned archive, `cargo test --test managed_packages -- --include-ignored --test-threads=1`
uses disposable clusters to verify backup/restore, preserved claims and explicit
external/managed config selection. It must never target an operator database or
rewrite the developer's configuration. Config proposals and recovery journals in
these tests belong exclusively to their temporary directories.

`cargo test --test okf_disk_full -- --ignored --nocapture --test-threads=1` mounts
and fills a private 128 MiB filesystem to verify actual `ENOSPC` recovery. macOS
uses an APFS image; Linux requires noninteractive `sudo` for an isolated tmpfs.
The fixture verifies a separate bounded device before filling it and detaches
before deleting its files. The native CI matrix runs this opt-in test; ordinary
`cargo test` does not mount filesystems.

## ADRs

Non-obvious architectural choices land as Architecture Decision Records under `docs/adr/`. New ADRs:

1. Copy the most recent ADR for shape.
2. Number sequentially (zero-padded to 4 digits).
3. State alternatives you rejected and *why* — future maintainers need to know what you considered.
4. Link from `docs/adr/README.md`.

## Conventions

- Match existing file style; don't impose a different one.
- Read before write — when in doubt, look at neighboring code.
- Keep PRs focused. A bug fix shouldn't carry surrounding cleanup unless explicitly noted.
- Don't add docstrings, comments, or type annotations to code you didn't change.
- Validate at system boundaries only (CLI args, env, webhooks). Trust internal code.

## AI-agent contributors

Many commits land via AI-driven sessions (see the `Co-Authored-By: Claude Opus...` trailers). Same rules apply: focused PRs, tests, ADRs for architectural choices. The repo's own `CLAUDE.md` and `AGENTS.md` carry the per-agent conventions.

## Reporting issues

Issues welcome. For bugs, include reproduction steps, expected behavior, what you observed, and the relevant `ygg logs` excerpt if applicable. For design questions, prefix the title with `[design]`.

## License

By contributing, you agree your contribution will be licensed under the MIT License (see `LICENSE`).
