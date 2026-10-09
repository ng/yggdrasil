# OKF engine implementation

The document, matching and local-store modules operate without a database
connection. Command dispatch still uses legacy SQL until the guarded cutover is
implemented. These primitives alone do not complete ADR 0019.

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
not a full disk or arbitrary filesystem failure model.

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

The SQL-relative hook latency gate remains unfinished. Explicit browsing snapshots scan the Yggdrasil
layout directly; rule/prime candidate lookup uses the disposable index below. Approval must still be revalidated immediately
before injection; never treat a previously read document as current authority.
Shared Git transport, migration and CLI integration build on this
layer and remain separate release gates.

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

Raw samples: [before session locks](../../docs/performance/okf-hooks-2026-10-09-before-session-locks.json),
[with session locks](../../docs/performance/okf-hooks-2026-10-09-session-locks.json),
[with pattern reuse](../../docs/performance/okf-hooks-2026-10-09-pattern-cache.json),
[with warm row moves](../../docs/performance/okf-hooks-2026-10-09-pattern-cache-index-moves.json),
[with batched usage](../../docs/performance/okf-hooks-2026-10-09-batched-telemetry.json),
[with disposable cache publication](../../docs/performance/okf-hooks-2026-10-09-disposable-cache.json),
[initial kind-specific indexes](../../docs/performance/okf-hooks-2026-10-09-kind-index.json),
and [final kind-specific indexes](../../docs/performance/okf-hooks-2026-10-09-kind-index-final.json).
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
These experiments are not part of the current implementation. Directory inventory
still reads all live names, preserving canonical spelling and global duplicate checks.

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

`legacy` converts existing `Memory` / `Learning` models to OKF and back without a
database connection. Import requires explicit database/corpus IDs plus source
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

## Compatibility guard and legacy write fence

Migration `20261008000002_knowledge_storage_guard.sql` creates one database identity,
monotonic storage generation, minimum client protocol and `sql`/`fenced`/`okf` phase.
It starts in `sql` without changing the selected backend. Memory and learning
repositories check protocol/phase inside a transaction and retain the shared
advisory lease through the query and commit. The dashboard's direct learning count
uses the same guard, separately from coordination counts.

`fenced` permits compatible SQL reads but rejects knowledge writes; `okf` rejects
legacy reads and writes through the compatibility guard. Statement triggers fence
INSERT, UPDATE, DELETE and TRUNCATE on the old content tables, including ordinary
older clients that do not call the guard. Phase changes acquire an exclusive lease
before marker row locks and wait for existing operations. Row-locking marker reads
reject stale repeatable-read snapshots after a committed transition. Marker changes
require the next generation, stable database ID and nondecreasing protocol; direct
SQL-to-OKF and OKF-to-SQL switches are rejected. Deletion/truncation of the marker
is rejected. Guard functions use fixed qualified objects and a fixed search path;
security-definer access permits read-only clients to lock/read the marker without
granting them permission to update it.

This is a compatibility foundation, not a ready-to-run cutover. SQL remains the
default; an explicitly selected local binding routes knowledge to OKF. Switching
only the database marker cannot publish that binding. Guarded repository calls
discover the current generation; `guard::legacy_transaction` also accepts an
expected generation. Export/publication and owner-only reverse-apply primitives
exist, but fleet compatibility inventory, complete backup/parity evidence and
coordinated configuration/generation publication still need the cutover workflow.
Never manually switch the marker as a substitute for those steps.

`ygg knowledge clients [--json]` reports the current database's live client
connections, excluding its own inspector. Application pools register each physical
connection after migration `20261009000003_knowledge_clients.sql`; the server
binds protocol, binary version and process UUID to its own PID and authenticated
role. Audit joins the declared backend start time to the actual server lifetime.
Missing registrations and protocols below the marker's minimum are blockers and
produce a nonzero command exit, with JSON observations still printed. Connections
opened before this migration remain unregistered until restarted. Runtime roles
have function execution permission, not direct registry write permission. Each
registration removes rows for ended connections and stale reuse of its own PID.

