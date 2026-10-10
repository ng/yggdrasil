# Shared knowledge cutover protocol

Implementation design for the remaining M5/M6 work in PR #122. **Not implemented
or operator-ready.** `migration::Plan` must continue rejecting shared/multi-host
execution until the forward, abort and current-state rollback paths below work
end to end. This extends the existing plan; it does not replace its release gates.

## Existing boundaries

`migration.rs` binds one journal to one corpus/policy directory and validates one
execution host. `cutover.rs` publishes private files and rejects shared transport.
`clients.rs` audits registered live PostgreSQL backends, not offline hosts or
external editors. `runtime::Selection` and `Context` hold local selection leases;
`shared.rs` confirms remote publication and retains uncertain commits. Ordinary
shared changes are limited to 32 paths, so that mutation API cannot publish an
entire migration snapshot by splitting it into independently visible batches.

## Identities and evidence

Introduce a separate versioned shared migration plan instead of relaxing version
1 private-plan validation. Bind the operation UUID, source database identity and
generation, corpus UUID, explicit legacy mappings, expected remote branch commit,
export manifest digest, and complete participating host IDs. Each host gets its
own local configuration/path/identity-policy evidence; common-directory aliases
and local bundle paths must not be copied from the coordinator onto other hosts.

Use authenticated operator transport to collect and deliver host records. A JSON
file containing a host name or digest is not proof of that host's identity or
quiescence. Bind records to the operation and the registered participant identity;
reject duplicate, missing, unexpected, stale or cross-operation records. The
operator still owns the complete host census, external-editor quiescence and
remote write-access controls. Do not describe these declarations as automatic
remote attestation. Audit live database clients again under the migration lease.

Host preparation must acquire the exclusive local selection lease, drain existing
compatible readers/writers, verify the local configuration, and durably record a
fenced selection before returning a prepared record. Records bind exact previous
and fenced policy bytes and directory identity. A killed preparation process must
leave a persistent fence, not a lease that expires and silently permits writers.
A host that cannot prepare blocks activation. Previously unknown or offline hosts
must remain disabled until explicitly enrolled; no majority/quorum shortcut.

## SQL-source host preparation contract

The existing `fence::local_for_migration` requires an already selected OKF corpus.
It cannot prepare a legacy SQL host. Do not treat that receipt as forward-migration
readiness or reuse it by inventing an original OKF selection.

Implement SQL-source preparation with an explicit absent original selection and a
retained host-local intent before publishing a fenced `runtime.json`. The intent
must bind operation, participant, database/corpus IDs, source generation, canonical
local configuration paths, directory identities, and exact preexisting policy
bytes. Capture each host's own configuration/identity policy in its backup; the
coordinator's backup cannot stand in for remote host state. A retry must accept
only the saved absent selection or exact fenced bytes, and must reject a changed
request, replaced directory, or independently selected corpus. Preserve existing
identity and shared-transport configuration until validated conditional staging;
preparation must not erase it to make an empty-host fixture pass.

`runtime::Selection::load` already rejects a valid `Phase::Fenced` selection before
opening SQL or the corpus. Reuse that compatibility boundary instead of adding a
second fence format that existing compatible clients would ignore. A legacy
command may have observed an absent selection immediately before publication and
already entered its SQL path. Local preparation therefore stops subsequent entry
but does **not** drain legacy SQL transactions. The coordinator must acquire the
exclusive database advisory migration lock, audit clients, verify source backup
and guards, and commit the database fence before export. Existing legacy guard
transactions hold the shared advisory lock until their work finishes.

Preparation can fail before the SQL fence exists. Provide a distinct pre-fence
abort event under the database migration lock: verify the source SQL generation,
advance the SQL generation and record cancellation for this operation atomically.
A delayed coordinator must fail its saved source-generation check, including older
coordinators that do not understand cancellation receipts. Each host may remove only its exact prepared fence
after checking that event and current SQL generation under a shared database
lease. A timeout or absence of a database event does not authorize local unfencing.
After a committed SQL fence, use the recorded return generation and conditional
host restoration described below. Neither abort path may restore a stale entire
policy snapshot over later independent changes.

The private `migration::Journal::prepare_policy` provides related byte/identity
checks, but runs after database fencing and export. The private pre-fence cancellation implemented below does not restore prepared
fleet selections, so calling private preparation independently on each host leaves
the fleet recovery case unsolved. Extract common validated operations only while preserving private
journal compatibility; do not sequentially invoke private migrations for each host.

