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

## Remaining engine work

Scope moves, disposable indexing, generic OKF bundle browsing, legacy JSON
adapters and latency measurements remain unfinished. Current snapshots
scan the Yggdrasil layout directly. Approval must still be revalidated immediately
before injection; never treat a previously read document as current authority.
Shared Git transport, legacy adapters, migration and CLI integration build on this
layer and remain separate release gates.
