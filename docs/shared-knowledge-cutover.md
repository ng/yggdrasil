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

`fence::cancel_sql` now supplies the matching local cancellation primitive. It
requires the exact coordinator request digest, operation/database/source identities,
immutable database cancellation receipt and current SQL return generation. While
holding local selection and shared database leases it retains a local cancellation
tombstone before removing only the exact fenced selection (or accepting original
absence). Interrupted removal resumes from that evidence. `prepare_sql` refuses a
tombstoned request, so delayed preparation cannot re-fence a cancelled host. Changed
selections, receipts, policy or database generations refuse without overwriting them.
These primitives still require coordinator authentication, census and workflow;
they do not expose an operator CLI or enable shared/fleet migration execution.

`fence::prepare_sql_at_source` supplies the connected preparation path: acquire the
local selection lease first, then hold the database shared generation lease while
verifying the expected compatible SQL source and absence of a coordinator
cancellation receipt through local fence publication. A host that never received
local cancellation cannot prepare an old request after database cancellation, even
if its request generation is edited to match the new SQL generation. Failure does
not publish a new fence intent or runtime selection. The offline `prepare_sql` is
a lower-level primitive and cannot provide this database check; coordinator work
must use the connected path. Request authentication, plan validation, complete
census and activation remain the coordinator's responsibility.

## Coordinator registration

`fleet::Registration` now reserves one source database generation for a coordinator
operation under the migration advisory lock. The immutable database row binds the
complete plan digest, corpus and canonical participant set (1–1024 unique non-nil
IDs). Matching retries succeed; different plans, corpora, participants or competing
operations refuse. Only the migration owner can register or cancel. Registration
leaves SQL authority and generation unchanged; it does not prove authentication,
quiescence, host availability or readiness.

Pre-fence cancellation advances SQL generation and records the existing immutable
cancellation receipt atomically. It preserves later SQL writes on retry and leaves
participant restoration explicit. A new operation may reserve the returned SQL
generation. Connected participant preparation now additionally requires membership
in this registered plan with the exact request digest/database/corpus/generation;
this checks the declared identity, while authenticating the actual host remains a
separate transport requirement. Registration has no activation/fencing method or
CLI until the full backup, host verification, publication and recovery workflow is
implemented. Private migrations still use their existing journal; a competing
private transition invalidates the fleet source generation rather than being
adopted as a fleet-owned fence.

## Validated fleet request

`fleet::plan::ValidatedPlan` accepts a bounded versioned request and retains its
exact JSON bytes. Its registration hashes those bytes, binding source identity,
generation, explicit user/repository/agent mappings, remote branch/base commit,
source-backup digest, participant set and each participant's policy and backup
reference. Reformatting the request changes its digest; recovery must use the
retained original bytes. Host-local paths are validated lexically, never resolved
on the coordinator. Each host must still verify its own canonical paths, directory
identities, backup contents and exact configuration under its local lease.

Validation rejects incomplete declarations, duplicate participants/names, missing
repository mappings, mismatched corpus identity, incompatible protocols and
invalid backup hashes. It does not authenticate hosts, inspect backups, establish
quiescence or prove that every source row has a mapping. Authenticated evidence exchange and full execution remain unfinished;
this parser does not enable shared migration. A native PostgreSQL regression
confirms that source backup verification accepts subsequent fleet registration
metadata but rejects subsequent legacy knowledge edits.

## Durable coordinator request journal

`fleet::journal::Journal` retains the validated request before registration. Its
private directory holds an atomically published intent with the exact request
bytes, SHA-256, canonical journal path and filesystem directory identity. An
exclusive process-owned lease rejects competing coordinators; process exit
releases the lease without deleting the durable intent. Matching preparation is
idempotent. Resume requires the independently retained registration digest and
rejects copied/moved journals, changed request bytes and replaced directories.
Every plan access rechecks the live directory and exact intent bytes. Existing
unrelated files are preserved and block initialization.

This is request retention, not the full migration state machine. The execution
layer must enforce separation from local deployment/corpus/policy/backup paths,
authenticate participants, retain transition evidence and implement forward,
abort and current-snapshot rollback before enabling shared migration.

## Participant transport implementation contract

Use OpenSSH for the initial authenticated participant channel. Bind each
participant's endpoint (host, port, account) and explicit server public key into
the immutable plan; changing enrollment requires a new plan. Use a dedicated
known-hosts file and participant-specific `HostKeyAlias`, strict host-key checking,
noninteractive public-key authentication, and no fallback to global known-hosts
files or DNS trust. Do not accept a key learned by an unauthenticated scan as
operator enrollment. Host-key authentication establishes the enrolled server;
the SSH account's authorization to execute Yggdrasil remains an operator concern.

Invoke a fixed participant subcommand. Send a bounded request on stdin rather
than interpolating JSON, paths or user content into a remote shell command. Bind
request and response to protocol version, operation, exact plan digest,
participant, action and a fresh request nonce. Accept a response only from the
successful authenticated subprocess for that request, never from an arbitrary
JSON file. The returned record must additionally match retained host-local
preparation/configuration/backup evidence. Successful SSH authentication does not
prove editor quiescence or a complete host census.

Disable connection multiplexing, forwarding and user configuration overrides
that could substitute a different authentication policy. Bound stdin/stdout/stderr
and total execution time. A timeout, disconnect or nonzero exit after dispatch
has an uncertain mutation outcome: retain the journal and reconcile the same
idempotent operation on the participant. Never interpret that failure as absence
of a prepared fence or permission to activate other hosts.

