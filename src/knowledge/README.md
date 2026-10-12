# OKF engine implementation

The document, matching and local-store modules operate without a database
connection. OKF is the only knowledge backend (ADR 0019, M7): `remember`,
`learn`, `prime`, hooks and task-claim injection read and write the selected
corpus, and the legacy `memories`/`learnings` tables no longer exist
(`migrations/20261012000001_remove_legacy_knowledge.sql`). The SQL→OKF cutover,
fleet and rollback tooling that performed the 2026-10-11 operator migration was
removed with M7; its history is in the release ledger and in Git.

`document` parses bounded OKF 0.2 Markdown/YAML, retains unknown metadata, and
checks digest-bound Yggdrasil activation. `matching` preserves the legacy SQL
predicates, including translated LIKE patterns, null filters, JSONB scope-tag
text, specificity and recency, with UUID tie-breaking. The same fixtures run
against PostgreSQL and the offline predicates.

## Private store durability

`store` currently supports macOS and Linux. `KnowledgeStore::open(path, true)`
explicitly creates a private corpus; `false` never creates one. The root must be
owned by the effective user with no group/other permission bits. Generated
subdirectories are mode 0700 and files are mode 0600. Document paths contain only
typed UUIDs and fixed layout components.

Every mutation takes an exclusive OS lock on `.writer.lock`. Callers supply the
expected SHA-256 of the exact existing bytes, or `Absent` for creation. A stale
revision returns a conflict. This revision differs from the activation digest,
which excludes display-only metadata and telemetry.

Writes serialize and reparse before publication, then write a unique temporary
file in the destination directory, sync it, atomically rename, and sync the
directory. New directory entries are synced too. Acknowledgment occurs only after
those operations succeed. Deletion checks the same revision and syncs its parent.
These guarantees depend on the local filesystem honoring fsync/atomic rename;
shared/network filesystem behavior has not been validated.

A failure before rename leaves the prior acknowledged version intact and removes
the temporary file when possible. A process crash can leave a hidden `.UUID.tmp`
file; it is never read as knowledge and may be removed under the writer lock.
A failure after rename is an ambiguous outcome: reload and compare before retrying.
OS locks release automatically when a process exits. Cooperative writers cannot
lose an acknowledged update, but an external editor that ignores the lock can
still overwrite a write; use a supported conditional edit path.

Reads and mutations use directory descriptors and no-follow opens, including for
the lock and temporary files. A symlink in a bundle cannot redirect an operation
outside it. Readers observe complete old/new files. A scan is not a multi-document
transaction: publication of complete migration/shared snapshots remains a separate
layer. Fresh scans diagnose and exclude corrupt files or path/identity mismatches
while retaining unaffected documents. Hidden abandoned staging files are ignored.

Tests cover reopening, stale revisions, 20 concurrent writers, concurrent readers,
symlink escapes, corrupt documents, permission checks and a subprocess file-size
limit that forces a partial write failure. The latter tests failed-write recovery,
not a full disk. The opt-in `okf_disk_full` fixture fills a separate 128 MiB
filesystem until real `ENOSPC`, then requires both an update and a new document
write to fail with that error while the prior UUID, bytes and revision remain
intact. It checks staging-file cleanup, frees space and verifies conditional retry
and reopening. It refuses to fill a filesystem sharing the host directory's device
or exceeding its capacity bound. macOS uses an owned APFS image; Linux uses an
isolated tmpfs and requires noninteractive `sudo` for mount/unmount. Detach failure
retains the fixture for inspection instead of recursively removing a mounted tree.