The audit requires READ COMMITTED so a reused snapshot cannot hide a generation
change. The CLI uses a dedicated read-only operator transaction, never application pool
registration or managed startup. External deployments use `YGG_DATABASE_OWNER_URL`
when configured, validate that it names the runtime endpoint, and require registry
SELECT permission plus `pg_read_all_stats` or superuser visibility. Managed mode
uses its existing private cluster's bootstrap connection only for this read-only
inspection. Insufficient statistics visibility fails; hidden sessions are not
silently omitted. Reports omit query text, network addresses and arbitrary
application names.

Registrations are client compatibility declarations, not signed executable
attestations. A clean live report does not account for disconnected/offline hosts,
external editors, clients that connect later or transaction-pool backend reuse.
Verify session affinity and the complete participating-host inventory separately,
then recheck under the migration lease before transition. Registration is
independent of that lease so coordination connections remain available during
knowledge cutover (forward migration `20261009000004`). The lease stabilizes the
knowledge phase; it does not prevent new client connections. Connection admission
and the complete host inventory need separate operator verification. Offline
local-fence evidence and full fleet cutover orchestration remain required.

Unaware older binaries can still SELECT frozen legacy tables: upgrade or retire
them before cutover, as required by the plan. These triggers also do not constrain
an administrator who disables triggers or changes table/function ownership. The
old-client tests cover ordinary SQL writes, including stale transaction snapshots;
they do not prove fleet upgrade, filesystem publication or lossless rollback.

## Migration dry run

`ygg knowledge migrate --dry-run --json` inventories all SQL notes and learnings
from one repeatable-read snapshot under the compatibility guard. It reports the
source database identity/generation/phase, source owners and repositories, row
fingerprints, proposed paths/document digests and unresolved issues. It reads all
row fields but reports fingerprints rather than copying private text into the
report. No bundle is created, counters seeded or storage marker changed.

Migration requires `SHOW server_encoding` to return `UTF8`. A UTF-8 client
connection alone is insufficient: SQL_ASCII treats the UTF-8 bytes of a character
as separate characters for `LIKE '_'`, changing rule scope after conversion.
Dry runs, exports, forward activation and reverse import reject other server
encodings with an explicit diagnostic before changing knowledge state. The SQL
guards also reject activation and fenced reverse writes on incompatible databases.
Ordinary legacy SQL access and pre-activation abort remain available; no automatic
encoding conversion occurs. Managed clusters are initialized with UTF-8. Encoding
preservation and admission across deployment backup/restore/move remain a separate
qualification requirement.

Supply `--mapping-file /absolute/path/mapping.json` to verify conversions. The
JSON object has exactly these fields:

```json
{
  "database_id": "SOURCE-DATABASE-UUID-FROM-REPORT",
  "corpus_id": "EXPLICIT-TARGET-CORPUS-UUID",
  "repos": {"LEGACY-REPO-UUID": "PORTABLE-REPO-UUID"},
  "users": {"": "EXPLICIT-OWNER-FOR-EMPTY-LEGACY-IDS", "alice": "alice"}
}
```

Replace the placeholder UUIDs with actual IDs. The mapping file is bounded to
1 MiB; duplicate source keys and unknown fields are rejected. Database identity
must match the selected source; an already fenced source must also match the
target corpus. Unmapped owners/repositories, unsupported SQL fields, cross-table
UUID collisions, parser limits or failed field-by-field round trips make the
report unresolved. UUIDs, nullable fields, pending/approval state, arbitrary JSONB
tags and usage totals are checked via the same legacy adapters.

JSON output remains on stdout even when row verification fails; exit status is
nonzero for unresolved reports. Choose `--dry-run` for assessment or an explicit
`--plan` and `--journal` for execution. `rows_verified: true` only proves this snapshot's rows passed
conversion; it does not establish fleet upgrade, frozen writes, backups, semantic
scope-fixture parity, filesystem publication or rollback readiness. A later fenced
export must reread and validate its own manifest rather than trusting this report.
The CLI tests create/drop their own database in the disposable PostgreSQL cluster
and verify that neither the knowledge directory nor storage mode is changed.

## Fenced export staging

`export::stage` prepares a separate private directory from an already fenced SQL
source. It first verifies the complete inventory and every adapter round trip,
then durably records `.export-plan.json` before writing documents. The immutable
plan binds database identity, generation, target corpus, full explicit mappings,
source/document digests, document keys and separate usage baselines. Artifacts are
bounded to 64 MiB; exceeding that limit is an error before staging begins.

