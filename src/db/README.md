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

Bootstrap allows 30 seconds for each native version/control-data probe, including
verification of a retained bootstrap attempt. Fresh executable loading exceeded
the former five-second version deadline in native package testing. Errors identify
the failed bootstrap stage; ordinary readiness/status deadlines are unchanged.
Cargo watches the migrations directory so adding a forward migration also rebuilds
the embedded migrator in the application, not only a changed integration test.

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
also passed on Intel macOS and GNU Linux x86_64; quarantine and publication of qualified release artifacts remain release gates. These tests do not use `DATABASE_URL`.

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
from ordinary runtime, status, hooks or scheduler paths. Native bundle assembly
and artifact smoke are described below.

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

### Native online/offline bundles

`scripts/build-release-bundles.py` assembles native artifacts from a freshly built
`ygg` and the exact pinned PostgreSQL archive. It verifies archive size/digest,
binary architecture and CLI version, preserves PostgreSQL/theseus-rs/OpenSSL and
Yggdrasil license notices, and emits an online tarball, an offline tarball, a plain
binary compatible with the existing `install` script, and `SHA256SUMS`. It never
replaces an existing output directory/artifact. Tar metadata and gzip timestamps
are deterministic for fixed inputs and source commit.

Each tarball contains `bin/ygg`, an `initialize` launcher, instructions, the pin
manifest and a file-digest manifest with source commit and dirty-tree status. The
offline tarball also contains the original PostgreSQL archive. Extract into an
empty directory and run `./initialize`; the online launcher uses the ordinary
pinned download path, while the offline launcher supplies `--postgres-archive`.
Both preserve configuration selection; an external selection rejects the managed
archive. Keep the extracted directory stable while its supervisor runs.
For database-only unattended setup, use
`./initialize --yes --skip tmux,jq,rtk,hooks,project` to avoid optional dependency
installers and hook/project changes. Ordinary commands use `./bin/ygg`.

```sh
cargo build --release --locked --bin ygg
python3 scripts/build-release-bundles.py --binary target/release/ygg \
  --postgres-archive /absolute/pinned-postgresql.tar.gz \
  --target aarch64-apple-darwin --output /absolute/new-output-directory
python3 tests/release_bundles.py
python3 scripts/smoke-release-bundle.py /absolute/bundle-offline.tar.gz \
  --directory /absolute/new-disposable-profile
```

The native workflow assembles and uploads candidate bundles after its lifecycle
suite, then exercises the extracted binaries in separate disposable profiles.
Offline smoke denies curl, initializes/migrates, proves stable cluster/process
identity on archive-free reuse, creates/verifies a database backup and stops the
owned supervisor. Online smoke uses `--allow-download` for first initialization,
then denies downloads during reuse. Checksums establish byte identity, not release
authenticity or independently reproducible builds. CI artifacts are candidates;
release publication, macOS Developer ID/notarization/quarantine behavior and
clean-machine OS dependencies remain separate gates. No unsigned macOS quarantine
bypass is built into the launcher. Linux still needs the distribution libraries
listed by the native workflow.

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
through PgBouncer's `ignore_startup_parameters` setting and explicitly enables
`max_prepared_statements = 100` for the authority-guard checks (PgBouncer 1.21+). Run against an isolated database with
`YGG_TEST_PGBOUNCER_BIN` pointing to the binary and, when required, supply its
upstream password separately through `YGG_TEST_PGBOUNCER_PASSWORD`:

```sh
cargo test --test database_diagnostics -- --include-ignored --test-threads=1
```

