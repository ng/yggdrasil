# Managed PostgreSQL process prototype

`runtime::ManagedCluster` implements the process-ownership part of M1. It is
connected to hooks and application pools through `db::connect`; `ygg init` uses
the pinned installer. Existing URLs continue to select external mode. No managed platform is advertised
as release-ready by this prototype.

Explicit `initialize(root, bin, major)` creates a new private persistent cluster
using an operator-selected native distribution. It checks the requested binary
major, initializes UTF-8 PostgreSQL, disables TCP, configures a private Unix socket,
and durably saves the PostgreSQL system identifier, independent cluster UUID,
major version, binary path and version string. It refuses nonempty roots and never
deletes an interrupted initialization. Release archive verification and atomic
extraction live in `package`. Bootstrap writes an immutable intent with the cluster
UUID, binary identity and canonical root before running initdb. Every attempt uses
its own directory, so a surviving initdb child cannot write into a retry's data.
Only a receipt written after successful initdb and durable configuration can
publish that attempt as `data`; publication and the final manifest use exclusive
rename and directory fsync. Rerunning `ygg init` resumes completed receipts and
retains unfinished attempts. Unexpected preexisting data, changed versions or
identity mismatches fail without replacement. Legacy interrupted roots lacking
an intent are not automatically adopted. Native unit tests kill subprocesses at
six durable boundaries; the CI matrix runs these on all planned platforms.

`open` and `status` do not initialize or start PostgreSQL. `try_owner` obtains a
nonblocking OS lease in the canonical cluster root. The returned `Owner` retains
that lease for its lifetime; competing clients wait for bounded readiness.
`start_or_adopt` starts a detached postmaster only if the cluster is stopped, or
checks the on-disk PostgreSQL system identifier before launch, and
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