A second guarded snapshot must match the planned generation, phase, row set and
each source/document/usage record. Missing staged documents are written through
the conditional durable store; matching files are retained and independently
changed files are never overwritten. An OS export lock serializes cooperating
exporters. A new or empty private directory can start an export; unrelated existing
contents without a matching intent are rejected. Leftover store temporary files
from an interrupted initial control-file write do not prevent resumption.

After rereading the staged inventory and all document digests, the exporter writes
`.export-complete.json` bound to the plan digest. `export::verify` checks the current
files, unique UUIDs, keys, saved mapping identities and usage identities each time;
receipt presence alone cannot validate an edited stage. A repeated identical
export is idempotent. Different source generations or manifests cannot reuse a
stage, even when the document bodies happen to match. Partial stages retain their
intent and existing data for resumption or explicit operator inspection.

`export::publish` revalidates every current SQL source row, document digest and
usage baseline against the completed manifest in one repeatable-read transaction.
It requires the same fenced database, generation and corpus, and retains that
transaction's shared generation lease through publication. A one-connection pool
works because inventory and publication use the same transaction.

Publication captures a retained exact-byte recovery archive. Its complete file
inventory must contain only manifest-listed documents and the exact plan/receipt;
unlisted files are rejected. Validation never opens mutable document readers or
creates lock files inside the immutable archive. The existing descriptor-anchored
restore path then copies, syncs and exclusively publishes the complete bundle into
an absent destination. Retries verify the retained archive and exact existing
destination; independently edited bytes are never replaced. A final database query
and transaction completion are required before acknowledgment. If that connection
fails, the unselected published directory and archive remain for inspection.

These are library operations pending the validated cutover command; the public
migration CLI still requires `--dry-run`. Publication does not select OKF, seed
telemetry, alter the SQL phase or configure trust. Fleet upgrade, source database
backup, scope-fixture parity, local generation/configuration activation and reverse
import remain required. Tests cover a one-connection pool, archive-only resumption,
unlisted files, independent target edits, incomplete archives and changed source
generations. They do not substitute for full killed-process migration, coordinated
configuration publication or rollback release gates.

## Transactional forward activation

`forward::activate_on` performs the database half of cutover on the coordinator's
existing READ COMMITTED transaction. The migration owner takes an exclusive
generation lease, rejects unregistered/outdated live clients, checks the source
column inventory even for empty tables, and revalidates every manifest row,
document digest and usage baseline. It seeds imported telemetry without changing
observed applications, advances the fenced marker to the next OKF generation,
and inserts an immutable receipt bound to the operation UUID and manifest digest.
The receipt and all database changes commit together. A savepoint removes partial
seeding even if the caller commits its outer transaction after a validation error.

After an uncertain commit, the same operation/manifest returns
`PreviouslyActivated` only when the receipt, selected generation, complete frozen
source and imported baselines still match. Retry does not reseed missing or changed
baselines, reset post-cutover observations, or reactivate a later fenced generation.
Source-table locks allow a queued legacy writer to finish waiting on the generation
fence and fail, without inverting advisory/table lock order.

This is a library transaction step, not an operator cutover command. Its caller
must persist the operation ID before sending SQL, validate retained publication
and backup evidence, and retain filesystem/selection leases through commit.
The live audit is neither a full participating-host census nor an admission
barrier. Durable workflow resumption, offline-host quiescence, schema/constraint
parity beyond column names, local binding/trust publication and complete forward/
reverse rehearsals remain required before enabling the migration CLI.

## Durable private-host forward completion

`cutover::PrivateJournal` connects retained publication evidence, transactional SQL
activation and atomic host-local binding publication. Preparation requires a
published private export, paired corpus/policy archives and an explicitly prepared
fenced `runtime.json` with validated corpus/repository identities. It retains the
exact original/selected binding bytes, canonical paths, root identities, revisions,
manifest and operation UUID in a synced journal before any database activation.
Identical preparation reuses the UUID; preparation never waits for a busy journal
while holding source leases. Journal relocation and conflicting evidence fail.

