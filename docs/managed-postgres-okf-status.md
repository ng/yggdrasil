# Managed PostgreSQL and OKF implementation status

This is the release-gate ledger for [PR #122](https://github.com/ng/yggdrasil/pull/122),
task `yggdrasil-5`, and [ADR 0019](adr/0019-managed-postgres-and-okf-knowledge.md).
The full implementation remains in progress. Existing installations keep their
configured database and SQL knowledge until an explicit validated cutover.
Candidate bundles and passing fixtures do not authorize default OKF rollout or
legacy-table removal.

Audit basis: the 2026-10-08 managed PostgreSQL/OKF implementation plan, tracked by
`yggdrasil-3`, SHA-256
`ba46868ba5c93c620a2f29bcbd156c83da63b8a6df15454d3cb3aaede9a62dca`.
The plan is maintained outside the repository. This ledger maps its requirements
to repository evidence and explicitly identifies missing implementation or proof.

## Milestones

| Milestone | Implemented evidence | Remaining acceptance |
| --- | --- | --- |
| M0: contracts | ADR 0019; `tests/database_config.rs`, `knowledge_contracts.rs`, `okf_documents.rs`, and `fixtures/knowledge/`; pinned OKF specification and deterministic profile/digest fixtures. | Keep contracts and operator examples aligned with subsequent changes. |
| M1: managed runtime | `src/db/runtime.rs`, `supervisor.rs`, `package.rs`; native installer/lifecycle fixtures, 20-process startup race, surviving-server adoption, killed bootstrap recovery and committed-row preservation. Native candidate bundles pass on three platforms; clean Ubuntu 24.04 runtime qualification is recorded below. | Public released-artifact qualification and macOS quarantine/signing path. |
| M2: integration | `src/config/database.rs`, `src/db.rs`, init and `db` commands; common connection resolver, existing URL preservation, no fallback on external failure, no hook download/init/upgrade. | Clean-machine operator qualification using the final released artifacts. Default knowledge rollout remains M7. |
| M3: hosted/lifecycle | PG16/18 CI; runtime/owner role separation; wrong-CA/hostname rejection; pooling/singleton-loss diagnostics; combined backup/restore/switch preserving database/knowledge IDs and task claims. Patch 16.14→16.15 and major 16→18 native recovery fixtures pass on all three platforms at `9ad3fd7`. Both external↔managed move directions pass locally at `6d49d51`, with Linux and Apple Silicon native CI passing. | Intel qualification of the new cross-mode fixtures remains pending. Published-artifact and actual deployment/credential recovery evidence remain distinct from local fixture results. |
| M4: OKF engine | Parser, identities, approval, matching, conditional store, browsing and disposable indexes; unknown metadata, null/legacy identity preservation, stale-revision conflicts, live eligibility revalidation, offline fixtures and real-disk-full tests on all three native platforms. | Two uninstrumented local repeats at `ba7aff6` pass the SQL-relative latency threshold (−61.51 ms and 4.86 ms added p95); the stricter SQL→OKF→SQL fixture passes twice at 17.61 ms and 16.01 ms worst added p95, meeting the local measured target. Historical variance, including the prior 108.57 ms failure, remains documented; these runs do not establish a universal latency bound. See `docs/performance/okf-latency-investigation.md`. Broader filesystem fault qualification remains open. |
| M5: integration/shared transport | Offline `remember`/`learn`, independent prime/hook knowledge, task-claim injection, linked-worktree scope, SQL/OKF ordered JSON parity; real bare-Git conflict, reachability, freshness/revocation, outage and draft-recovery fixtures; bounded complete-snapshot publication with exact remote-base checks. | Shared remote credential/provider and resource/latency qualification; deployed shared cutover and rollback rehearsal. Component fixtures do not prove a deployed fleet. |
| M6: cutover/dogfood | Database generation/write guards, client registration and lossless export; private and public fleet forward/abort/current-state rollback commands, immutable journals, backup evidence, apply-once receipts, reverse cancellation and descendant reconciliation. All 23 native migration tests pass, including three real two-host CLI flows. | Establish compatibility and writer quiescence for the actual deployment, perform its inventory and rollback rehearsal, then record the required dogfood window. Fixture declarations alone do not demonstrate a deployed fleet. |
| M7: removal/default rollout | Deliberately deferred; legacy tables/repositories remain available for the compatibility/recovery window. | **14 days of dogfooding plus successful rollback rehearsal**, preceding milestone gates, then a new forward removal migration, repository removal, installer/default changes and final restore/coordination qualification. |

## Requirements and evidence boundaries

### Database selection and ownership

Configuration has a single resolver: explicit mode wins, an existing URL selects
external mode, and no mode/URL selects managed initialization. A conflicting
managed mode and URL fails. User `.env` compatibility remains; repository `.env`
files are not read. Profiles share state across worktrees and use independent
data, binary, runtime and knowledge paths outside repositories.

The pinned native distribution is PostgreSQL 16.15 with archive hashes, provenance,
license notices and `uuid-ossp`. Staged exclusive publication precedes use. One
supervisor owns the private Unix-socket cluster; ordinary CLI exit does not stop
it. Adoption verifies live process/database identity. Runtime roles do not own
schema or migration markers. Status remains observational, and external stop is
unsupported. See [operator instructions](../src/db/README.md).

PostgreSQL remains the coordination engine. JSONB/arrays/enums, transactional
claims, resource leases and dedicated singleton sessions retain their existing
models. `tests/singleton_authority.rs` checks authority loss and reacquisition;
TLS and PgBouncer fixtures check supported connection behavior. CI validates
PostgreSQL 16 and 18, not every hosted provider. Database hosting does not provide
multi-host tmux/worktree execution.

### Knowledge authority and recovery

The [knowledge guide](../src/knowledge/README.md) describes the pinned OKF v0.2
profile, portable IDs, explicit legacy mappings, approval digests, private/shared
trust, matching semantics and expected-revision writes. Generic valid documents
remain browseable without becoming executable rules. Backups preserve unknown
content; SQL rollback refuses content it cannot represent losslessly.

SQL/OKF parity fixtures compare ordered legacy JSON and original identifiers,
including UTF-8, nullable filters, wildcard escaping, scope and prime's five-note
limit. Migration requires UTF8 server semantics; restore verifies recorded target
encoding/locale compatibility before import. These are admission checks, not an
encoding conversion facility.

A disposable 16.15→18.3 restore rehearsal initially failed final catalog validation:
all table counts and row hashes match, but PostgreSQL 18 adds 204 table `NOT NULL`
constraint entries. No configuration switch occurs. The
[raw comparison](validation/pg16-to-pg18-2026-10-09.json) records this failed case;
its application tables were empty, so it is not a complete data-move rehearsal.
[PostgreSQL 18](https://www.postgresql.org/docs/18/catalog-pg-constraint.html)
records these table constraints in `pg_constraint`, while
[earlier versions](https://www.postgresql.org/docs/17/catalog-pg-constraint.html)
represent table nullability through `pg_attribute`. Major-upgrade support needs
version-aware semantic verification without dropping unrelated constraint checks.

Restore now handles this specific PG16/17→18 catalog transition: it compares
ordinary validated, enforced, local table `NOT NULL` entries against the source
column's recorded nullability. Same-major evidence stays exact; domain constraints,
inherited/partitioned constraints and newer constraint semantics are not omitted.
The populated native `tests/restore_major.rs` fixture passed locally on 16.15→18.3,
preserving task/run claims, IDs and row hashes, and rejecting changed rows,
nullability, checks and domain checks. The original failed report remains historical
evidence. This restore evidence does not implement major upgrade orchestration or qualify all
cross-major catalog differences. PG17 has not had a native restore rehearsal.

Combined backups bind a consistent dump to corpus/policy revisions and exact user
configuration evidence. Restore preserves database IDs and document UUIDs. A
selected restored corpus changes only its local runtime bundle path, with
independently derived version 2 receipt evidence; fenced authority stays fenced.
Source data and archives remain unchanged. The native fixture reads the same note
through actual external and managed restore/switch commands and rejects altered
target policy. Scope aliases on another host and shared/fleet moves need broader
qualification. External certificates and provider credentials may need separate
operator provisioning; a configuration snapshot is not a credential service.

Private cutover and reverse import preserve current edits, deletions, activation
and usage baselines; retries do not replay committed mutations or overwrite later
writes. Shared transport uses confirmed Git snapshots, conditional non-force
pushes and a 60-second automatic-injection freshness limit. Retained pending
commits support explicit inspection/recovery. The public `knowledge fleet` command
orchestrates forward cutover and current-state SQL return. The fleet journal
now captures current shared documents and SQL usage only after the authenticated
fleet fence commits. It retains immutable intent, paired coordinator cache/policy
archives, and a lossless reverse candidate; retries require identical evidence.
Capture leaves SQL fenced and does not apply the candidate or return hosts to SQL.
The two-host fixture covers post-cutover writes, exact retry, lost completion
record recovery, and refusal of independent policy or remote changes.
The journal can subsequently apply that candidate, its reverse-import receipt, and
SQL generation activation in one transaction. The SQL recovery event binds the
exact rollback request, capture, and source backup. Committed retries verify the
SQL generation and schema without replaying old rows or requiring the old Git tip.
The separate authenticated deselection step removes only each host's exact committed
local fence after checking the SQL-return receipt under a shared generation lease.
It retains local intent before removal and per-host coordinator evidence afterward.
Retries preserve independently changed selections and refuse missing evidence.
The public CLI also exposes forward cancellation/abort, reverse cancellation and
explicit descendant reconciliation; deployment qualification remains open.
Reverse cancellation now has an immutable SQL barrier that drains admitted host
operations and rejects delayed fences. It only starts at the original OKF
generation. Authenticated host restoration now preserves original fence evidence,
restores only the exact original selection, and records a local complete-host census.
An immutable SQL completion seal now validates the complete restored census before
releasing admission for a fresh operation. Competing requests serialize; terminal
retries only inspect historical evidence and cannot unfence a newer operation.
Explicit reconciliation after the global reverse fence now retains a chain of
fresh quiescence requests and descendant Git snapshots in separate caches. Capture,
SQL return and host deselection resolve the selected request; superseded imports
and Git rewinds are refused. Earlier captures and archives remain unchanged.
The public CLI workflow is documented in [shared cutover](shared-knowledge-cutover.md#operator-command-workflow). Broader recovery qualification remains open.

### Faults, performance and packaging

| Gate | Evidence and current limit |
| --- | --- |
| Interrupted bootstrap/cutover/config publication | Native subprocess kills and resume fixtures cover durable boundaries, orphan initializer isolation, source preservation and later-write preservation. |
| Lost singleton session | Tests terminate the lock backend and require dispatch/reaping authority to stop until a new lock is acquired. |
| Ambiguous shared push, stale activation, unmapped scope, old SQL writer | Shared/store/migration fixtures cover conditional conflicts, reachability recovery, fresh approval checks, scope refusal and database write fences. Full deployed-client admission remains an operator/release requirement. |
| Full filesystem | `tests/okf_disk_full.rs` passed locally and in release-mode native CI on all three platforms at `a97f8fa`: update/create failures preserved acknowledged bytes after real `ENOSPC`, then retry/reopen passed after freeing space. The 128 MiB APFS images filled after 129,368,064 bytes; Linux tmpfs filled after 134,152,192 bytes. Tmpfs tests allocation failure rather than disk durability. Earlier `RLIMIT_FSIZE` coverage remains a distinct partial-write test. |
| Warm hook latency | Actual release CLI, 10,000 documents, 20 clients and 100 samples per SQL→OKF→SQL phase: two uninstrumented bracketed runs pass with worst added p95 **17.61 ms** and **16.01 ms**, below the **50 ms** target. Ordered output and all 2,400 OKF observations pass. Raw reports: `docs/performance/okf-hooks-2026-10-10-bracketed*.json`. The earlier 108.57 ms failure remains historical variance evidence; these local measurements do not establish a universal bound. |
| Separate latency cases | Shared-network, database cold-start, broader scope distributions and cross-platform qualification remain. Warm local measurements cannot stand in for these cases. |
| Native bundles | Online/offline assembler, manifests, license retention, deterministic checksums, bounded extraction and extracted-binary lifecycle/backup smoke passed on all three target platforms at `b3153a7` in [run 37991943961](https://github.com/ng/yggdrasil/actions/runs/37991943961). CI candidates are not public release artifacts. |
| Final distribution | Release publication and macOS quarantine/Developer ID/notarization workflow remain; clean Ubuntu runtime candidate qualification is recorded below. |

A separate local debug-build initialization, running alongside the full suite,
hit the 30-second `postgres --version` bootstrap deadline for newly extracted
binaries. No server, data directory or bootstrap intent was created, and process
inspection found no surviving child. A later standalone version probe completed
in 17 ms. This remains cold-start evidence to investigate; the warm retry does not
qualify a clean-machine first initialization.

Local full-suite validation at `f62d529`: **585 passed, 22 opt-in ignored**, plus
all-target, formatting and diff checks. The following `a97f8fa` change adds the
disk-full test/workflow/documentation without changing production Rust; its local
native test, all-target check, formatting, diff and workflow lint passed.
[Standard CI 37995932270](https://github.com/ng/yggdrasil/actions/runs/37995932270)
passed on `a97f8fa`, including PostgreSQL 16 and 18.
[Native run 37995932262](https://github.com/ng/yggdrasil/actions/runs/37995932262)
passed lifecycle/restore, real disk-full and both bundle-flavor smoke tests on all
three platforms. Every assembly recorded a clean source checkout and uploaded its
candidate artifact. The following documentation-only `f820a9d` also passed
[standard CI 37996388468](https://github.com/ng/yggdrasil/actions/runs/37996388468)
and [native CI 37996388458](https://github.com/ng/yggdrasil/actions/runs/37996388458).
Ownership-guard head `df918f7` passed
[standard CI 37999389831](https://github.com/ng/yggdrasil/actions/runs/37999389831)
and [native CI 37999389807](https://github.com/ng/yggdrasil/actions/runs/37999389807)
on all three platforms. The subsequent restore-compatibility change passed 588
local tests (24 opt-in ignored, 90 result groups), plus the separately invoked
populated PG16→18 native restore fixture.
Restore head `054aaba` passed standard PG16/18 CI and native Linux/Apple Silicon
jobs, but its Intel native job failed the disk-full fixture: a document update
succeeded after the filler had reported `ENOSPC`. The fixture now refills between
bounded attempts and retains every successful publication as acknowledged state.
It still requires actual failed update and create operations with `ENOSPC`, checks
all acknowledged documents and staging cleanup, then verifies retry after freeing
space. New Intel qualification is required; the failed run is
[38002643029](https://github.com/ng/yggdrasil/actions/runs/38002643029).
New-head CI results must be recorded when terminal; observation
timeouts are not test failures or reasons to restart a running job.

## Next release work

1. Finish Intel CI qualification of the newly added public fleet CLI and
   external↔managed move tests; preceding patch/major upgrade fixtures already
   passed all three native platforms at `9ad3fd7`.
2. Produce and smoke-test the final released artifacts on every advertised target,
   including positive macOS signing/quarantine evidence. Candidate CI bundles do
   not satisfy published-artifact qualification.
3. Keep database cold-start and shared-remote latency measurements separate from
   the passing local warm-hook benchmark; retain the documented variance limits.
4. Qualify the actual deployment's client compatibility, writer quiescence,
   inventory and identity mappings, then rehearse cutover and current-data rollback
   with recoverable backups and credentials.
5. Record **14 days of dogfooding and a successful rollback rehearsal**. Only then
   implement M7's forward removal migration, remove the SQL knowledge repositories,
   change installer/default behavior, and verify final restore and coordination.

Operational follow-ups already noted in the component guides include cache/session
and archived-draft retention, Git-history resource bounds and broader refresh
policy. They remain visible here; they do not replace the original milestone
acceptance criteria or justify claiming completion early.

### Clean Linux bundle qualification

The native workflow now also runs assembled online/offline Linux bundles in a
minimal Ubuntu 24.04 runtime container (`scripts/clean-linux-bundle.Dockerfile`).
It installs explicit distribution runtime libraries and Python/Git/curl for the
smoke harness, but no system PostgreSQL, Rust, or Docker tools. A non-root user
runs with read-only source/artifact mounts and a writable temporary filesystem;
the offline test has networking disabled. Both modes exercise initialization,
persistent identity, archive-free reuse, backup verification and owned shutdown.
This qualifies candidate bundles on that distribution only. At `fc281cf`, the complete native Linux job passed in [run 38017240665](https://github.com/ng/yggdrasil/actions/runs/38017240665/job/114110131143), including both container modes. Standard PG16/18 CI also passed in [run 38017240616](https://github.com/ng/yggdrasil/actions/runs/38017240616). The local Docker daemon was unavailable; this is CI execution evidence. Published artifacts, macOS signing and broader distribution qualification remain open.

### Uploaded macOS candidate signing audit

The arm64 CLI downloaded from native run `38013540700` matches its accompanying
`SHA256SUMS`. On macOS, `codesign --verify --strict` succeeds, but signature
display reports `adhoc,linker-signed`, no Team ID, and `spctl --assess --type
execute` rejects it (exit 3). [Raw evidence](validation/macos-candidate-signing-2026-10-10.json)
records the candidate digest and assessment. No signatures, quarantine attributes
or system policy were changed. This is CLI assessment only, not a quarantine
first-launch rehearsal or qualification of nested PostgreSQL binaries.

Apple's [notarization guidance](https://developer.apple.com/documentation/security/notarizing-macos-software-before-distribution)
requires Developer ID signing and hardened runtime for command-line targets.
Signature integrity alone does not meet that distribution requirement. The pinned
PostgreSQL archive is currently retained byte-for-byte; signing its executables
would change archive digests and therefore needs explicit release provenance and
catalog/packaging support, not an unrecorded post-verification mutation. The
release workflow currently has no signing/notarization stage. This gate remains
open; do not clear quarantine or weaken Gatekeeper to claim a pass.

A read-only local signing-identity inventory on 2026-10-10 found **zero Developer
ID Application identities**; the one available identity was Apple Development.
No private key was used and no identity was changed. The positive Developer ID
audit cannot be completed using this local identity inventory. Certificate names
and fingerprints are intentionally omitted from this public implementation ledger.

### Coordinator preparation progress

At `ea5abe4`, `fleet::Registration` binds an immutable operation/plan digest/corpus/
participant set to one SQL source generation and rejects competing coordinators.
Registration and cancellation require the migration owner. Connected SQL-host
preparation requires current SQL authority and matching registered membership;
local cancellation verifies the database receipt and retains a durable cancellation
record before restoring original selection absence. These are library primitives,
not an operator-ready fleet workflow. The private-only execution rejection remains.
See [the shared cutover protocol](shared-knowledge-cutover.md) for remaining
authentication, backup/evidence, publication, readiness, activation and rollback work.

Validation: ten native migration tests and eight OKF command tests passed locally,
including independent-connection coordinator contention, role denial, changed
registration refusal, unnotified-host preparation rejection, cancellation recovery
and preservation of later SQL writes. [Standard PostgreSQL 16/18 CI](https://github.com/ng/yggdrasil/actions/runs/38027051846)
passed for `ea5abe4`; its native qualification is still pending. The preceding
`93bc8ed` passed [standard CI](https://github.com/ng/yggdrasil/actions/runs/38025750973)
and [native qualification on all three platforms](https://github.com/ng/yggdrasil/actions/runs/38025750969).
CI's strict clippy step uses `continue-on-error: true` while existing warnings are
addressed; a green job does not establish a warning-free build. Local non-strict
clippy, all-target checking and formatting passed.

## Cross-mode deployment move qualification

`tests/managed_deployment_move.rs` exercises both external→managed and
managed→external PostgreSQL 16 moves through the public backup, restore and switch
commands. Each disposable fixture retains the source database and corpus, checks
database/corpus identity and exact document revisions, preserves task assignee,
current attempt, run state and idempotency key, rejects restore over an existing
target, and verifies that switch retry preserves a later target write without
changing the source. Native CI runs both directions on all three supported targets.

These fixtures use local Unix-socket servers for the external endpoints; the
external CLI receives only a connection URL. They do not qualify a hosted provider,
remote TLS/credential rotation, deployment-wide writer quiescence or a real operator
move. Both directions passed locally against pinned PostgreSQL 16.15 in 104.79
seconds; the new target compile, formatting and whitespace checks passed. The
three-platform CI result remains pending.

### Native reconciliation and disk-full checkpoint

At `9ad3fd7`, all three native jobs passed in
[run 38052446733](https://github.com/ng/yggdrasil/actions/runs/38052446733),
including the real filesystem-full recovery step on Intel macOS, Apple Silicon
macOS and Linux. Retaining acknowledged revision file handles prevents replaced
blocks from being recycled indefinitely after filler allocation reaches ENOSPC;
the fixture still requires failed writes to preserve acknowledged data and a
successful retry after space is freed. This resolves the earlier Intel fixture
failure. Later public fleet CLI and cross-mode move additions require their own
native results; this checkpoint does not qualify those additions or a release.

### Public fleet CLI verification

The complete repository suite for `80b453f` passed against a disposable UTF-8
PostgreSQL 16 cluster: 623 passed, 47 ignored across 94 result groups; teardown
completed cleanly. All 23 native migration tests passed separately, as did the
all-target check, formatting and whitespace checks. The later `6d49d51` changes
add only cross-mode tests, CI and documentation; both new native tests and their
compile check passed separately. Standard PostgreSQL 16/18 CI passed at `6d49d51`
in [run 38053885195](https://github.com/ng/yggdrasil/actions/runs/38053885195).
Linux and Apple Silicon native jobs passed in
[run 38053885160](https://github.com/ng/yggdrasil/actions/runs/38053885160);
Intel remains in progress. Release publication, deployment rehearsal and dogfood
gates remain open.

### Separate database lifecycle measurements

The verified Apple Silicon candidate bundles from native run `38053885160`
(PR head `6d49d51`, manifest build source `f79947f28f70a5baeb44b9032a48cd5da685de8b`)
passed the updated smoke test with fresh disposable profiles, preserved cluster
identity across restart, verified backups and clean shutdown. The manifest source
is the CI pull-request merge checkout. Raw measurements, bundle hashes and scope
limits are retained in [the lifecycle report](performance/managed-lifecycle-2026-10-10.json).

| Bundle | Fresh profile init | Running-cluster init reuse | Stopped-cluster start |
| --- | ---: | ---: | ---: |
| Offline | 9,518.93 ms | 25.77 ms | 93.20 ms |
| Online (including download) | 9,910.55 ms | 28.23 ms | 96.00 ms |

Each value is one wall-clock observation including CLI overhead. The CLI version
check precedes initialization; OS caches were not evicted. These are database
lifecycle measurements, separate from the warm-hook benchmark, not cold-machine,
p95, universal latency or published-artifact qualification. The smoke now emits
success only after teardown succeeds; all nine bundle regression tests pass,
including refusal to report success when final shutdown fails. Shared-remote
latency still requires its separate measurement.