A disposable, unprivileged loopback OpenSSH probe on macOS (2026-10-10) accepted
the matching temporary host/client keys and rejected both a wrong pinned host key
and a wrong client key (exit 255, no remote-command output). No system SSH
configuration or operator keys were changed; the test daemon was stopped. This
establishes local test feasibility only. Two-host migration integration remains to be implemented. OpenSSH's [configuration manual](https://man.openbsd.org/ssh_config.5)
defines the host-key alias and strict-checking behavior used by this design.


`fleet::transport::exchange` now implements the bounded SSH byte exchange. Plans
require each participant's endpoint/account/public host key. A fresh private
known-hosts file isolates the pinned key for each invocation; the fixed remote
command is `ygg knowledge fleet-participant`. Requests travel on stdin (8 MiB
maximum), with stdout limited to 1 MiB, stderr to 64 KiB and an overall 60-second
deadline. Errors retain uncertain-outcome semantics; there is no automatic retry.

A real disposable loopback `sshd` test covers correct authentication, wrong host
and client keys, excess response bytes and timeout. Run it explicitly with
`cargo test --lib knowledge::fleet::transport::tests::authenticated_exchange_rejects_wrong_keys_and_bounds_output -- --ignored --exact`
on a machine with OpenSSH client/server tools. The fixture uses temporary keys and
a forced echo command; it verifies transport only, not participant behavior. Callers must still validate
operation/plan/participant/action/nonce and retained host evidence before using any
response. Shared migration remains unavailable.


`fleet::protocol` now binds preparation/cancellation requests and responses to
version, operation, participant, exact plan digest, action and a fresh nonce.
`protocol::call` dispatches over the pinned SSH transport and rechecks the journal
before returning a matching `AuthenticatedPreparation`. That type has no public
JSON/file constructor. Receipt validation additionally checks source database,
corpus, generation, host policy path and evidence hash formats. A response from
an earlier invocation cannot satisfy a new nonce, even for the same idempotent
operation. Participant handlers must reconcile prior local results and return
those results in the current request envelope.

These checks do not compare hashes against live host files or prove that backups
remain valid. The participant command must perform those checks under the host
lease; complete-set revalidation and database transitions remain outstanding.
Protocol tests reject replay, changed envelope fields, tampered plan bytes and
wrong host/source evidence. Shared migration execution remains disabled.


`fence::prepare_sql_backed` now verifies a participant's pinned combined backup
before and after local fence publication, while holding its exclusive local
selection lease and the database's shared generation lease. It verifies the
backup database/generation, captured deployment selection, corpus contents and
policy contents. Only exact newly created preparation-intent and fenced-runtime
bytes may differ from the original policy archive; archived entries cannot be
exempted or replaced. This permits an interrupted preparation to retry against
its original backup while rejecting unrelated policy/corpus/configuration edits.
A verification failure never removes a published fence. Participant dispatch and
the complete fleet state machine remain outstanding.


`protocol::Request::execute` now provides a host-side library handler for backed
SQL preparation and cancellation. It compares canonical paths from the host's
actual deployment configuration with that participant's planned paths, derives
the source binding and explicit mappings, and serializes the checked result in
the current request envelope. It does not copy coordinator-local aliases or
configuration onto the host.

`fence::cancel_sql_backed` checks the same original backup while permitting only
the exact preparation controls and operation-bound cancellation tombstone. It
requires the immutable database cancellation event and current return generation
before restoring absent local selection. Repeated cancellation neither reapplies
SQL rows nor erases independent files. The complete coordinator workflow remains unavailable.


The hidden `ygg knowledge fleet-participant` command now exposes preparation and
cancellation through bounded JSON stdin/stdout. It reads at most 8 MiB with a
30-second input deadline, validates the request before loading deployment state,
and uses the maintenance connection without starting or initializing PostgreSQL.
Only a successful checked response goes to stdout; failures use stderr and a
nonzero exit status. The native migration fixture exercises retry and cancellation
through independent CLI processes, including rejection without a response after
cancellation. This component command does not export, publish, activate or roll
back a shared corpus. The operator-facing shared migration rejection remains.

The native `native_authenticated_ssh_participant_preparation_and_cancellation`
fixture now exercises `protocol::call` through a real pinned SSH connection, the
built participant CLI, and disposable PostgreSQL. It verifies authentication
failure before mutation, preparation retry, refusal to cancel without a database
receipt, successful cancellation afterward, and retention of later SQL writes
on retry. Temporary server/client keys and an isolated forced-command wrapper
supply only fixture configuration; system/operator SSH settings are untouched.
Strict ownership checks remain enabled, so authorized-key fixtures live in an
automatically removed private directory beneath the user-owned home rather than
world-writable `/tmp`. Native Linux CI installs OpenSSH server tools for this test.
This proves the one-participant preparation/cancellation path, not multi-host
cutover, shared publication, activation or current-state rollback.

`Journal::prepare_hosts` now verifies the coordinator's pinned source backup and
configuration, rejects journal overlap with local deployment/backup paths,
registers the plan, and contacts every participant. Each successful authenticated
receipt is retained separately. `fleet-prepared.json` is written only after the
complete declared set responds. A retry recontacts every host and compares exact
retained evidence; it never treats a cached receipt as current readiness.

`Journal::cancel_hosts` first records database cancellation, then reconciles every
participant and writes `fleet-cancelled.json` only after all respond. An unreachable
participant does not prevent cancellation of later reachable participants; the
coordinator retains successful receipts and reports every failed participant for
retry. Journal integrity or receipt-retention failures still stop immediately. A backed
host that never prepared may acknowledge cancellation only after verifying the
original absent selection, original backup, matching database cancellation event
and current SQL return generation. It records intent plus cancellation tombstone
without publishing a fence. A partial or uncertain attempt remains resumable;
there is no implicit success for an unavailable host. These coordinator methods
still stop before database fencing, export, publication or activation.
