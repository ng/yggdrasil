# Managed PostgreSQL upgrades

`ygg db upgrade` explicitly upgrades a running managed cluster from an older
patch to the packaged pin for its existing major (16.15 or 18.6). The
`db upgrade` command does not change major versions or external databases. Ordinary startup never upgrades a cluster.

Stop application writers, hooks, schedulers and other clients first. The
`--quiesced` flag is your declaration that this has happened; it does not stop
remote or older clients. The source preflight rejects other sessions in the
Yggdrasil database and unsupported extensions.

```sh
ygg db upgrade --quiesced --backup /absolute/unused/backup-directory --json
```

For offline operation, add `--postgres-archive /absolute/pinned-archive.tar.gz`.
The archive must match the packaged checksum and platform. The backup parent
must exist; the destination must be unused and outside the cluster directory.
The combined backup includes database, knowledge and configuration evidence.

The command records a durable operation UUID, fences ordinary managed startup,
verifies the backup, stops the source, selects the verified binaries, starts the
target and checks database identity, schema and rows. It retains the original
binaries and backup. It does not automatically restore data or downgrade after
an error.

Inspect a pending operation without starting the server:

```sh
ygg db status --json
```

Resume with the same configuration, backup destination and operation UUID:

```sh
ygg db upgrade --quiesced --backup /absolute/unused/backup-directory --resume OPERATION_UUID --json
```

Supply the same offline archive option when needed. Keep clients quiescent
until the operation completes. Changed configuration or backup evidence is
rejected. A completed operation may be resumed without replaying its backup
or discarding writes made after completion.

Before binary selection changes, `--abort --resume OPERATION_UUID` restores
ordinary startup of the source selection and retains the evidence. Abort is
rejected once target binaries have been selected. At that point, inspect the
failure and resume or perform explicit recovery; do not edit the journal or
replace the data directory manually. A stop timeout leaves state for inspection.

Native tests exercise PostgreSQL 16.14 to 16.15, including killed upgrade
processes after prepared, backed-up, stopped, switched and complete journal
publication, abort before selection, and preservation of later writes.
Fleet-wide quiescence remains an operator prerequisite.

## Explicit major upgrade through dump and restore

The supported managed 16→18 path uses a consistent combined backup, a new data
directory and a separately validated configuration switch. It preserves the
source cluster and files. PostgreSQL 18.6 has a separate pinned archive for each
supported platform; ordinary initialization still defaults to PostgreSQL 16.

Stop all application writers and keep them stopped through selection. Create
an unused backup from the source deployment:

```sh
ygg db backup /absolute/major-backup --json
```

Restore into a new managed data directory and a separate new file directory:

```sh
YGG_DB_MODE=managed YGG_DATA_DIR=/absolute/pg18-data \
  ygg db restore /absolute/major-backup --postgres-major 18 \
  --destination /absolute/pg18-files --json
```

For offline operation, add `--postgres-archive /absolute/pg18-pinned.tar.gz`.
A PostgreSQL 16 archive cannot satisfy an explicit PostgreSQL 18 selection.
The target and file-directory parents must exist and be owned and private.
Restore validates database identity, rows, claims, schema and both file snapshots
before publishing its receipt. It leaves the original configuration selected.

Create a private target configuration file (mode `0600`) selecting all components:

```toml
data_dir = "/absolute/pg18-data"
knowledge_dir = "/absolute/pg18-files/knowledge"
knowledge_policy_dir = "/absolute/pg18-files/policy"

[database]
mode = "managed"
```

For a SQL-only backup with no file component, select unused knowledge/policy
paths instead. Remove conflicting database/path environment or user `.env`
overrides before applying the target file:

```sh
ygg db switch /absolute/major-backup --restore-dir /absolute/pg18-files \
  --target-config /absolute/pg18.toml --json
```

Switch revalidates restored state and runtime grants, then publishes the whole
configuration atomically. Keep the returned operation UUID. An interrupted
switch is resumed by repeating that command with `--resume OPERATION_UUID`;
a confirmed switch retry preserves later writes rather than restoring old data.

A failed restore retains its target for inspection and refuses to overwrite it.
Inspect the failure and use fresh destinations for a deliberate retry. Do not
start applications against a partial target. If writes have occurred after the
switch, the retained source is stale: recovery must preserve those new writes
instead of simply selecting the old cluster. Back up the selected PostgreSQL 18
deployment before planning such recovery; this workflow does not provide an
automatic major-version downgrade.

`tests/managed_major_restore.rs` exercises the real CLI backup, new-directory
restore and configuration switch with populated claims and knowledge, confirms
the unchanged source, rejects unsupported/mismatched majors and existing targets,
and preserves post-switch writes on retry. Native 16.15→18.6 passed locally, including the configuration switch and later-write retry.
The major-flow baseline suite passed 590 tests, and 11 native tests passed after
the startup-deadline correction. The new three-platform CI run is still required
before claiming platform qualification.

Startup version and control-file probes share the caller's overall timeout.
A cold native executable can take more than five seconds to launch; there is no
separate five-second probe cap. Deadline expiry retains cluster state and names
the timed-out helper for inspection. The controlled-probe native test accepts a
six-second helper under a 30-second budget and rejects it under a two-second
budget. This does not certify every clean-machine signing or dependency path.