Required additional fixture: begin with two hosts having no `runtime.json`, hold a
legacy SQL write transaction across local preparation, and show that SQL fencing
waits for it. Fresh CLI commands on both prepared hosts must refuse SQL fallback.
Kill after intent and after selection publication, resume in new processes, abort
before the SQL fence, then race a delayed coordinator against that cancellation.
Check exact original absence, saved local identities, and untouched policy bytes.

## Forward state machine

1. Prepare all participating hosts and preserve their corpus/configuration/policy
   evidence. Require writer/editor/schema quiescence and validate the source
   backup before fencing the database. A crash here may leave some hosts locally
   fenced while SQL remains authoritative; resume or explicitly abort preparation.
2. Under the database migration lease, recheck live clients, source identity and
   generation, backup consistency and the complete prepared host set. Fence SQL
   writes and durably record the operation. Host configuration cannot independently
   activate shared writes at this stage.
3. Export the consistent source to a complete staging snapshot. Reuse field-level
   round-trip validation, scope/retrieval parity, approval handling and row-to-path
   digests. Recheck host bindings before publication. Unrelated remote content or
   documents must not be silently overwritten or omitted.
4. Publish one complete Git tree/commit conditionally against the planned remote
   branch state. Add a dedicated bounded bulk-publication path; do not raise the
   ordinary 32-change limit or publish a series of partial snapshots. Preserve
   pending commit evidence before push, never force-push, and confirm reachability
   after an uncertain result. A conflicting remote update blocks activation.
5. Every prepared host fetches and verifies the exact published commit and corpus
   identity, stages its own shared configuration and mappings, and returns a ready
   record while remaining fenced. The coordinator revalidates the host set and
   publication evidence, then atomically records database OKF generation plus an
   activation event bound to the operation/commit/manifest/host-set digest.
6. Hosts finalize from that verified activation event, using conditional local
   publication and the saved directory/configuration identity. Missing or changed
   host evidence fails closed. A crash after database activation leaves unfinished
   hosts fenced until resume; it cannot return them to SQL. A completed retry must
   preserve newer policy changes and knowledge writes rather than replay staging.

The database and Git cannot commit atomically. Durable intent and a persistent
fenced interval are required between them. Remote reachability alone authorizes
neither local activation nor database generation changes. Likewise, a local ready
record alone does not authorize activation. The source generation has one active
migration operation; competing coordinators must conflict under the database lease.

## Abort and rollback

Before database activation, abort verifies the unchanged operation and publication
state, records the SQL return generation, and restores each host conditionally.
Retain published Git commits and backup evidence; abort must not force-reset a
remote branch or delete independent remote edits. Conflicting host changes require
operator reconciliation rather than overwriting them. Interrupted abort resumes
from durable events and cannot be mistaken for an activation retry.

After activation, prepare **every** host again, durably fencing local offline
writers, and quiesce external remote writers. Refresh and pin the current remote
snapshot under those controls; never reverse-import the original export. Reuse
lossless reverse mapping and reject unrepresentable generic content, identities,
approvals or deletions before changing authority. Bind the reverse import and its
apply-once database event to the current commit and all host evidence. Restore SQL
selection on each host only after the SQL generation event is verified. Keep
unfinished hosts fenced. Later retries must not reapply old rows over later SQL
writes. Partial host availability blocks rollback rather than losing offline edits.

## Required verification before removing the rejection

- Two independently configured hosts sharing a real bare remote and isolated
  PostgreSQL preserve UUIDs, nullable metadata, approval and usage through forward
  activation and current-state rollback; host-local paths and aliases remain local.
- Missing/offline hosts, duplicate or forged participant records, old/unregistered
  clients, changed configuration and mismatched operation/generation block transition.
- Kill and resume each host/coordinator after every durable publication boundary;
  uncertain Git push and uncertain SQL commit do not duplicate or lose writes.
- Race two coordinators, an external branch update, a local policy edit and a writer
  holding its selection lease. Never permit simultaneous SQL and OKF writers.
- Complete-snapshot publication exceeds 32 paths without intermediate partial
  authority and retains existing resource bounds. Ordinary mutations remain bounded.
- Post-cutover edits, deletions and approvals survive rollback; unrepresentable
  content refuses before SQL activation; retries preserve subsequent SQL/OKF writes.