Activation drains the local selection lease and reacquires the paired source
leases before calling `forward::activate_on`. It rechecks evidence before commit.
After commit it acquires a shared lease on the selected SQL generation before
publishing the saved binding; a transition during that gap leaves local access
fenced. Binding publication retains both filesystem writer leases and revalidates
the complete inventories with only the intended selection change permitted.
An uncertain SQL commit or interrupted local publication resumes the same journal.
The journal operation UUID names the publication temporary before it is created.
A partial temporary, including incomplete UTF-8, is recoverable only when it is a
byte prefix of the exact intended binding and every other source file still
matches its archive. A conflicting temporary or unrelated edit remains untouched;
recovery never ignores arbitrary temporary files.

If the exact selected binding already exists, recovery requires the matching SQL
receipt and generation. It does not restore or compare current OKF documents to
the frozen export: legitimate edits made after activation remain authoritative.
Local selected bytes without a SQL receipt cannot authorize activation. The
returned outcome describes the SQL step; `PreviouslyActivated` can also accompany
successful completion of a previously interrupted local publication.

This is one host's private-corpus completion path, not the fleet migration CLI.
The caller still must capture/validate source-database backup evidence, fence SQL,
verify full schema/scope parity, prepare identity/trust explicitly and coordinate
all hosts and external editors. Shared transport is rejected by preparation;
shared publication, remote admission/quiescence, reverse backend activation and
complete process-kill rehearsals remain orchestration work. Tests reconstruct the
committed-SQL/fenced-local crash boundary, resume through the receipt, make an
actual offline `remember` write and verify later retries preserve its bytes.

## Reverse-import candidate validation

`reverse::build` constructs SQL row candidates from the **current** complete corpus
snapshot, not from the frozen export. It compares UUIDs against the saved export to
list deletions, accepts new documents, preserves current text and matching scope,
and requires explicit current usage for every rule. Totals below the recorded
migration baseline, missing usage, duplicate UUIDs, changed document kinds and
snapshot diagnostics block the candidate. No missing counter becomes zero.

The strict reverse adapters convert each row back into an OKF document and compare
its modeled content. Exact body, nullable fields, owner, scope, source, state and
approval actor/time must survive. They normalize the generated legacy provenance,
original repository hint and activation representation; current scope still needs
an unambiguous saved identity mapping. Manual creation evidence is retained in SQL
approval columns instead of being omitted as in the display-only JSON adapter.
Unknown document/profile/provenance/approval metadata, unsupported note fields and
invalid active approvals block rollback rather than disappearing or being silently
downgraded. Current edits must be reviewed or explicitly pending before reversal.

A candidate records current document revisions and the original export digest.
It is not an authorization or an applied rollback. The migration workflow must
capture authoritative current usage, retain the exact current bundle as recovery
evidence, quiesce writers, revalidate revisions under its leases, use the restricted
SQL migration bypass, validate restored rows and then publish the storage/config
transition. The complete orchestration and rollback rehearsal remain open.

`reverse::capture_on` builds a candidate using committed database usage in the
caller's READ COMMITTED transaction. It verifies the expected fenced generation,
holds the exclusive migration lease and a SHARE lock on the usage table through
transaction completion, and verifies each imported baseline against the export.
Missing or conflicting imported baselines block capture. A new rule with no usage
row receives zero only after the table lock proves that absence; unexpected
imported baselines on new rules and totals outside the legacy integer range fail.
It never reads the local usage cache. Keep the same transaction for `apply_on`;
the caller still owns filesystem quiescence, recovery evidence and activation.
These totals reflect recorded deliveries only: offline deliveries were not queued
and cannot be reconstructed during rollback.

