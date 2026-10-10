# Managed PostgreSQL patch upgrades

`ygg db upgrade` explicitly upgrades a running managed cluster from an older
patch to the packaged PostgreSQL pin (currently 16.15). It does not upgrade
major versions or external databases. Ordinary startup never upgrades a cluster.

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
Major-version upgrade orchestration and fleet-wide quiescence remain open.