- Keep the existing private migration, shared outage/revocation and compatibility
  fixtures. Full tests/check/fmt and native qualification remain required.

These tests establish protocol behavior on controlled participants. Deployment
census, external access controls, final release artifacts and 14-day dogfooding
remain separate evidence requirements from the original plan.

## Implemented transport primitive

`SharedGit::replace_snapshot(expected_commit, desired)` now provides conditional
whole-tree publication for a future coordinator. It retains the 20,000-file and
64 MiB snapshot limits, rejects duplicate document UUIDs, and produces one commit.
It compares the entire expected remote commit, not just affected file digests. A
trusted operation-local pre-push hook also requires Git's advertised old object ID
to match; receive-pack then checks that advertised ID atomically. This prevents a
remote rewind between fetch and push from accepting a stale snapshot. The push
never uses force, and remote/corpus-provided hooks remain disabled.
Pending intent records this strict-base mode; inspection exposes `exact_base`,
confirmation recovers a remotely reachable commit, and retry refuses a changed
remote base. Explicit discard retains the local draft. Ordinary `change` retains
its 32-path limit and disjoint-edit retry behavior. Identical snapshots return the
confirmed base without an extra commit.

Real bare-Git tests cover 64-file atomic publication, stale-base rejection,
independent remote edits during push, uncertain-publication reopening and limits.
This primitive does not validate an export manifest or authorize migration. Coordinator-managed host
preparation, authenticated evidence exchange, database activation and coordinated
rollback above are still unimplemented; shared/fleet execution remains rejected.

Validation: the initial bulk implementation passed the full serial suite against a
fresh disposable PostgreSQL database (**594 passed, 29 opt-in ignored, 93 result
groups**). Review then added the advertised-OID rewind guard and its regression;
all **13 shared-transport tests** passed on that correction (one subprocess helper
ignored). Current-source `cargo check --all-targets`, `cargo clippy --all-targets`,
`cargo fmt --check` and `git diff --check` passed. The full-suite result predates
the rewind guard and is not represented as an exact-final-source full-suite run.

## Implemented local request binding

`knowledge fence-local --expected-generation N --migration-operation UUID
--participant UUID --json` binds the durable local fence to both coordinator
request IDs. Both IDs must be present and non-nil. Retries must supply the same
pair; an unbound request cannot adopt a bound fence. Version 1 unbound journals
remain supported; bound journals use version 2. Exact selection bytes and
directory identities retain the existing crash-replay checks.

This drains selected OKF operations only. Legacy SQL clients without a local OKF
selection do not hold this lease. The receipt does not prove SQL writer drainage,
authenticate a host, establish a complete census, or authorize activation. Shared
migration execution remains rejected pending the complete coordinator workflow.

Latest integrated validation at `53c52d2`: full serial suite passed with 596 tests,
29 opt-in cases ignored, and 93 result groups. This includes the advertised-OID
guard and coordinator-bound local fence regression; check, clippy and fmt passed.

## Private pre-fence cancellation

`knowledge migrate --abort` can now cancel a prepared private journal before a
source backup or SQL fence exists. Under the migration advisory lock it advances
the SQL generation once and records an immutable intent-bound cancellation in
`knowledge_migration_cancellations`. SQL remains authoritative, existing writes
are drained, and knowledge rows and local policy are preserved. Repeated aborts
verify the receipt and current return generation without replaying data. A delayed
execution of the old plan fails its generation check, including older coordinators.
The post-fence abort path retains its existing backup validation. This implements
the database cancellation boundary; fleet host preparation/restoration is still
unimplemented and shared/fleet execution remains rejected.

## SQL-host preparation primitive

`fence::prepare_sql` now retains an intent for an absent original selection before
publishing the compatible fenced runtime binding. It binds coordinator/participant,
source database/corpus/generation, canonical local paths and directory identities,
and exact identity/shared-policy bytes. Retrying a different request or changed
policy fails; resuming the original absence reuses the same intent. Independent
selections and replaced roots are preserved and refused.

This library primitive has no CLI entry point. Its caller must authenticate the
request, back up the host configuration, and supply existing separate roots; it
neither creates a corpus nor validates a complete migration plan. Existing SQL
transactions still require database fencing under the advisory migration lock.
Coordinator cancellation/activation checks, host evidence revalidation and fleet
restoration remain required before exposing host preparation to operators. A saved
receipt alone cannot authorize activation or local unfencing.