`KnowledgeStore::backup_pair_retained` captures the corpus and separate policy
while retaining both writer/export leases (and shared transport leases when
present). The returned `PairedBackup` rechecks both complete source inventories
against the retained evidence without reacquiring locks. Destinations must be
outside both sources and separate from one another. Dropping the guard releases
the leases. Independent editors still require explicit quiescence.
`reverse::capture_recovery_on` parses the guarded private corpus, captures database
usage and rechecks both inventories before returning the candidate. Hold the
selection lease before capture and retain the backup guard through apply/commit.
Shared cache roots are rejected by the private helper. Shared rollback uses
`SharedGit::recovery_snapshot` and `reverse::capture_shared_recovery_on`: refresh
before capturing the transport/policy backup, then retain its leases while reading
the saved confirmed Git objects. Recovery checks the root identity, requires no
pending draft, rejects blocked/superseded local state and verifies the exact branch
tip with a live read-only remote request. It never fetches into the saved object
store or falls back during remote failure. Parsing uses the ordinary shared-read
inventory over the exact commit blobs; disposable views are not recovery evidence.
The shared candidate includes its commit ID. Recheck `verify_recovery` before
committing SQL and keep all remote writers quiesced: a local lease and repeated
remote checks cannot prevent another host from publishing after the check.
Neither helper publishes configuration or handles an uncertain database commit;
those remain orchestrator responsibilities.

`reverse::apply_on` and the owner-only `ygg_knowledge_reverse_import` function
provide the SQL apply layer. The function takes an exclusive generation lease,
requires the exact database/corpus/fenced generation and supported protocol,
validates complete row schemas and types, deletes absent UUIDs, upserts current
rows and compares the complete resulting tables with normalized typed input.
JSONB `null` tags remain JSONB values rather than becoming SQL NULL. Unknown
columns/fields, duplicate UUIDs, constraint violations and timestamp precision
beyond PostgreSQL's microseconds fail atomically. The strict candidate adapters
also reject excess timestamp precision before SQL apply. This is a representability
check; it does not silently round existing document timestamps. New CLI note/rule
creation and approval timestamps use microsecond precision so ordinary new records
remain representable during rollback.

The bypass is transaction-local and requires an authenticated migration-owner
session; setting its flag as a runtime user cannot open the fence. Function EXECUTE
is revoked from PUBLIC and an explicit owner check still rejects runtime users if
EXECUTE is accidentally granted. Catalog references are qualified and function
search paths put temporary objects last; a runtime-owned temporary `pg_class` cannot
forge ownership. Successful calls restore the prior flag and leave
the storage marker fenced. Caller rollback undoes all row changes. This layer does
not capture source evidence or activate SQL, and must be enclosed by the complete
quiesced rollback workflow. Tests use disposable databases and distinct runtime
credentials to verify privilege boundaries, exact row results and failure rollback.

`reverse::apply_once_on` adds an operation receipt to that same SQL transaction.
The migration workflow must durably save the operation UUID, candidate and
`RecoveryEvidence` before sending it. Evidence binds the candidate digest, exact
corpus/policy backup revisions and optional shared commit. The owner-only function
holds the exclusive migration lease, requires READ COMMITTED and the exact fenced
generation, and hashes the complete request and restored rows. A retry waits for
any earlier transaction to finish; an identical committed operation returns
`PreviouslyApplied` after verifying row hashes, without repeating the import.
Conflicting operation IDs or changed restored rows fail. Rolling back removes
both row changes and receipt. Receipts cannot be updated, deleted or truncated;
runtime credentials cannot insert them even with an accidental table grant.
The returned outcome remains provisional until commit. Receipts prove the SQL
apply, not filesystem quiescence, schema parity or configuration activation; the
complete resume/activation workflow remains required.

`rollback::Journal::prepare` durably saves the operation UUID, complete candidate,
original export, fenced generation, recovery revisions and exact source directory
identities before SQL application. Identical preparation reuses the UUID; a
different request cannot overwrite it. Journal and archives must remain separate
from both source directories. Preparation refuses a busy journal without waiting
while holding source leases. Resume holds the journal lease, reacquires local
selection and source leases, verifies retained archives and current source bytes,
recaptures authoritative usage and shared Git evidence, and applies the saved
operation. A missing local apply record after SQL commit is safe to retry: the
database receipt verifies the existing rows without importing them again.

`ygg knowledge rollback-status JOURNAL --json` inspects this saved intent offline.
Its `local_apply_recorded` field is only a local hint; resume must verify the
database receipt and current rows. Changed sources, archives, directory identities
or journal bytes fail without replacing recovery evidence. Quiesce external
editors and remote hosts before preparation/resume. This component requires the
exact fenced SQL generation, preserves local selection, and does not activate a
backend or certify fleet compatibility. The private rollback command below adds
source backup/schema checks, final SQL activation and local deselection. Shared
and fleet orchestration remain pending.