Readiness must not equate the PID-file timestamp with
`pg_postmaster_start_time()`. PostgreSQL captures `MyStartTime` before startup work
and `PgStartTime` after shared-preload initialization; these can cross a second
boundary. See the [PostgreSQL startup implementation](https://github.com/postgres/postgres/blob/REL_16_STABLE/src/backend/postmaster/postmaster.c).
Verification binds a live authenticated backend's parent PID to the unchanged PID
file while also checking directory, system identifier, version and disabled TCP.
The native regression test compiles a small preload library that sleeps for two
seconds, proves the timestamps differ, and verifies readiness and draining stop.

The native-process approach avoids putting a `postgresql_embedded` handle in
short-lived clients: the candidate's current [Drop implementation](https://raw.githubusercontent.com/theseus-rs/postgresql-embedded/main/postgresql_embedded/src/postgresql.rs)
stops a started server even with persistent data. PostgreSQL documents native
startup and smart shutdown in [pg_ctl](https://www.postgresql.org/docs/16/app-pg-ctl.html).
`ygg db serve` retains `Owner`, monitors readiness, restarts a verified crashed
server and coordinates explicit stop requests. SIGINT/SIGTERM end supervision but
leave PostgreSQL running for adoption. Only `ygg db stop` drains the database.

The bootstrap role is only for provisioning and identity verification. Explicit
`ygg migrate` provisions a separate non-superuser migration owner and runtime role.
Ordinary commands use the runtime role, which can write application data but cannot
change schema, triggers, roles or migration/knowledge markers. Supplied binaries are trusted inputs in this spike;
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
18.3 and pinned 16.15 on macOS arm64. The native installer/lifecycle CI matrix
also passed on Intel macOS and GNU Linux x86_64; quarantine and offline release
bundle assembly remain release gates. These tests do not use `DATABASE_URL`.

## Supervisor commands

The `db` commands and `AppConfig` use the same deployment resolver. Ordinary
commands start/adopt initialized managed clusters through `db::connect`; configuration
loading and status do not start servers. For managed mode they select
`<resolved-profile-data-dir>/postgres`; that root must already have been initialized
by the native runtime. Run `ygg init` to install the pinned native package, initialize the private
cluster and provision roles/schema. Offline installations use
`ygg init --postgres-archive /absolute/native-release.tar.gz`. Concurrent init
processes serialize, and reruns retain the cluster identity and installed version
without downloading. Managed init never writes a localhost URL. External init
uses the configured database and role, does not start services or create roles/
databases, and requires owner privileges when running migrations.
`--database-url` is an invocation-only override; persistent selection belongs in
user config or environment.
They never download binaries, initialize data, migrate schemas or upgrade binaries.

- `ygg db status [--json]` reads configuration, checks a managed server's identity
  and optionally queries its supervisor. Missing managed roots report
  `not_initialized` without creating directories. External status reports only
  configured/unmanaged selection; it does not claim connection health or expose
  the URL or credentials.
- `ygg db start [--timeout 30] [--json]` detaches `ygg db serve` and returns only when
  its control response and independent PostgreSQL verification agree on a ready
  PID. Each child receives the selected canonical root and cluster UUID, avoiding
  a configuration/profile change redirecting the launch. Concurrent losing
  supervisors exit; another spawn is permitted only after the previous child has
  exited and the OS ownership lease is available.
- `ygg db stop [--timeout 30] [--json]` asks the current owner for smart shutdown.
  Without a supervisor it can take the lease and stop a verified orphan directly,
  without first starting a server. After delivery, a missing reply is reported as
  an unknown outcome and is never automatically resent. Inspect status before
  retrying. A stop timeout ends monitoring so a later completed drain cannot be
  mistaken for a crash and automatically restarted.
- `ygg db serve` owns the initialized managed cluster in the foreground. The
  supervisor uses a bounded JSON protocol on a mode-0600 Unix socket inside the
  private runtime directory. Protocol version, cluster identity and peer OS user
  are checked. Wrong identities and malformed/oversized frames cannot authorize
  shutdown. Only the lifetime lock owner replaces a stale owned socket; unexpected
  files or symlinks are retained as errors.

Timeout arguments accept 1–300 seconds. External start/stop/serve are rejected,
without connecting to that target or creating managed state. Application pools,
hooks and schedulers still use their existing connection paths pending central
runtime integration and role provisioning.

The opt-in `managed_supervisor` test runs 20 actual `ygg db start` processes,
checks one reported owner/postmaster, kills the supervisor and adopts the surviving
postmaster, crashes PostgreSQL and verifies automatic recovery with a retained row,
checks client-draining stop and immediate restart, and proves stop timeout does
not cause automatic restart. The normal CLI test covers read-only status, ignored
repository `.env`, missing initialization and redacted external-mode rejection.
Run native coverage with the same binary/major environment as above:

```sh
YGG_TEST_PG_BIN=/absolute/postgresql/bin YGG_TEST_PG_MAJOR=18 \
  cargo test --test managed_supervisor -- --include-ignored --test-threads=1
```

## Pinned distribution installer

`packages.json` pins PostgreSQL 16.15 / theseus-rs distribution 16.15.0 for macOS
arm64, macOS x86_64 and GNU Linux x86_64. Each record includes the exact release
URL, archive size and SHA-256. The release records source commit
`2954f800589f74265f136cdb490b87749e209e7c`. Downloaded archives for all three targets
were checked against the release asset digests; they include `uuid-ossp`.

`package::install_offline` accepts the native pinned archive, reads it through a
bounded regular-file descriptor and verifies its exact bytes before creating any
destination state. `install_download` explicitly downloads the same HTTPS artifact
using curl (which is not required for offline installation). Neither is called
from ordinary runtime, status, hooks or scheduler paths. User-facing `ygg init`
and offline release-bundle assembly remain pending.

Installation holds a private OS lock, extracts into a fresh private staging
directory, syncs files and directories, then publishes using an exclusive atomic
directory rename. It never replaces an existing destination, even an empty one.
Existing installations are compared with the freshly verified archive's complete
file inventory, digests, executable bits and symlink targets; independent edits
and extra files cause an error. Receipts contain no exclusive copy of binaries.
Failed or killed staging directories are never selected as installed packages;
partial directories remain available for inspection and later explicit cleanup.

Extraction accepts only regular files, directories and relative sibling symlinks
to regular files. Links are created after file extraction. Absolute paths, parent
traversal, hard links, devices, duplicate members and excessive compressed or
expanded data are rejected. Package files/directories are private to the OS user;
archive ownership, special permissions and xattrs are not applied. PostgreSQL and
upstream license files are preserved. The installation adds `YGG-RELEASE.json`
and `YGG-THIRD-PARTY-NOTICES.txt`, including the Apache 2.0 license for the bundled
macOS OpenSSL 3.6.3 libraries. Linux uses system runtime libraries.

The downloaded macOS arm64 16.15 archive passed the runtime and supervisor native
suite, including 20 starts, crashes and adoption. An installed copy also starts,
runs all migrations and loads `uuid-ossp`. The installer/lifecycle suite has also passed on Intel macOS and GNU Linux
x86_64; those results do not establish quarantine or clean-machine dependency gates. `.github/workflows/managed-postgres.yml` now runs release-mode
installer/lifecycle smoke on macOS arm64, macOS Intel and Ubuntu 24.04. Its Linux
job installs runtime libraries (not PostgreSQL) needed by the GNU artifact, and
its macOS jobs verify the upstream ad-hoc signature. `scripts/prepare-postgres-smoke.py`
is CI-only preparation, not the product installer. Platform results, clean-machine
dependency handling, quarantine behavior and offline release packaging must be
verified before advertising managed installation as release-ready.

To test an offline archive locally:

```sh
YGG_TEST_PG_ARCHIVE=/absolute/postgresql-16.15.0-aarch64-apple-darwin.tar.gz \
  cargo test --test managed_packages -- --include-ignored --test-threads=1
```

Set `YGG_TEST_PG_DOWNLOAD=1` too to exercise the explicit HTTPS downloader. The test
creates only disposable private clusters and never reads `DATABASE_URL`.

## Session-lock authority

Scheduler and watcher daemons use `singleton::SingletonGuard` on a detached
connection. The guard verifies the original backend PID and granted advisory lock
before polling work and every 250 ms while it runs, with a three-second probe
limit. A failed probe permanently invalidates that guard, drops the supervised
future and exits the daemon. Pool recovery cannot revive the old authority;
a new run must acquire a fresh session lock. Standalone scheduler ticks and
watcher `--once` also use the guard.

Cancellation cannot recall SQL or OS actions already issued. Their outcome can
be unknown; the guard does not retry them. Checks require direct or session-
preserving connections. Observing one successful probe does not certify a
transaction-pooling endpoint as compatible. Integration tests terminate the
actual lock backend, verify work cancellation and daemon exit while other pooled
connections remain healthy, then acquire replacement authority.

## External TLS

External remote TCP connections default to `sslmode=verify-full`: SQLx verifies
both the certificate chain and the requested hostname. Explicit weaker modes
(including `require` and `verify-ca`) are rejected for remote hosts. Unix sockets,
`localhost` and loopback IP endpoints retain their configured modes for local
PostgreSQL compatibility; select `verify-full` explicitly when TLS is required
on a local tunnel or proxy. The effective SQLx host/socket, including URL query
overrides, determines which policy applies.

A private CA can be configured through `sslrootcert` in the database URL or
`PGSSLROOTCERT` in the user environment. For example:

```text
postgres://user@db.example.com/ygg?sslmode=verify-full&sslrootcert=/absolute/ca.pem
```

Unknown URL parameters and parse errors are rejected without echoing values,
before SQLx can log ignored parameters. Keep credentials in user-owned config or
environment. A native test creates a disposable CA/server certificate, verifies
an encrypted session, rejects an unrelated CA and hostname mismatch, and refuses
a server that does not offer TLS. The native release matrix runs this test.

## External migration credentials

Set `YGG_DATABASE_OWNER_URL` or `[database].owner_url` in the user configuration
to supply a separate migration credential. The environment value takes precedence.
Only explicit `ygg migrate` and the migration phase of `ygg init` use it;
ordinary commands and `ygg migrate --check` use the runtime database URL.
Without an owner URL, explicit migrations retain the configured database
credential for compatibility. The operator must provision runtime grants and
either allow the owner to install `uuid-ossp` or preinstall that extension.

The owner URL must select the same effective host, socket, port and database as
the runtime URL. Different credentials are allowed; endpoint aliases and a
different default database are rejected before connecting. Both URLs obey the
external TLS policy. Owner credentials are redacted from debug configuration.
Managed mode rejects external owner configuration and provisions its own roles.

The native package test creates a separate external database, proves that its
restricted runtime role cannot migrate, applies migrations with its owner, and
runs `migrate --check` with an unusable owner credential to verify separation.

## Connection diagnostics

Run `ygg db diagnose --json` to inspect the configured runtime credential without
starting, initializing or migrating a server. The bounded, read-only probe reports
PostgreSQL major version (16 and 18 are the tested suite), `uuid-ossp` availability,
role and schema-creation privileges, and backend identity across five separate
transactions on one client connection. Owner credentials are not used. Elevated
runtime privileges indicate that the operator should supply a restricted role;
missing `uuid-ossp` requires explicit owner migration or administrator setup.
`postgres_backend_tls` describes the PostgreSQL connection, which may be the
proxy-to-server leg; it does not certify the client-to-proxy TLS configuration.

A changed backend produces `backend_changed_incompatible` and a failing exit
status. An unchanged backend produces `stable_but_unverified`: this is **not** a
certificate of session support. Confirm a direct or session-pooling endpoint with
the provider/operator before running scheduler or watcher. Transaction pooling is
unsupported, even if a quiet pool happens to reuse one backend during this probe.
The probe only issues SELECTs, so it leaves no session advisory locks or modified
session state on pooled servers. Connection/query failures are redacted.

The integration test starts a private PgBouncer with session and transaction
aliases, populates two backends and enables round-robin reuse. It verifies both
library observations and CLI exit status. It also pins the original backend on
another client to verify that the singleton guard rejects reassignment before
polling work. The fixture accepts SQLx's `extra_float_digits` startup parameter
through PgBouncer's `ignore_startup_parameters` setting. Run against an isolated database with
`YGG_TEST_PGBOUNCER_BIN` pointing to the binary and, when required, supply its
upstream password separately through `YGG_TEST_PGBOUNCER_PASSWORD`:

```sh
cargo test --test database_diagnostics -- --include-ignored --test-threads=1
```

The PostgreSQL 16/18 CI jobs run this test with the distribution PgBouncer package.
See [PgBouncer's pooling configuration](https://www.pgbouncer.org/config) for the
session and transaction guarantees.