The PostgreSQL 16/18 CI jobs run this test with the distribution PgBouncer package.
See [PgBouncer's pooling configuration](https://www.pgbouncer.org/config) for the
session and transaction guarantees.

## PostgreSQL backup component

`backup::dump` writes a consistent custom-format archive through an already-open,
empty private regular file. It records database identity, storage generation and
backend, corpus binding, applied migrations, table row counts, tool/server versions,
archive size and SHA-256. Inventory and `pg_dump --snapshot` use the same exported
repeatable-read snapshot; its transaction remains open until the tool exits.
The explicitly supplied binary directory must be absolute, and `pg_dump` cannot
be older than the source server. This operation never starts/migrates a server or
changes configuration. Run it against a direct/session-preserving endpoint with
an operator-selected credential that can read the entire database.

Native client settings are derived from effective SQLx options, including decoded
identity, socket, TLS mode and certificate paths. Credentials travel only in the
child environment, not arguments or debug output. Inherited `PG*` overrides and
later pgpass lookup cannot redirect the connection. Verified native TLS requires
an explicit `sslrootcert` file: PostgreSQL 16 libpq and SQLx do not share default
trust stores. This is checked before launching a tool; there is no weaker-mode
fallback. GSS encryption is disabled so
it cannot supersede the selected TLS policy. Native tool diagnostics are bounded
and redacted; any warning or unsuccessful exit leaves the artifact uncertified.
The operation has a 30-minute limit and kills its child on cancellation. Callers
must retain incomplete staging privately and publish only on success.

The pinned-package test exports while another transaction commits a new row,
restores into a disposable empty database and compares every inventoried table
count, the original UUID and database identity, and a restored check constraint.
TLS tests exercise native `pg_dump` as well as SQLx. The combined backup command below includes bundle/policy snapshots and publishes
a manifest. Validated restore is described below; automated deployment switching remains unfinished.
Roles and tablespaces are cluster objects outside this single-database dump;
restoration must explicitly provision target roles and validate grants.

See [PostgreSQL pg_dump](https://www.postgresql.org/docs/18/app-pgdump.html) for
exported snapshots and archive semantics.


## Combined operator backups

`ygg db backup /absolute/path/to/new-backup [--policy-dir /path/to/policy] [--json]`
creates a private directory containing `database.dump`, `backup.json`, and—when
knowledge exists—`knowledge/` and `policy/` snapshots. The destination parent must
exist, be owned by the operator and not be writable by other users. Existing
files, directories and symlinks are never replaced. Failed attempts retain private
`.deployment-backup-<UUID>` stages for inspection; retries use new stages.

Managed mode requires an already-running initialized server and uses its pinned
native tools and migration owner. It never installs or starts PostgreSQL. External
mode requires `--pg-bin /absolute/path/to/postgresql/bin` and uses the configured
owner credential when present, otherwise the runtime credential. The selected
credential must read all database contents. Remote verified TLS also requires an
explicit CA file as described above.

The bundle path comes from deployment configuration (`YGG_KNOWLEDGE_DIR` or
`knowledge_dir`). Its separate policy path comes from `YGG_KNOWLEDGE_POLICY_DIR`
or `knowledge_policy_dir`, defaulting to `knowledge-policy` under the profile data
directory. `--policy-dir` overrides that selection for one backup. An existing
bundle requires an initialized policy registry; neither directory may overlap the
other or the backup destination.
If both are absent, only a database still using SQL knowledge can be backed up.
A file-backed or fenced database cannot publish a database-only backup. No policy
registry is initialized implicitly, and no source files are deleted.

Bundle and policy snapshots hold both stores' writer/export leases in a stable
order. Database and filesystem revisions are recorded separately; this is not a
cross-storage transaction. Quiesce writers and external editors for deployment
moves or any recovery point requiring a single coordinated instant. A change to
the database storage-generation marker during capture prevents publication, as
does a mismatch between database and policy corpus IDs.

`ygg db verify-backup /absolute/path/to/backup [--json]` performs offline integrity
checks without loading database configuration or contacting a server. It checks
the exact component inventory, custom archive header, hashes, knowledge revisions
and corpus binding. It does not establish provenance, validate every SQL object,
or replace a restore rehearsal. The whole stage is verified and synced before
exclusive publication. No existing database or active configuration is changed by
backup. Restore is described below; upgrade and deployment-switch commands remain
unfinished.

### Validated restore

Create a fresh combined backup before recovery: older manifests without content
and schema evidence can still be integrity-checked, but `restore` refuses them.
Backups now record SHA-256 over sorted, length-framed JSONB table rows, alongside
counts and catalog definitions for relations, columns, constraints, indexes,
triggers, enum labels, user routines, views, extensions and database encoding/locale.
Restore compares these
inside a repeatable-read transaction, including database/storage identity and
migration checksums. Ownership and ACLs are intentionally rebound rather than
compared. This is validation of these recorded objects, not a general-purpose
PostgreSQL catalog equivalence proof; sequence values and arbitrary additional
provider objects are not independently fingerprinted.

Use only a trusted backup: PostgreSQL archives contain executable SQL, and restored
policy retains the backup's trust/approval configuration. Keep source and target
writers stopped throughout a deployment move. External targets must already exist,
contain no user objects or additional extensions, and have no other connected
sessions. Before importing, restore compares the target database encoding, locale
provider, collation and character classification against the recorded source
properties. A mismatch or missing/ambiguous source evidence rejects the restore
before creating database objects. Use matching database encoding and locale; managed destinations use UTF-8
and the pinned initializer’s C locale. Supply an owner credential with the required schema/extension privileges;
external runtime grants remain the operator's responsibility.

```sh
# DATABASE_URL and YGG_DATABASE_OWNER_URL select the empty target, not the source.
YGG_DB_MODE=external ygg db restore /private/backups/pre-move \
  --destination /private/recovery/external \
  --pg-bin /absolute/postgresql/bin --json

# Recover into an absent data directory using the pinned offline package.
# Unset DATABASE_URL and YGG_DATABASE_OWNER_URL when selecting managed mode.
YGG_DB_MODE=managed YGG_DATA_DIR=/private/recovery/new-managed \
  ygg db restore /private/backups/pre-move \
  --destination /private/recovery/files \
  --postgres-archive /private/packages/postgresql.tar.gz --json
```

The destination directory must be absent under an existing owned parent that is
not writable by other users. It receives `knowledge/`, `policy/` (when present)
and `restore.json`. Preserve the restored policy registry: the database identity,
corpus identity, document UUIDs and existing mappings remain the same. No source
server connection is made. Existing destinations and managed data directories are
never replaced. Managed restore provisions fresh owner/runtime roles, restores as
the limited owner, applies the usual runtime grants, and stops the new target.
It does not run schema migrations or binary upgrades as part of restoring data.

Native restore uses one transaction, refuses a PostgreSQL major downgrade, checks
the archive checksum through its open descriptor, and compares the restored state
before publishing the filesystem receipt. Cancellation kills the native client;
a disconnected PostgreSQL backend may take time to finish its current statement
and roll back. An error or lost response is not permission to retry a possibly
committed operation: retain and inspect the target and private staging directory.
Validation failure never cleans the target or overwrites the source. The database
commit and filesystem publication are separate boundaries, so a crash can leave a
complete target without a published receipt.

`restore.json` always reports `configuration_switched: false`. Keep writers stopped
until an explicit, reviewed configuration switch selects both the target database
and restored corpus/policy paths. The explicit switch command below selects those paths together. Upgrade commands
and complete recovery rehearsal gates remain under implementation.

### Select a restored deployment

`ygg db switch BACKUP --restore-dir RESTORED --target-config PROPOSED.toml`
validates a completed restore and atomically replaces the selected user
`config.toml`. Keep source and target writers stopped from backup capture through
this switch. This command selects one user's deployment; it does not quiesce other
clients, change the SQL/OKF storage generation, or migrate a fleet. Restart all
participating clients with the selected configuration before resuming writes.
The original database and original corpus are retained for rollback.

Prepare a private (`0600`) TOML file with explicit `data_dir`, `knowledge_dir`,
`knowledge_policy_dir` and database mode. For example, after a managed restore:

```toml
data_dir = '/private/recovery/new-managed'
knowledge_dir = '/private/recovery/files/knowledge'
knowledge_policy_dir = '/private/recovery/files/policy'

[database]
mode = 'managed'
```

For external mode use `mode = 'external'`, `url` for runtime access and optionally
`owner_url` for validation. Store credentials only in the private configuration
file. Corpus and policy paths must select the restored directories together.
For a SQL-only backup with no files, select absent corpus/policy paths instead;
the switch cannot attach an unrelated existing corpus. Managed targets may be
started for validation, but binaries are never installed or upgraded. A successful
managed switch leaves the selected target running. External lifecycle is unmanaged.

The current config directory must exist and be owned/private (`0700`). Conflicting
environment or legacy user `.env` settings cause refusal rather than a switch that
future commands would silently ignore. The proposed configuration is checked with
the same resolver used by normal commands. The command rechecks database contents,
recorded schema and identity, runtime CRUD/marker permissions, exact corpus/policy
bytes, the backup, and the proposed configuration before publication. It does not
change corpus trust, UUIDs or existing database identity mappings.

Each attempt retains a private `deployment-switch-UUID/` journal in the config
directory with exact `previous.toml` (if one existed), `next.toml`, immutable intent,
validation evidence and a completion receipt. It returns the operation UUID; an
error after journal creation also names it. A killed process may leave an operation
whose UUID is available from that directory name. Resume with the same inputs:

```sh
ygg db switch /private/backups/pre-move \
  --restore-dir /private/recovery/files \
  --target-config /private/recovery/proposed.toml \
  --resume OPERATION_UUID --json
```

A pending operation revalidates before publishing. A journal proving publication
finishes its receipt without restoring or replaying database mutations, even if
ordinary writes have since occurred. A changed current config, proposed file or
backup refuses recovery rather than overwriting independent edits. An interrupted
attempt without a durable `intent.json` cannot have published a config; retain it
and start a fresh attempt. Configuration publication uses an OS lease, descriptor
relative file access, private temporary files, fsync and atomic rename. External
editors must remain quiesced; the expected-content check is not a filesystem lock
against an uncooperative editor racing the final rename.

The previous config is a recovery artifact, not an automatic rollback command.
After new writes, switching back to the frozen source can lose those changes;
validated reverse import/recovery remains a separate required operation.

### Long data paths and transient sockets

New clusters keep their sockets in `postgres/runtime` when that canonical path
fits the portable Unix socket limit and needs no directory-list quoting. Longer
paths and paths containing spaces or punctuation select a short endpoint under
canonical `/tmp`, named with the OS user ID and random cluster UUID. This supports
64-character profiles and data directories such as macOS `Application Support`.
Persistent data, binaries, logs, ownership leases and authoritative manifests stay
under the configured data directory. Existing manifests without an endpoint field
continue using their original `runtime` directory; they are not relocated silently.

The short directory is private (`0700`) under a root-owned sticky parent. Its
private identity marker binds the directory to the exact canonical cluster root
and UUID. Unexpected paths, ownership, permissions, symlinks, identity markers or
unrecognized preexisting contents fail instead of being adopted. Marker publication
is exclusive and synced; interrupted private marker stages are retained for retry.
Both PostgreSQL and supervisor sockets use the same selected directory. Readiness
checks the PID-file endpoint, live socket configuration/permissions, database
system identity, major version, data directory, postmaster process and disabled TCP.

Temporary-directory cleanup does not remove database state. `status` and metadata
inspection never recreate a missing endpoint. An owner may recreate it under the
persistent cluster lease only after proving PostgreSQL is stopped. If a live server
loses its endpoint, it remains unverified: Yggdrasil neither starts a competitor nor
signals an unverified PID. Restore the original endpoint directory if it was moved,
or resolve the server state explicitly before restarting. Ordinary stop keeps the
small endpoint identity directory so subsequent starts reuse the same binding.

Native bootstrap fault coverage also pauses a real `initdb` after it creates
`PG_VERSION`, kills only its bootstrap parent, and resumes initialization while
the orphan remains alive. Recovery publishes a separate attempt with the original
cluster UUID. The test then resumes the orphan, waits for its successful
completion in the retained abandoned directory, and verifies that the published
cluster's control file is unchanged. Test-only output redirection lets the orphan
finish without depending on pipes owned by the killed parent. The native matrix
runs this fixture alongside the six publication-boundary crash tests.