## Corpus backup component

`KnowledgeStore::backup` creates an immutable snapshot directory containing
`corpus/` and `knowledge-backup.json`. It preserves exact bytes, unknown files and
empty directories. Identity/policy lives in a separate `IdentityRegistry` directory:
a deployment backup must snapshot that directory as well as its document bundle,
and record both returned revisions alongside its consistent PostgreSQL dump.
This library component alone is not a complete Yggdrasil deployment backup.
`db backup` pairs bundle and policy snapshots with a database dump. `db restore`
restores exact corpus/policy bytes into a new destination and validates the database;
explicit `db switch` selects a validated restored deployment. Binary upgrades remain pending.

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

`remember` now has an offline OKF adapter using the same note JSON contract.
Knowledge paths resolve independently of database validity, with the same profile,
user configuration and environment precedence as deployment commands. Existing
installations without a policy `runtime.json` continue using guarded SQL.

A selected context requires a versioned OKF binding with a positive storage
generation, supported minimum client protocol, canonical bundle path, corpus
UUID, explicit source user/repository mappings and optional agent-name mappings.
Repository bindings must agree with the identity registry. Each command holds a
shared `.selection.lock` lease in the policy directory. Cutover/rollback
publication takes its exclusive lease while changing selection.
Malformed, fenced, unsupported or mismatched bindings fail without SQL fallback.
The selection lease is disposable and excluded from corpus/policy backups.

`ygg knowledge fence-local --expected-generation GENERATION [--json]` drains
compatible commands using this policy directory and durably fences their selected
OKF generation without a database connection. It saves the exact original and
fenced bindings, operation UUID and source directory identities in
`local-fence-GENERATION.json` before publishing the fence. A retry resumes the same
operation only when the selection is exactly the saved original or fenced bytes;
conflicting edits and replaced directories fail without overwriting them. This
leaves the generation unchanged: it is the last selected OKF generation, not proof
of a PostgreSQL transition. Take policy recovery backups after local fencing;
rollback preparation rejects a live or mismatched local selection. The command
does not stop external editors or other hosts, and does not provide an unfence
shortcut. Activation still requires the complete validated migration workflow.

This checkpoint does **not** expose a command for minting a selection or enable
OKF by default. The validated migration/cutover publisher remains required;
hand-writing `runtime.json` is not a supported migration. Tests construct bindings
only in disposable fixtures. The binding is local authority during database
outages, not a fleet-wide revocation mechanism. Fleet quiescence and generation
publication remain migration requirements, and deployment moves must explicitly
rebind its canonical bundle path before enabling the target.

Offline note creation preserves UUID/provenance fields and uses explicit agent
bindings without querying PostgreSQL; unknown agents retain null provenance.
Repo-scoped commands require a mapped Git identity, including shared worktree
identity. An unmapped/non-Git cwd is never silently promoted to global scope;
use `--global` deliberately. New repo notes require an unambiguous legacy output
mapping before writing. Listing retains the `count`/`results` JSON envelope,
reports malformed-document diagnostics separately, and performs no telemetry or
database access. `prime`, hooks and UI dispatch still require integration.


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
confirmed bytes for offline browsing. Deployment moves still need the planned
validated rebinding of the runtime's canonical bundle path. Automatic Git/cache
retention cleanup and a supported cutover/configuration publisher remain unfinished.


## Private single-host migration command

`ygg knowledge migrate --plan /absolute/plan.json --journal /absolute/operation`
executes or resumes a private migration. External PostgreSQL also requires
`--pg-bin /absolute/compatible-postgres/bin` when creating the initial backup.
`--json` prints the resulting operation ID, source/corpus IDs, generation, state
and retained journal path. Missing mode arguments remain an error.

The version-1 plan contains `transport: "private"`, `source_generation`, the
`mappings` object shown above, explicit `identities` (the identity.json format),
`agents` (agent names to UUIDs), `execution_host`,
`all_participating_hosts_listed: true`, and one `hosts` entry. That entry contains
`name`, `protocol: 1`, and explicit true declarations for
`knowledge_writers_stopped`, `external_editors_stopped`, `schema_changes_stopped`,
and `session_preserving_endpoint`. These are operator assertions about the whole
deployment, including offline clients; live session inspection cannot prove them.
The command rejects shared transports and multiple hosts. It does not discover
participants, stop writers, configure admission rules, or infer identity/trust.

