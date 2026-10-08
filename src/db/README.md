# Managed PostgreSQL process prototype

`runtime::ManagedCluster` implements the process-ownership part of M1. It is
deliberately not yet connected to `ygg init`, hooks or application pools. Existing
external database selection remains unchanged. No managed platform is advertised
as release-ready by this prototype.

Explicit `initialize(root, bin, major)` creates a new private persistent cluster
using an operator-selected native distribution. It checks the requested binary
major, initializes UTF-8 PostgreSQL, disables TCP, configures a private Unix socket,
and durably saves the PostgreSQL system identifier, independent cluster UUID,
major version, binary path and version string. It refuses nonempty roots and never
deletes an interrupted initialization. This is not an installer: release archive
checksums, provenance, atomic binary extraction and interrupted-bootstrap recovery
remain required before user-facing initialization is enabled.

`open` and `status` do not initialize or start PostgreSQL. `try_owner` obtains a
nonblocking OS lease in the canonical cluster root. The returned `Owner` retains
that lease for its lifetime; competing clients wait for bounded readiness.
`start_or_adopt` starts a detached postmaster only if the cluster is stopped, or
adopts the existing server after checking:

- PostgreSQL data directory, system identifier, major version and disabled TCP;
- PID-file directory, port, socket directory and start time;
- the live SQL backend's parent PID, matching the recorded postmaster;
- an unchanged PID file across verification.

A stale positive PID can authorize PostgreSQL's own stale-lock recovery only if
the OS reports that process absent. A live but unverified PID never authorizes a
signal, PID-file deletion or competing start. Dropping an owner releases its OS
lease without stopping the database; dropping ordinary connections also leaves
the postmaster running. A replacement owner can adopt that server. The owner can
reap and restart a crashed child. Explicit `stop` verifies identity before asking
`pg_ctl` for smart shutdown, which drains clients; timeout does not escalate to
an immediate shutdown. Readiness timeouts retain server state for inspection.

The native-process approach avoids putting a `postgresql_embedded` handle in
short-lived clients: the candidate's current [Drop implementation](https://raw.githubusercontent.com/theseus-rs/postgresql-embedded/main/postgresql_embedded/src/postgresql.rs)
stops a started server even with persistent data. PostgreSQL documents native
startup and smart shutdown in [pg_ctl](https://www.postgresql.org/docs/16/app-pg-ctl.html).
The future `ygg db serve` loop must retain `Owner`, monitor readiness, and coordinate
explicit stop requests. This library does not yet implement that CLI/IPC loop.

The bootstrap role is only for initialization and identity verification. Limited
runtime and migration-owner roles, migrations and application connection dispatch
remain to be integrated. Supplied binaries are trusted inputs in this spike;
exact version strings are not archive integrity checks. Socket paths exceeding
the portable Unix socket budget are rejected instead of enabling TCP. Configuration
and process ownership assume the owning OS user controls their private files.

Run the opt-in native lifecycle tests against an explicit development distribution:

```sh
YGG_TEST_PG_BIN=/absolute/postgresql/bin YGG_TEST_PG_MAJOR=18 \
  cargo test --test managed_runtime -- --include-ignored --test-threads=1
```

They initialize only disposable clusters under `/tmp`, require `uuid-ossp`, race
20 OS processes, verify one lifetime owner and one postmaster, terminate the owner
and adopt its surviving server, kill/recover PostgreSQL and verify acknowledged
rows, and prove client-draining shutdown. Negative tests cover wrong major and an
unrelated live PID without signaling it. Local validation uses Homebrew PostgreSQL
18.3 on macOS arm64; pinned PostgreSQL 16 release artifacts and macOS x86_64/Linux
x86_64 smoke tests remain release gates. These tests do not use `DATABASE_URL`.
