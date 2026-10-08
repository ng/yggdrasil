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

## Remaining engine work

Generic OKF bundle browsing, command integration of the legacy adapters and the
SQL-relative hook latency gate remain unfinished. Explicit browsing snapshots scan the Yggdrasil
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

Rule matching and prime-note selection now use a disposable `.lookup.json` in the
private bundle root. Its versioned header pins the parser version and contains a
SHA-256 corpus revision over the sorted path, file-fingerprint, document-digest
and matching-metadata rows. The file is bounded to 64 MiB and replaced atomically.
It contains no Markdown bodies, activation evidence or unknown YAML metadata.
Notes/proposals requested for explicit browsing still use authoritative snapshots.

Each lookup inventories all live UUIDs to exclude ambiguous copies, then checks
file metadata relative to held directory descriptors without following symlinks.
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

This is a compatibility foundation, not a ready-to-run cutover. Production commands
still select SQL and will report a guard error if an administrator switches the
marker to OKF. The guarded repository calls discover the current generation;
`guard::legacy_transaction` also accepts an expected generation for future adapters
that hold a configuration snapshot. OKF dispatch/local generation publication,
manifest validation, minimum-client inventory, migration-only reverse-import bypass
and restricted runtime/migration roles remain required before exposing cutover.
Never manually switch the marker as a substitute for those steps.

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
nonzero for unresolved reports. `--dry-run` is required because publication is
not implemented yet. `rows_verified: true` only proves this snapshot's rows passed
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

The exporter does not publish, seed telemetry, change storage mode or configure
trust. It is a library operation pending the validated cutover command; the public
migration CLI still requires `--dry-run`. A future publisher must recheck the live
source generation and copy only manifest-listed documents and required controls
into its new authoritative destination; it must not blindly publish arbitrary
unlisted files from a staging directory. Fleet upgrade, backups, scope-fixture
parity, local generation/configuration publication and reverse import remain
required. The tests exercise durable partial states and conflicts; they are not
a substitute for the full killed-process migration and rollback release gates.