Before fencing, the command checks ownership, the compatible SQL generation,
installed legacy write/marker guards, live registered clients and complete row
conversion. It captures a consistent PostgreSQL dump plus existing configured
knowledge/policy files, seals the backup manifest digest, and compares recorded
schema, legacy knowledge rows and migration checksums at cutover. The schema
comparison uses backup catalog evidence; it does not certify arbitrary external
DDL or replace the schema-quiescence declaration. An immutable SQL event records
which operation owns the fence. The command then exports/publishes the frozen
rows, installs the exact explicit policy and fenced binding, retains a paired
recovery archive, and completes the private activation journal. Existing target
files must match the export exactly; independently edited files are never replaced.

Retry with the same plan, paths and journal. Completed SQL activation uses its
receipt and frozen source evidence without restoring old OKF documents, preserving
acknowledged post-cutover edits. Interrupted paired captures verify every retained
archive and create only missing archives. The native regression test kills the
actual CLI while blocked at local selection after fencing/publication, then
resumes it using the original dump. This is one tested interruption boundary,
not exhaustive power-loss or disk-full qualification.

`--abort` supports only this operation's original pre-activation fence. It checks
retained source evidence, records the next SQL generation, and clears only its
exact prepared fenced binding while holding local selection and database leases.
Retry after an acknowledged abort preserves later SQL writes. All backup, stage,
unselected export and policy evidence remains for inspection. Once OKF is active,
abort is refused: rollback must import the current bundle, including later edits
and deletions. The private rollback command below completes reverse import and
local activation. Shared/fleet execution, rollout and release gates remain unfinished.

## Private current-bundle rollback command

`ygg knowledge rollback --plan /absolute/rollback.json --journal /absolute/recovery`
restores the current private OKF corpus to SQL. Use `--pg-bin` for an external
source backup and `--json` for the operation report. The journal must be separate
from the corpus, policy, and original export. The version-1 plan contains
`transport: "private"`, `source_generation` (the currently selected OKF generation),
`original_export` (the original retained staging export, for example the forward
journal's `stage` directory), `execution_host`,
`all_participating_hosts_listed: true`, and the same single-host maintenance
assertions used by the forward plan. It uses the saved original mappings and
requires the current local binding to match them. It does not infer identities,
stop hosts/editors, or certify fleet admission.

The operation saves its request before changes and captures a consistent database
plus current corpus/policy backup. Before permanently fencing either store, it
holds the source leases and validates current document conversion, committed usage,
SQL field types and constraints inside a transaction that is rolled back. Unknown
metadata, unsupported SQL columns and unrepresentable values leave OKF selected.
The retained backup is not overwritten; after correcting source files, prepare a
fresh journal to capture the corrected state.

After successful preflight, the command drains/fences local OKF operations,
records ownership of the next SQL fence, retains a fresh paired recovery archive,
and saves the complete reverse candidate. The current documents determine edits,
deletions, new rows, approvals and pending state; the old frozen SQL rows never
serve as the rollback payload. Reverse import, its immutable receipt, final SQL
generation and activation event commit together. Local deselection then occurs
under the local selection lease and a renewed SQL generation lease. Until that
unlink, the local CLI remains fenced.

Retry with the same plan and journal. A committed activation event proves which
operation selected SQL; retry finishes deselection without replaying the saved
candidate over later SQL writes. A later generation or changed selection refuses
recovery. All source dumps, original exports, current corpus/policy archives and
journals remain retained. The opt-in native fixture kills the actual CLI after
SQL activation commits while its renewal lease is blocked, then verifies that
resumption preserves a SQL edit accepted in the gap and creates no second import
receipt. Round-trip fixtures also cover current note edits/deletions, approval
changes, pending rules and imported usage totals. These private single-host
checks do not replace shared/fleet recovery, representative scope-result parity,
full deployment-move rehearsal, disk-full qualification or the 14-day dogfood gate.
