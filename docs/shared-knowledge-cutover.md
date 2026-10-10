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