Run `cargo test --test okf_disk_full -- --ignored --nocapture --test-threads=1`.
The native platform workflow runs the same fixture in release mode. Local macOS
arm64 APFS validation reached `ENOSPC` after 129,368,064 filler bytes and passed
all preservation/retry assertions. This is disk-full write evidence, not arbitrary
filesystem or power-loss qualification; Linux tmpfs is not a disk durability test.
[Native run 37995932262](https://github.com/ng/yggdrasil/actions/runs/37995932262)
at `a97f8fa` passed this fixture in release mode on macOS arm64, Intel macOS and
GNU Linux x86_64. Both APFS jobs reached `ENOSPC` after 129,368,064 filler bytes;
Linux tmpfs reached it after 134,152,192 bytes. All three jobs also passed native
lifecycle/restore tests and both extracted-bundle smoke tests.

## Identity and policy configuration

`identity` stores `identity.json` in a separate private configuration directory,
using the same locked atomic-write primitives. It persists the corpus UUID,
explicit trust flag, portable repository UUIDs, canonical URL aliases, common Git
directories, and source-database UUID to legacy-repo UUID bindings. Multiple legacy
rows may explicitly map to one portable repository. This configuration must be
backed up with the bundle; it is not disposable index state.

Git discovery uses the canonical common directory, so linked worktrees share a
scope and unrelated local repositories with equal names do not. Standard forge
SSH/HTTPS aliases resolve together; custom server schemes, ports, SSH users and
home-relative paths retain their identity distinctions. URLs with queries or
fragments require explicit mapping; passwords are never persisted as aliases.
A changed origin or conflicting URL/path does not silently reassign knowledge.

Explicit alias, database-binding or trust edits require the current configuration
revision. Reinitialization cannot replace a corpus ID, repair corrupt configuration
by discarding it, or change its trust flag. Backup restoration and new database
bindings preserve all existing corpus, repository and document IDs. On a different
host, review/rebind local common-directory paths instead of treating paths copied
from another machine as authoritative identity evidence. Client commands still
need to wire these primitives into their scope and trust resolution.

## Note and learning operations

`service` joins the private bundle with the separate identity/trust registry and
an explicit nonempty user mapping. It creates/lists/deletes notes, creates manual
rules or pending proposals, approves/rejects with expected revisions, and edits
without changing original provenance. It never needs a database connection.
Manual active creation retains the existing CLI semantics; proposals cannot
activate themselves. Approval accepts a human caller or an agent UUID explicitly
listed in `approval_leads` in policy configuration. Caller classification belongs
to the command/session adapter, not untrusted document metadata.

Covered edits clear approval and become pending. External edits that invalidate
an active rule's digest also appear in triage; unchanged expired rules do not
become proposals. Display-only edits preserve evidence and cannot mint activation.
User/repo bindings, corpus trust, expiry, matching and current byte revisions are
checked before delivery. Rule/note revalidation methods reject stale selections.
Prime's five-note limit is applied after expiry filtering, while explicit browsing
can still show stale/deprecated content.

The store inventories IDs across all scopes and both document kinds before a
mutation. Duplicate UUIDs are rejected or excluded with diagnostics, including
externally copied files; unrelated good documents remain available. An invalid
filename or corrupt document does not erase unaffected knowledge. Incomplete
filesystem enumeration fails UUID mutations/lookups rather than guessing identity.
The inventory currently scans filenames and is not the final performance index.

## Scope moves and interrupted operations

`service::move_scope` preserves a document's UUID, kind, text and original
provenance. A moved rule becomes pending and loses its old approval; note scope
moves retain the explicit manual note state. Unknown target repo bindings and
stale revisions are errors.

The local store persists one `.move.json` intent under its writer lock. This is
operational recovery state, containing a bounded JSON header followed by exact
new document bytes. It records both keys and before/after digests. The target is
written to a synced staging inode, linked exclusively into its destination, and
synced before the source is removed. Source removal and intent removal each sync
their directory. A destination appearing independently is never overwritten.

Reads and mutations resume a pending intent under the same OS lock. Recovery
checks UUID uniqueness and the recorded source/destination revisions before doing
anything destructive. If either location changed independently, recovery reports
an error and preserves both data and intent for manual resolution. A failure or
process exit can be an ambiguous move outcome: reload before retrying. Recovery
finishes forward; it does not infer that an interrupted request was cancelled.
A failed recovery suppresses automatic bundle reads with a diagnostic until the
conflict is resolved.

Tests exit a subprocess abruptly after durable intent publication, destination
publication, source removal and intent removal. Reopening recovers exactly one
complete document with the same UUID. Additional tests cover concurrent moves,
revision/destination conflicts, activation invalidation and independent edits
made after interruption. This is a single-document move protocol, not a general
multi-document transaction or the migration cutover protocol.

## Explicit document browsing

`ygg knowledge browse --json` lists valid, visible Markdown documents throughout
the selected private or shared corpus, including generic OKF types without a
Yggdrasil profile. `ygg knowledge browse decisions/example.md` prints the exact
source text; add `--json` for its path, type, exact-byte SHA-256 and source text.
Unknown metadata and body bytes are retained. These commands require no database.
Browsing does not grant activation or add documents to note/rule candidates.
Backups retain generic documents, but SQL rollback refuses a corpus containing
documents outside the representable note/rule snapshot rather than omitting them.
Shared results identify the confirmed Git commit and whether it is current; after
refresh failure, a retained confirmed revision remains explicitly browseable.

Paths are relative to the corpus and cannot contain hidden components, traversal,
backslashes or control characters. Reads reject symlinks, hard links and special
files. Listings retain valid documents alongside diagnostics and exit nonzero if
incomplete. Non-Markdown files and hidden control/cache directories are excluded.
Traversal is bounded to 32 path components, 100,000 directory entries, 1 MiB per
document and 64 MiB of document reads. Explicit reads return only one document;
listings retain metadata without document bodies. External edits may occur between
files: the listing is an inspection, not a transactional corpus snapshot.

## Remaining engine work

Explicit browsing snapshots scan the Yggdrasil layout directly; rule/prime
candidate lookup uses the disposable index below. Approval must still be
revalidated immediately before injection; never treat a previously read
document as current authority. Shared Git transport builds on this layer.

## Reproducible local read measurement

Run the manual process benchmark on an otherwise idle host:

```sh
cargo test --release --test okf_performance -- --exact okf_process_benchmark --ignored --nocapture
```

It creates a disposable private corpus of 10,000 documents (8,000 notes and
2,000 active rules), then starts 20 independent OS processes. Each process warms
its own service instance, waits at a common barrier, and measures three rounds.
Each round selects 20 matching rules and the five prime notes and revalidates
every selected document immediately before counting its body bytes. Selection,
file access, parsing, approval checks, sorting and revalidation are included;
fixture creation and warmup are excluded. The report contains p50/p95 milliseconds
for rules and notes separately over 60 samples and the selected body size.
Workers are terminated on failure or timeout; the corpus is removed on exit.

This is an engine measurement, not proof of the release's 50 ms *additional*
warm-hook p95 target. The SQL hook baseline, actual CLI/hook overhead, rendering,
telemetry, concurrent writes and shared-remote latency require separate runs.
Do not compare debug-build timings or run this concurrently with other tests.

The SQL-relative edit-hook qualification uses the actual release CLI:

```sh
DATABASE_URL=postgres://.../isolated_test_database \
YGG_HOOK_BENCH_REPORT=/absolute/path/hook-latency.json \
cargo test --release --test okf_hook_performance sql_relative_edit_hook_p95 -- --ignored --nocapture --test-threads=1
```

It creates and drops its own migrated database and private 10,000-document corpus
(8,000 notes and 2,000 rules). Twenty agents modify distinct file paths; every
request uses a new session and must emit the same 20 ordered rules in both modes.
Each mode warms all clients, then measures five synchronized rounds (100 samples).
Timing includes CLI startup, selection/revalidation, output, coordination and
telemetry. The SQL phase precedes OKF selection; this fixture selection is not a
migration rehearsal. The JSON report retains every sample and computes OKF p95
minus SQL p95. The test fails when added p95 exceeds 50 ms, and preserves the report
at the optional path even on a threshold failure. Child deadlines and fixture
cleanup apply on failures. Run it without other local tests/builds; it does not
qualify shared-remote latency, cold database startup, task-claim correctness,
concurrent document mutation or every possible scope distribution.

Set `YGG_HOOK_BENCH_PHASES=1` for diagnostic tracing in that benchmark. Ordinary
commands can enable `RUST_LOG=ygg::knowledge::timing=debug`. Timings contain only
static phase names and elapsed microseconds, never document text, paths, session
IDs or credentials. The report retains per-request phases alongside total samples.
Nested timings are inclusive; do not add their percentiles. Tracing adds overhead,
so an instrumented run cannot replace uninstrumented release qualification.

The [initial detailed profile](../../docs/performance/okf-hooks-2026-10-09-phase-profile.json)
measured selection p95 483.63 ms: repository lookup 90.67 ms, rule selection
276.05 ms, receipt handling 143.49 ms (including revalidation 38.31 ms). Within
rule selection, index inventory was 55.41 ms, index reading 63.92 ms, and index
validation 148.39 ms. SQL usage recording was 18.31 ms versus local usage-cache
publication 166.89 ms. These are overlapping distributions, not additive totals.
All ordered output and 2,400 observations passed; added hook p95 still failed.
The [post-change diagnostic profile](../../docs/performance/okf-hooks-2026-10-09-disposable-cache-phases.json)
retains the same phase samples after disposable cache publication; use the
uninstrumented table below for the measured release gap.

Session receipts and the optional usage snapshot now use locked atomic replacement
without file/directory synchronization. During normal operation readers still see a complete
old or new file. A system/power failure can lose these disposable caches: eligible
rules may repeat and usage display falls back to migration baselines until refreshed.
Committed SQL counters remain authoritative for telemetry. Knowledge documents,
policy, migration baselines and recovery journals retain synced publication.

Measured on 2026-10-09, release build, macOS arm64, 10 logical CPUs,
PostgreSQL 18.3 (Homebrew), 100 samples per mode:

| Optimization stage | SQL p95 | OKF p95 | Added p95 | 50 ms gate |
| --- | ---: | ---: | ---: | --- |
| Global writer lease | 251.04 ms | 1,781.85 ms | 1,530.81 ms | Failed |
| Session lease + shared compatibility lease | 251.06 ms | 1,568.37 ms | 1,317.31 ms | Failed |
| Per-query file-pattern reuse | 262.44 ms | 637.42 ms | 374.98 ms | Failed |
| Pattern reuse + moved warm index rows | 259.90 ms | 610.15 ms | 350.24 ms | Failed |
| Above + batched usage recording | 207.08 ms | 569.17 ms | 362.09 ms | Failed |
| Above + disposable cache publication | 210.95 ms | 467.02 ms | 256.07 ms | Failed |
| Above + kind-specific indexes (initial run) | 231.56 ms | 298.33 ms | 66.78 ms | Failed |
| Kind-specific indexes (final verification) | 227.02 ms | 320.01 ms | 92.99 ms | Failed |
| Selected-UUID point checks, individual candidates | 307.08 ms | 345.29 ms | 38.21 ms | Passed |
| Individual point checks, unchanged repeat | 317.15 ms | 334.80 ms | 17.65 ms | Passed |
| Batched selected-UUID checks | 551.26 ms | 344.01 ms | -207.25 ms | Passed |
| Batched checks, unchanged repeat | 279.39 ms | 387.96 ms | 108.57 ms | Failed |

Raw samples: [before session locks](../../docs/performance/okf-hooks-2026-10-09-before-session-locks.json),
[with session locks](../../docs/performance/okf-hooks-2026-10-09-session-locks.json),
[with pattern reuse](../../docs/performance/okf-hooks-2026-10-09-pattern-cache.json),
[with warm row moves](../../docs/performance/okf-hooks-2026-10-09-pattern-cache-index-moves.json),
[with batched usage](../../docs/performance/okf-hooks-2026-10-09-batched-telemetry.json),
[with disposable cache publication](../../docs/performance/okf-hooks-2026-10-09-disposable-cache.json),
[initial kind-specific indexes](../../docs/performance/okf-hooks-2026-10-09-kind-index.json),
and [final kind-specific indexes](../../docs/performance/okf-hooks-2026-10-09-kind-index-final.json).
Selected-UUID reports: [individual checks](../../docs/performance/okf-hooks-2026-10-09-selected-uuid.json),
[individual repeat](../../docs/performance/okf-hooks-2026-10-09-selected-uuid-repeat.json),
[batched checks](../../docs/performance/okf-hooks-2026-10-09-selected-uuid-batch.json),
and [batched repeat](../../docs/performance/okf-hooks-2026-10-09-selected-uuid-batch-repeat.json).
The first batch run had a SQL tail spike; its negative delta does not demonstrate
an equivalent speedup. The unchanged repeat failed, so selected-UUID checks do
not yet establish repeatable compliance with the latency target. A separate
[phase profile](../../docs/performance/okf-hooks-2026-10-09-selected-uuid-batch-phases.json)
measured index-inventory p95 10.25 ms, index-read 11.68 ms and index-validation
25.84 ms; repository discovery was 125.47 ms and rule selection 122.56 ms.
These nested, instrumented measurements are diagnostic only and must not be summed.

All runs verified identical rule output and all 2,400 usage observations per mode
(including warmup). These are diagnostic runs on one host, not a cross-platform
latency guarantee. Independent session leases remove one contention source, but
the release's SQL-relative latency requirement remains **unmet**. Batching reduces
usage recording from about 80 calls for 20 fresh applications to two calls, but
this run does not show improvement in the SQL-relative gap.

Kind-specific index qualification retains a [diagnostic profile](../../docs/performance/okf-hooks-2026-10-09-kind-index-phases.json).
Two subsequent inventory-allocation experiments were reverted because they did not
improve measured inventory time or establish an end-to-end benefit. Their raw
[stack-format/counter-map run](../../docs/performance/okf-hooks-2026-10-09-kind-inventory.json),
[direct-entry run](../../docs/performance/okf-hooks-2026-10-09-stream-inventory.json)
and [direct-entry phase profile](../../docs/performance/okf-hooks-2026-10-09-stream-inventory-phases.json)
remain available; added p95 was 82.78 ms and 116.79 ms in the uninstrumented runs.
These experiments are not part of the current implementation.

Candidate indexing now enumerates filenames only for the requested kind. Before
loading selected documents, fresh descriptor-relative existence checks resolve
those UUIDs across both kinds and every freshly discovered repository scope,
including unregistered scopes. Rule candidates share one batch inventory; prime
notes check candidates individually until enough current notes are found. Final
injection revalidation checks the batch again. Malformed files, dangling symlinks
and directories occupying a canonical UUID name still make that UUID ambiguous;
an inaccessible scope suppresses the batch. No directory timestamp or persisted
membership cache authorizes injection. Every selected body is read and hashed.
Point checks scale with selected IDs times scopes, so the small-selection fixture
does not establish performance for corpora with many scopes or large selections.

File-pattern results are reused only within one immutable query, with at most 256
keys and 64 KiB of pattern text retained. Every rule still undergoes its own scope,
ownership, approval and fresh-byte checks; invalid patterns remain errors. Warm
index reads move validated rows into the candidate list instead of cloning profiles
and rebuilding a second map. Changed indexes serialize a sorted view of borrowed
rows; live inventory/fingerprint checks and selected-document revalidation remain.

Initial scan-only measurement (2026-10-08, engine at `cbed794`, release build,
macOS arm64, 10 logical CPUs, 24 GiB RAM):

| Operation | p50 | p95 |
| --- | ---: | ---: |
| Matching rules plus revalidation | 7,349.5 ms | 8,211.0 ms |
| Prime notes plus revalidation | 6,862.8 ms | 7,815.9 ms |

The 60 samples each selected 6,400 body bytes. The full run, including preparation
and warmup, took 63.44 seconds. These results identify the full-corpus read path as
a substantial scaling problem; they do not measure the SQL-relative release gate.
Every selection currently parses the entire corpus, and every selected-document
revalidation inventories all filenames again. Disposable indexing must reduce
both costs while preserving duplicate-UUID detection and fresh selected-file
approval, scope, deletion, expiry and policy checks. Retain this fixture when
comparing the indexed implementation.

### Batched selection revalidation

Use `revalidate_rules` / `revalidate_notes` for an injection batch. They inventory
UUIDs once for the batch, then read, hash and parse each selected file and check
current policy, ownership, matching, freshness and approval. Missing, ambiguous,
changed or ineligible selections are excluded; malformed selections produce
diagnostics while unrelated documents remain available. Repeated input IDs are
deduplicated. This is not a cross-document transaction snapshot, and filenames
are never taken from an earlier cached inventory.

The process benchmark now calls these batch APIs. Note and proposal sorting also
computes profile sort keys once per row. Further lookup optimization remains necessary; the persistent metadata index
below replaces whole-corpus parsing for rule and prime candidates. An experimental in-process parsed-document
cache did not improve this workload and is not retained.

With batched revalidation and cached sort keys, the same fixture on the same host
measured rule p50/p95 of 6,295.4/6,744.2 ms and prime-note p50/p95 of
6,022.2/6,439.9 ms (60 samples; 51.46 seconds total). Output remained 6,400 body
bytes per sample. Compared with the initial scan-only run, p95 improved about
18% for each operation. These are single-run observations, not a statistically
controlled regression threshold. Whole-corpus reads and parsing remain to be addressed
by the persistent index; the SQL-relative 50 ms target remains unverified.

### Persistent metadata lookup

Rule matching uses `.lookup-rules.json` and prime-note selection uses
`.lookup-notes.json` in the private bundle root. Each disposable version-2 header
pins the parser version and document kind, with a SHA-256 revision over that kind's
sorted path, file-fingerprint, document-digest and matching-metadata rows. Each
file is bounded to 64 MiB and replaced atomically. A wrong-kind or legacy cache
is rebuilt from documents; the old `.lookup.json` is ignored. Backups exclude all
three cache filenames.
It contains no Markdown bodies, activation evidence or unknown YAML metadata.
Notes/proposals requested for explicit browsing still use authoritative snapshots.

Each lookup inventories all live UUIDs across both kinds to exclude ambiguous
copies, including a duplicate note filename that conflicts with a rule UUID.
It then checks metadata only for the requested kind, relative to held directory
descriptors without following symlinks. Content diagnostics apply to the requested
kind; full explicit browsing still parses all documents. This avoids reading and
fingerprinting unrelated note metadata on every rule lookup.
It reuses one directory descriptor per contiguous scope/kind group, and opens
only changed or selected files through the existing containment checks. Cached rows are
reused only when device, inode, size, mtime and ctime (including nanoseconds) match.
Changed/new files are parsed and their exact byte digests recorded; deleted files
leave the index. Fingerprints are checked before and after reading changed files.
This assumes the local filesystem reports changes through those metadata fields;
it is not a shared/network-filesystem coherence protocol.

The index only selects candidates. Selected files are read and parsed afresh,
their exact digests must match the candidate rows, and current ownership, scope,
matching, freshness and corpus-bound approval are checked. Prime selects up to
five eligible notes after freshness checks. Injection adapters must still call
batched revalidation immediately before rendering. A concurrently changed
candidate is omitted until the next lookup rather than using older bytes.

Missing, corrupt, checksum-mismatched or incompatible indexes rebuild from the
bundle. Cache-write failure does not fail a valid read. Concurrent builders may
publish older observations, but every reader checks current file fingerprints;
cache publication never mutates authoritative documents. The lookup cache may be
deleted at any time and should be excluded from authoritative exports and Git
transport. OS timestamp checks and per-file reads do not create a transaction
snapshot across independently edited files.

On the same 10,000-document/20-process fixture, persistent metadata lookup with
anchored `fstatat` checks measured rule p50/p95 of 299.9/419.0 ms and prime-note
p50/p95 of 314.5/403.8 ms (60 samples, 6,400 selected body bytes per sample;
6.56 seconds including setup/warmup). This is about 94% lower p95 than the batched
full-scan run. Directory inventory, metadata checks and index decoding still scale
with corpus size. The SQL-relative 50 ms additional hook-latency gate is not yet
measured or satisfied by this engine benchmark; retain the exact fixture while
optimizing these remaining costs and integrating actual commands.

## Legacy model adapters

`legacy` converts the `Memory` / `Learning` JSON models to OKF and back without a
database connection; the output adapters are the CLI's JSON contract and the import
adapters remain for fixture round trips. Import requires explicit database/corpus IDs plus source
repo-to-portable-repo and source-user-to-current-user mappings. An empty legacy
user needs its own explicit mapping; unmapped scope or ownership never becomes
global/current-user data. UUIDs, exact text, timestamps, nullable context/creator,
source and approval evidence survive the fixture round trips.

Only guarded migration of the selected database may call the activation-preserving
learning importer. Active source rows get explicit `Legacy` evidence bound to the
new document digest and corpus, preserving missing actor/time. Pending historical
approval fields remain provenance, not activation. Arbitrary non-object JSONB
scope tags retain their original representation in provenance and have no matching
keys; conflicting later object-tag edits require resolving that representation.
Unknown status/source values and documents exceeding parser bounds are errors.

API adapters emit the existing JSON model fields. They choose legacy repo IDs from
the document's current portable scope: original IDs survive same-database duplicate
mappings, while moves and deployment rebinding require an unambiguous current
mapping. `legacy_user_id` provides the separate SQL owner field omitted from those
JSON models. Multiple possible inverse mappings are errors, never arbitrary picks.
A digest-invalid rule is reported pending and cannot retain active approval fields.

Usage is returned separately with `(corpus_id, document_id)`, application count and
last-applied time. The API adapter requires an explicitly supplied matching usage
record and does not fabricate zero counts when telemetry is unavailable. Durable
telemetry storage/fallback and command-level outage behavior still require wiring.
These are model adapters, not the fenced cutover/reverse-import implementation:
rollback must additionally audit OKF-only fields for representability, inventory
all owners/scopes, and coordinate database generation/fences before writing SQL.

## Operational usage storage

Migration `20261008000001_knowledge_telemetry.sql` adds `knowledge_usage` and
`knowledge_applications`, keyed by corpus and document UUIDs without references
to the legacy knowledge-content tables. `telemetry::Telemetry` seeds the imported
baseline separately from subsequently observed applications. Identical seed
retries succeed; conflicting snapshots fail without changing counts. A seed can
arrive after observed events without resetting or double-counting them.

Application recording commits the unique application ID and counter update in
one transaction. Retries must reuse that ID; they retain the first timestamp and
return `false` instead of incrementing twice. Distinct events add once, and the
last-applied time is the maximum imported/observed time. This ledger does not
replace per-session injection deduplication and does not authorize an injection.
Application IDs must not be pruned while an old operation can still be retried.

Batches use two database calls: lock/upsert every usage row in sorted UUID order,
then insert unique receipts and aggregate only newly inserted observations. The
single-record path takes the same usage-before-receipt lock order. Existing and
missing rows share one order; mixed batches cannot lock newly created rows ahead
of earlier existing rows. Duplicate IDs retain their first timestamp, including
within a batch, and returned per-application totals retain their intermediate
values. Missing imported baselines remain excluded from published cache totals.
Both receipt and counter changes roll back together on counter/total overflow.

Totals use signed 64-bit counters. Conversion to legacy `Learning` JSON rejects
values outside its signed 32-bit contract rather than wrapping or clamping them.
A missing usage row remains explicitly absent. No knowledge text, approval or
matching metadata is stored in these tables.

This repository returns database failures to its caller. Command/hook integration
must isolate optional telemetry failures from acknowledged document writes and
injection, and supply a defined last-known-statistics path during database outage.
That integration and the cutover's fenced baseline manifest remain unfinished.
The SQL tests use private schemas in the isolated test database and cover 20-client
races, seed conflicts, corpus isolation, failed-update rollback and wide totals.

## Corpus backup component

`KnowledgeStore::backup` creates an immutable snapshot directory containing
`corpus/` and `knowledge-backup.json`. It preserves exact bytes, unknown files and
empty directories. Identity/policy lives in a separate `IdentityRegistry` directory:
a deployment backup must snapshot that directory as well as its document bundle,
and record both returned revisions alongside its consistent PostgreSQL dump.
This library component alone is not a complete Yggdrasil deployment backup.
`db backup` pairs bundle and policy snapshots with a database dump. `db restore`
restores exact corpus bytes into a new destination and validates the database.
For a selected corpus, it changes only the archived runtime binding's bundle path
after verifying its identities, mappings, generation and phase against the backup.
Version 2 restore receipts record the original and restored binding digests;
`db switch` independently derives the same change and rejects other policy edits.
The source/archive remain unchanged, and a fenced selection stays fenced.
Explicit `db switch` selects a validated restored deployment. Binary upgrades remain pending.

New deployment backups use manifest version 2 and include a digest-bound, private
`configuration.json`. It preserves exact user `config.toml` and `.env` text,
relevant inherited environment overrides, captured dotenv expansion results,
effective deployment paths/URLs, and any explicit backup policy-directory override.
These inputs can contain credentials: the component is mode 0600 and reports expose
only its size and digest. Configuration files must be owned regular files without
symlinks or hardlinks, at most 1 MiB each. Dotenv expansion is bounded; the serialized
snapshot is limited to 16 MiB. Offline verification never reads the original files
or re-expands variables using the verifying machine's environment.

Forward migration and current-bundle rollback compare the saved configuration
with the selected deployment and live source inputs before fencing or activation.
Changed inputs require a new source backup. Restore retains exact configuration
bytes as private `source-configuration.json`, bound to its receipt; `db switch`
checks that evidence. Restore does not activate the source endpoint or overwrite
user configuration: target configuration and credentials are explicitly supplied.
Referenced certificate files, password files, and external provider assets are
not bundled; their provisioning remains the operator's responsibility. Version 1
archives remain verifiable and restorable, but have no configuration component and
cannot provide the additional source-configuration evidence.

The snapshot holds the export and writer leases, completes pending scope-move
recovery, copies through held directory descriptors, and rereads source hashes
before publication. Quiesce external editors: cooperative locks cannot establish
an atomic snapshot against tools that ignore them. Root writer/export locks and
the disposable lookup cache are excluded. Symlinks, hardlinked files and special
files fail rather than being followed or silently skipped. Limits are 100,000
entries, 32 directory levels, 1 GiB per file and 4 GiB total; oversized input fails.
Malformed document bytes are retained for recovery, not treated as valid rules.

Files and directories are private and synced before exclusive atomic publication.
An existing destination is never replaced. Interrupted or failed attempts retain
private `.knowledge-backup-<UUID>` staging directories; a new attempt uses a new
stage. `KnowledgeBackup::verify` checks the complete inventory, hashes and revision,
including unexpected and missing files. Checksums prove integrity against the
manifest, not provenance or permission to activate imported rules. Tests kill real
backup processes before and after publication, verify retry behavior, preserve a
separate registry's identity/trust, reject tampering, and keep reading the held source if its pathname is replaced.

### Local command selection

`remember` uses an offline OKF adapter with the same note JSON contract.
Knowledge paths resolve independently of database validity, with the same profile,
user configuration and environment precedence as deployment commands.

A selected context requires a versioned OKF binding (`runtime.json` in the policy
directory) with a positive storage generation, supported minimum client protocol,
canonical bundle path, corpus UUID, optional legacy user/repository mappings and
optional agent-name mappings. Repository bindings must agree with the identity
registry. Each command holds a shared `.selection.lock` lease in the policy
directory. Malformed, fenced, unsupported or mismatched bindings fail; an absent
binding means knowledge is uninitialized and every knowledge command reports
`run ygg init`. There is no SQL fallback. The selection lease is disposable and
excluded from corpus/policy backups.

`ygg init` (and `ygg knowledge init [--json]`) mint the selection for a fresh
installation: they create the private corpus and policy directories, initialize
`identity.json` with a new trusted corpus, advance the database's
`knowledge_storage` marker from `sql` through `fenced` to `okf` bound to that
corpus (migration-owner rights), and write `runtime.json` with no legacy
mappings and the current user mapped to itself. Re-running verifies the existing
selection and changes nothing. A database already bound to another corpus is
refused: restore that corpus's backup instead.

The runtime user (`YGG_USER`, else `whoami`) is its own portable identity unless
the binding maps it explicitly. A checkout that resolves to no repository binding
is bound on first use (worktrees and clones of a bound repository share its ID; a
changed origin or contradictory URL/path still requires explicit repair). Notes and
rules written in a repository without a legacy mapping report the portable
repository UUID as `repo_id`; connected task claims accept a task whose database
`repo_id` is that portable ID. An unmapped/non-Git cwd is never silently promoted
to global scope; use `--global` deliberately.

Offline note creation preserves UUID/provenance fields and uses explicit agent
bindings without querying PostgreSQL; unknown agents retain null provenance.
Listing retains the `count`/`results` JSON envelope, reports malformed-document
diagnostics separately, and performs no telemetry or database access.

### Offline learning commands

All ordinary `learn` actions now dispatch through the selected OKF context before
constructing database configuration. Create/manual-pending/propose retain their
status/source distinction, exact rule text, context, scope tags and legacy JSON
fields. Manual activation evidence remains in the document but does not invent a
separate `approved_at`/`approved_by` action in API output. Explicit listing can
browse untrusted or stale active rules; automatic injection still requires trust,
freshness and matching approval digests. Pending triage includes edited rules
whose prior approval digest no longer matches.

Approval without an agent argument or `YGG_AGENT_NAME` is a human action. An
explicit or environment-selected agent must resolve to an identity binding and
be present in the registry's approval-lead set. An unknown agent cannot fall back
to human authority. This is policy for cooperating callers, not protection from
the owning OS user who can edit their policy or environment. Approve/reject/delete
load the selected revision and use the store's conditional mutation; learning
commands refuse note UUIDs. Creation and approval never depend on telemetry.

`usage-baseline.json` in the policy directory records the validated migration's
original totals using `runtime::UsageSnapshot`. Optional `usage-snapshot.json`
uses the same schema for last-known operational totals. Both are identity-bound
and bounded to 64 MiB. A corrupt optional cache falls back to migration baselines;
a cache that predates the baseline cannot reduce its count or last-applied time.
Imported rules without any recorded totals produce a repair diagnostic rather
than fabricated zero counts. New rules begin at zero. CLI listing labels these
values as last-known on stderr while preserving the JSON schema. Successful connected rule emissions refresh the optional cache as described below;
explicit offline browsing does not query PostgreSQL. Neither usage file is part of
a rule's activation digest.

`ygg knowledge refresh-usage [--json]` explicitly fetches other clients' committed
usage for the current user's active and pending local rules. It verifies database,
corpus and storage generation under a shared migration lease, queries at most
10,000 document IDs, then releases SQL before merging the private optional cache.
The connection/query attempt is limited to five seconds. Missing SQL rows,
incomplete imported baselines and counters outside the legacy integer range are
reported separately and retain their prior local values. Concurrent or older
observations cannot lower cached totals. This command does not record applications,
change documents/approval, repair missing migration baselines, or spool offline
usage. Ordinary listing and hook retrieval keep their existing offline behavior.

### Prime during coordination outages

`prime` resolves local knowledge selection independently of coordination. With a
selected or invalid/fenced OKF binding it never reads legacy SQL notes. It bounds
the complete coordination attempt to three seconds, then reads and immediately
revalidates local notes while retaining the selection lease through output.
The existing five-note cap, newest ordering, snippet formatting and explicit
global labels remain unchanged. Trust revocation, deprecation, deletion and
malformed files are checked independently of database health. If the cwd has no
valid repository binding, only explicitly global notes are eligible and a scope
diagnostic explains the omission.

Healthy coordination still supplies agent/task/lock state. Handoff reads have a
separate half-second bound and an unavailable indicator; no local handoff copy is
invented. Degraded output states that coordination and handoff were not loaded,
without echoing database credentials or claiming that shared locks work offline.
SessionStart and PreCompact inherit this path. The dashboard no longer queries
an unused SQL learning count; database-health views inspect relation metadata only.

### Edit-time injection and session receipts

PreToolUse now resolves selected OKF rules before coordination work. Edit, Write
and NotebookEdit retain file/agent matching and emit only file-scoped, active,
trusted, current rules. Selection is revalidated before emission; pending,
changed, deleted or revoked rules cannot be authorized by session state. An
unmapped cwd permits only explicitly global rules. A selected or invalid/fenced
binding never falls back to SQL rules. Healthy coordination still records tool
use, sends heartbeats and acquires the shared database lock; its complete attempt
is bounded to three seconds independently of local injection.

Private policy `.sessions/` receipts use a SHA-256 key over corpus, mapped user
and the exact session ID, avoiding path traversal and lossy identifier collisions.
The directory is opened relative to the held policy descriptor without following
symlinks. Waiting for a paused cache writer is bounded to two seconds, after which
the hook uses the same duplicates-possible fallback as a cache write failure.
An exclusive lease per receipt serializes claims for the same session while
unrelated sessions revalidate concurrently. A shared legacy writer lease keeps
compatibility with older global-lock publishers; both acquisitions share the same
two-second wait budget. Writers revalidate eligibility under the receipt lease and
durably publish the last emitted approval digest per UUID.
Receipts are bounded to 10,000 rules and 1 MiB per session; raw session IDs are
limited to 4096 bytes and are not retained. Changed, reapproved content can fire
again in the same session. Display-only changes do not reset deduplication.

A receipt is committed before stdout emission. A crash in between can suppress an
undelivered rule for that session; this is best-effort deduplication, not an
exactly-once delivery protocol. Corrupt receipts reset with a diagnostic. If the
cache cannot be used or written, the hook freshly revalidates eligible rules and
emits them with a duplicates-possible diagnostic. Missing session IDs skip
deduplication. These disposable receipts are excluded from backups, contain no
exclusive knowledge or activation evidence, and may be removed explicitly when
sessions are no longer active. Automatic session-cache retention cleanup remains unfinished.

### Connected task-claim injection

Task claims retain PostgreSQL coordination and route selected knowledge to OKF.
The task's explicit database repository UUID is mapped to portable scope; the cwd
cannot substitute another repository. Before matching, the local selection's
source database, corpus and generation must agree with the connected public
storage marker, which must select OKF and support this client. Missing mappings,
fenced/invalid selections and mismatched markers omit instructions with a concise
diagnostic; they never fall back to frozen SQL and do not undo a successful claim.

The operation holds the local selection lease and a shared database migration
lease through emission, acquiring them in that order. Connected lease acquisition
is bounded to three seconds. Migration publication must use the same lock order.
The advisory lease stabilizes the marker without taking a tuple lock, avoiding a
reader/transition-trigger lock inversion. File order, file/rule predicates,
agent/kind matching and within-call UUID deduplication retain the SQL behavior;
when no paths are mentioned, only rules without file or rule scope are included.
Current document bytes, activation, trust and freshness are revalidated before
formatting. Optional usage recording follows output, as described below.

### Optional connected usage observations

After printing selected rules, task claims and healthy edit hooks record one
application UUID per emitted document. Hook session suppression produces no new
application. The connection must match the selected database, corpus, generation
and client protocol. Task claims reuse their selection transaction; hooks acquire
a separate verified lease only after local output and coordination. They do not
write counters to frozen SQL learning rows or to authoritative documents.

Batches use consistent document lock ordering and commit all increments together.
Application IDs make a retry of the same batch idempotent. The CLI itself does not
retry ambiguous commits, spool offline events or promise exact delivery counts:
a crash/outage after output can leave an unrecorded observation. Database recording
is bounded to 500 ms, with an additional 500 ms acquisition bound for hooks.
Counter failures never undo claims, suppress rules or block knowledge writes.

Only committed totals update `usage-snapshot.json`, after the database lease ends.
The publisher serializes with a two-second writer-lock bound, prevents concurrent
count/time regression, rejects wrong corpus identity and recovers malformed cache
state. Publication is capped at 10,000 documents and 1 MiB; oversized or unavailable
cache state leaves database observations intact. Values outside the legacy i32
contract are diagnosed rather than clamped. Imported documents cannot cache an
observed-only total until their migration baseline has been seeded. Baselines
remain separate, authoritative migration metadata; they are never guessed from
current counters. Broader cross-client refresh and automatic cache retention are
still separate work.

Persistent lock files are opened by existing inode or exclusive creation, with a
bounded retry if another creator wins. This avoids a reproduced concurrent first
creation failure on macOS without replacing a held lock. The usage-cache fixture
races twenty independent contexts over five fresh directories.

### Shared Git transport and service routing

A selected runtime may carry private policy `shared.json` with
`{"version":1,"remote":"…","branch":"knowledge"}`. Its configured knowledge
directory is then a dedicated Git transport cache. Selection/configuration is
still published by the unfinished validated cutover workflow, not by editing
these files as a supported migration. No existing private files are uploaded.
A hosted database alone never selects shared knowledge.

The ordinary note/rule service uses complete confirmed Git trees through disposable
private views. It never checks out repository files, executes their hooks/filters,
or follows their remote links. Explicit commands and session entry fetch the configured branch; subsequent edit
hooks reuse a confirmed snapshot for up to 60 seconds. Automatic note/rule
selection and revalidation enforce a maximum 60-second
confirmed-snapshot age. Explicit browsing can retain cached content during outage.
A failed mandatory session refresh omits automatic instructions even if an older
cache is young. A fetched but unusable newer tree marks the older cache superseded
even within that window. Private local mode remains independent of Git and PostgreSQL.

Mutations fetch first, compare every affected path's exact SHA-256 or required
absence, construct a complete tree with Git plumbing, and push without force.
Scope moves change both paths in one commit. Definitive non-fast-forward rejection
retries at most three times, only while the affected expected digests still match;
conflicting text, scope or approval is never merged. UUID-path collisions are
rejected. A mutation is acknowledged only after its commit is confirmed reachable
from the fetched authoritative branch. An uncertain response retains a durable
pending commit and prevents another write from silently duplicating it.
`ygg knowledge sync --confirm-pending` can confirm that commit after connectivity
returns; it never blindly republishes an unconfirmed draft.

`ygg knowledge pending --json` inspects the retained commit, its parent and changed
paths with before/after digests without fetching or needing a usable read cache.
`ygg knowledge recover FULL_COMMIT --retry` explicitly retries that exact intent,
rechecking every original affected digest against the current remote. A conflict
keeps the pending draft. `--discard` instead retains the draft under
`refs/ygg/drafts/FULL_COMMIT`, refreshes the confirmed cache and clears the journal;
it does not change remote content. Both actions first fetch and check whether the
original commit already reached the remote; if so, they only confirm it. Neither
action resolves an unavailable remote. A changed pending commit requires inspection
again. Archived refs have no automatic expiry yet. `knowledge sync` can also repair
a missing read-cache pointer. Recovery still validates selected corpus, generation,
protocol, canonical path and identity bindings.

New transport caches initialize in unique private `.init-UUID` directories. The
client validates the bare repository, syncs the bounded generated tree, atomically
renames it to `objects.git` and syncs the parent. A surviving initializer from a
killed client can finish only its abandoned stage; subsequent attempts never reuse
or delete that directory. Existing invalid object stores, and missing stores with
retained snapshot/pending state, fail without replacement. Abandoned initialization
stages are excluded from shared backups; automatic cleanup remains future work.
The fault fixture kills the client while its initializer is alive, recovers with a
new attempt, then resumes the orphan and checks the published directory inode and
configuration remain unchanged.

Each host has its own cache and two-second cooperative transport-lock acquisition
bound. Git invocations have a 30-second deadline, private bounded output files,
redacted failure messages and process-group cancellation. Existing Git/SSH
credentials are used; repository/index/config environment overrides are cleared.
The client disables hooks, checkout filters, replacement objects, automatic Git
maintenance and implicit signing. Git authorship is the Yggdrasil client identity;
human approval remains the separately recorded document evidence.

Current transport limits are 20,000 regular non-executable files, 64 MiB batch
input/output and 32 path components. Hidden path components, symlinks, submodules,
executables and traversal are refused. Raw document bytes and unknown metadata
survive commits unchanged. These snapshot/output limits are not a quota on fetched
Git history; remote transport and large-repository resource validation remain release
work. Shared-network latency is measured separately from private local lookup.

Backups acquire the transport lease before export/writer leases, retain the bare
objects, confirmed snapshot pointer, remote binding and uncertain-publication
journal, and omit transient views/command files/locks. Restored caches preserve
confirmed bytes for offline browsing. Deployment restore rebases the runtime's
canonical bundle path with retained evidence; shared/fleet move qualification
remains open. Automatic Git/cache
retention cleanup and a supported cutover/configuration publisher remain unfinished.
