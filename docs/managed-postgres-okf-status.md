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
| M1: managed runtime | `src/db/runtime.rs`, `supervisor.rs`, `package.rs`; native installer/lifecycle fixtures, 20-process startup race, surviving-server adoption, killed bootstrap recovery and committed-row preservation. Native candidate bundles pass on three platforms. | Public released-artifact qualification, macOS quarantine/signing path and clean-machine dependency handling. |
| M2: integration | `src/config/database.rs`, `src/db.rs`, init and `db` commands; common connection resolver, existing URL preservation, no fallback on external failure, no hook download/init/upgrade. | Clean-machine operator qualification using the final released artifacts. Default knowledge rollout remains M7. |
| M3: hosted/lifecycle | PG16/18 CI; runtime/owner role separation; CA/hostname rejection; pooling/singleton-loss diagnostics; combined backup/restore/switch, current selection-path rebasing and preserved database/knowledge IDs. | Explicit patch upgrade now has a journal, backup validation, startup fence, resume and pre-switch abort; local native 16.14→16.15 happy-path and seven crash/abort cases pass. Patch-upgrade native CI passed on all three platforms at `25187e7`. An explicit managed 16→18 backup/restore/switch path now uses a separate pinned 18.6 catalog and new data directory; its populated native CLI flow passes locally. The major-flow baseline suite passed 590 tests; the later startup-deadline correction passed 11 native regressions. At `6e80c7e`, standard CI (run `38013540750`) passes PostgreSQL 16/18 tests, check, Clippy and formatting; native Linux, Intel macOS and Apple Silicon macOS all pass in run `38013540700`. This qualifies the candidate lifecycle fixtures, not published artifacts or clean-machine distribution. Broader deployment-move, credential/provider recovery and published-platform qualification remain. |
| M4: OKF engine | Parser, identities, approval, matching, conditional store, browsing and disposable indexes; unknown metadata, null/legacy identity preservation, stale-revision conflicts, live eligibility revalidation, offline fixtures and real-disk-full tests on all three native platforms. | Two uninstrumented local repeats at `ba7aff6` pass the SQL-relative latency threshold (−61.51 ms and 4.86 ms added p95); the stricter SQL→OKF→SQL fixture passes twice at 17.61 ms and 16.01 ms worst added p95, meeting the local measured target. Historical variance, including the prior 108.57 ms failure, remains documented; these runs do not establish a universal latency bound. See `docs/performance/okf-latency-investigation.md`. Broader filesystem fault qualification remains open. |
| M5: integration/shared transport | Offline `remember`/`learn`, independent prime/hook knowledge, task-claim injection, linked-worktree scope, SQL/OKF ordered JSON parity; real bare-Git conflict, reachability, freshness/revocation, outage and draft-recovery fixtures. | Shared remote credential/provider and resource/latency qualification; complete shared cutover and rollback orchestration. Component fixtures do not prove a deployed fleet. |
| M6: cutover/dogfood | Database generation/write guards and client registration; full row export/round-trip validation; private single-host forward/abort/current-state rollback coordinators with journals, backups, apply-once receipts and killed-process resumption. | Shared/multi-host execution is rejected. Deployment-wide client compatibility and writer quiescence must be established; local operator declarations alone do not demonstrate them. Full recovery/rollback rehearsal and recorded dogfooding remain. |
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
commits support explicit inspection/recovery. Shared reverse-capture primitives
exist, but there is no complete shared/fleet migration command.

### Faults, performance and packaging

| Gate | Evidence and current limit |
| --- | --- |
| Interrupted bootstrap/cutover/config publication | Native subprocess kills and resume fixtures cover durable boundaries, orphan initializer isolation, source preservation and later-write preservation. |
| Lost singleton session | Tests terminate the lock backend and require dispatch/reaping authority to stop until a new lock is acquired. |
| Ambiguous shared push, stale activation, unmapped scope, old SQL writer | Shared/store/migration fixtures cover conditional conflicts, reachability recovery, fresh approval checks, scope refusal and database write fences. Full deployed-client admission remains an operator/release requirement. |
| Full filesystem | `tests/okf_disk_full.rs` passed locally and in release-mode native CI on all three platforms at `a97f8fa`: update/create failures preserved acknowledged bytes after real `ENOSPC`, then retry/reopen passed after freeing space. The 128 MiB APFS images filled after 129,368,064 bytes; Linux tmpfs filled after 134,152,192 bytes. Tmpfs tests allocation failure rather than disk durability. Earlier `RLIMIT_FSIZE` coverage remains a distinct partial-write test. |
| Warm hook latency | Actual release CLI: 10,000 documents, 20 concurrent clients, 100 warm samples/mode; ordered output and all 2,400 observations pass. Latest unchanged batching repeat: SQL 279.39 ms / OKF 387.96 ms p95, **108.57 ms added**, exceeding the **50 ms** target. Earlier passing samples do not qualify the release. Reports: `docs/performance/okf-hooks-2026-10-09-selected-uuid*.json`. |
| Separate latency cases | Shared-network, database cold-start, broader scope distributions and cross-platform qualification remain. Warm local measurements cannot stand in for these cases. |
| Native bundles | Online/offline assembler, manifests, license retention, deterministic checksums, bounded extraction and extracted-binary lifecycle/backup smoke passed on all three target platforms at `b3153a7` in [run 37991943961](https://github.com/ng/yggdrasil/actions/runs/37991943961). CI candidates are not public release artifacts. |
| Final distribution | Release publication, macOS quarantine/Developer ID/notarization workflow and clean-machine Linux runtime dependency qualification remain. |

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

1. Qualify [explicit patch upgrades](managed-postgres-upgrades.md) across native
   platforms and qualify the explicit 16→18 new-directory restore/switch workflow; never turn ordinary startup into an upgrade.
2. Qualify final native artifacts and complete the remaining latency/fault cases.
3. Complete shared/fleet cutover and current-state rollback, with explicit evidence
   of participating-client compatibility, writer quiescence and recoverable identity
   mappings. Rehearse deployment moves and external credential recovery.
4. Record the required dogfood window and rollback evidence. Only then implement
   M7's forward removal and default rollout.

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
This qualifies candidate bundles on that distribution only. Execution is pending
CI; the local Docker daemon was unavailable. Published artifacts, macOS signing
and broader distribution qualification remain open.
