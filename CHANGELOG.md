# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.1](https://github.com/ng/yggdrasil/compare/v0.1.0...v0.1.1) - 2026-10-13

### Added

- publish fleet exports with retained exact-commit recovery
- resume the exact retained shared snapshot commit
- bind fleet export staging to the owned SQL fence
- coordinate owned SQL fleet fencing and resumable abort
- coordinate fleet preparation and resumable pre-fence cancellation
- expose bounded fleet participant protocol command
- handle backed fleet preparation and cancellation requests
- verify participant backups across SQL fence preparation
- bind authenticated fleet responses to individual requests
- add pinned SSH transport for fleet participants
- retain fleet coordinator requests in resumable journals
- validate and bind complete fleet migration requests
- *(knowledge)* publish complete shared snapshots conditionally
- *(db)* add explicit PostgreSQL 18 restore workflow
- *(db)* add resumable managed PostgreSQL patch upgrades
- install checksum-pinned native Postgres distributions
- supervise managed Postgres through db lifecycle commands
- prototype persistent managed Postgres ownership
- stage resumable fenced knowledge exports
- inventory legacy knowledge with migration dry-run command
- guard legacy knowledge operations and fence SQL writes
- store retry-safe knowledge usage telemetry
- preserve legacy knowledge models through OKF adapters

### Fixed

- reconcile reachable fleet hosts after cancellation failures
- validate PostgreSQL 18 table nullability during restore
- revalidate managed metadata under the ownership lease
- rebase selected knowledge paths during deployment restore

### Other

- Address adversarial review findings on managed PostgreSQL and OKF
- Address CodeRabbit review on managed PostgreSQL and OKF knowledge
- Let restore accept an explicit collation change for deployment moves
- Record green CI for the M7 head
- Expect the OKF corpus that ygg init now bootstraps in native package tests
- Make OKF the only knowledge backend and remove legacy SQL knowledge (M7)
- Record operator cutover, rollback rehearsal and revised release gates
- Carry the watcher_heartbeat migration so dogfood databases migrate cleanly
- Record ca5038f CI qualification and resumed handoff status
- Save paused PR 122 implementation handoff
- Measure shared publication and fetch over authenticated SSH
- Measure packaged database lifecycle separately from hook latency
- Reconcile release ledger with verified migration and latency gates
- Record full fleet CLI verification and cross-mode CI results
- Record passing native disk-full requalification
- Qualify database moves between external and managed modes
- Run native qualification for fleet CLI module changes
- Expose resumable fleet cutover and recovery commands
- Keep replaced inodes allocated in disk-full qualification
- Reconcile fenced rollbacks through explicit descendant snapshots
- Seal rollback cancellation before admitting a fresh operation
- Restore cancelled rollback hosts through authenticated receipts
- Record rollback cancellation before restoring fleet hosts
- Deselect fleet hosts using the committed SQL return receipt
- Return fleet knowledge to SQL with an atomic recovery receipt
- Capture current shared rollback data under the fleet fence
- Commit fleet reverse fences and preserve evidence during recovery
- Authenticate registered rollback host fencing over fleet RPC
- Reserve fresh generation-bound fleet rollback requests
- Qualify two-host finalization recovery with intervening shared writes
- Complete authenticated fleet activation and resumable host finalization
- Retain immutable generation-bound fleet activation evidence
- Reject mounted corpus and candidate directories before fleet swap
- Bind retained directory swap evidence to authenticated host readiness
- Add recoverable no-replace corpus directory swap
- Qualify staged Git cache relocation and reject relative fleet remotes
- Stage and authenticate fleet host readiness before activation
- exhaust smaller allocations in disk-full fixture
- exercise fleet participant protocol through real SSH
- specify authenticated fleet participant transport
- resume fleet journal after coordinator process death
- verify source backup survives fleet registration
- Reconcile coordinator evidence and local signing prerequisite
- Reserve fleet migration sources with immutable participant registration
- Verify SQL source before preparing migration participants
- Verify coordinator cancellation before restoring SQL host selection
- Prepare durable local fences for unselected SQL hosts
- Cancel unfenced knowledge migrations with a durable SQL generation
- Specify SQL-source host preparation and cancellation ordering
- Bind local OKF fences to migration participants
- define shared cutover preparation and recovery protocol
- add explicit macOS bundle distribution audit
- record uploaded macOS candidate signing gap
- include tested Ubuntu bundle prerequisites
- record clean Ubuntu bundle qualification
- qualify Linux bundles in a minimal runtime container
- *(okf)* bracket hook latency with SQL baselines
- retain two uninstrumented hook latency qualification runs
- *(okf)* profile repository identity discovery phases
- record current qualification and latency bottleneck evidence
- handle successful writes after filler ENOSPC
- align setup and milestone status with managed PostgreSQL
- qualify knowledge writes on full filesystems
- pull official PostgreSQL images from ECR Public
- assemble and smoke-test native release bundles
- narrow knowledge inventories to selected UUIDs
- Browse generic OKF documents and reject lossy SQL rollback
- Select a distinct catalog locale for native restore tests
- Reject incompatible restore database properties before import
- Require UTF8 server semantics for knowledge migration
- Index note and rule metadata separately
- Profile hook phases and avoid syncing disposable caches
- Batch usage receipts with ordered row locking
- Reuse query pattern results and move warm index rows
- Measure SQL-relative hooks and isolate session receipt leases
- Refresh local usage counters from guarded database telemetry
- Compare SQL and OKF retrieval across scope fixtures
- Preserve private deployment configuration in recovery backups
- Restore current private knowledge through resumable SQL rollback
- Execute and resume private knowledge migration with source backup evidence
- Resume private cutover through SQL and local publication
- Activate frozen knowledge generations with atomic usage receipts
- Audit live client compatibility without blocking coordination
- Fence local OKF selections durably before rollback backups
- Persist rollback intents and resume through verified SQL receipts
- Record atomic rollback receipts and verify uncertain retries
- Capture shared rollback from verified remote commits and retained backups
- Retain corpus and policy backup leases through rollback capture
- Tolerate closed peer during long-path fixture connection cleanup
- Capture rollback usage under the fenced migration transaction
- Apply current knowledge rows through a fenced owner-only transaction
- Validate reverse-import candidates against the current corpus
- Handle lingering connections when cleaning up claim test databases
- Publish verified export bundles under the source generation lease
- Publish shared Git caches from isolated initialization stages
- Add explicit recovery for uncertain shared knowledge writes
- Route shared knowledge through conditional Git snapshots
- Record optional OKF usage after rule emission
- Route task-claim rules through the selected OKF generation
- Inject local edit rules with private session deduplication
- Preserve local prime notes when coordination is unavailable
- Integrate offline OKF learning commands and approval policy
- Route selected OKF notes through offline remember commands
- Exercise recovery with a surviving native initdb child
- Support private managed endpoints for long data paths
- Select validated deployments with recoverable configuration publication
- Restore combined backups into validated empty deployments
- Publish combined database and knowledge backups
- Export consistent PostgreSQL snapshots for deployment backups
- Snapshot and verify durable knowledge backup components
- Enable prepared statement tracking in PgBouncer fixture
- Diagnose runtime privileges and session pooling compatibility
- Separate external migration owner from runtime credentials
- Require verified TLS for remote PostgreSQL connections
- Stop scheduler and watcher work when singleton authority is lost
- Verify managed readiness without equating distinct startup timestamps
- Recover managed bootstrap through durable isolated attempts
- Initialize managed PostgreSQL from pinned native packages
- Route application connections through managed deployment resolution
- Provision separate managed database owner and runtime roles
- persist advisory knowledge metadata index
- batch knowledge revalidation and cache sort keys
- measure OKF reads with twenty independent clients
- recover scope moves from durable intents
- add offline note and learning lifecycle service
- persist portable corpus and repository identities
- persist conditional OKF writes and preserve SQL matching
- align compact-tab regression and run complete PostgreSQL matrix
- establish OKF document and deployment contracts
- keep inbox persistence checks isolated from worker spawning
- record storage architecture as ADR 0019
- plan managed Postgres and OKF knowledge storage
- Stop the dashboard-spawned watcher from corrupting the TUI ([#120](https://github.com/ng/yggdrasil/pull/120))
- Track worktree teardown separately from window reaping
- Drain cleaned workers from list_cleanable with a one-shot reaped flag
- Make worker reaping self-healing and safe to run anywhere
- Keep the TUI refresh cascade off the input thread
- Default lock acquire/release agent to YGG_AGENT_NAME
- Anchor repo identity to origin URL, not local directory name ([#117](https://github.com/ng/yggdrasil/pull/117))
- Fallback when release bot credentials are unset
- Use release bot token for release-plz
- Cache tmux availability probe
- Address maintenance cleanup review feedback
- Harden maintenance cleanup tmux evidence
# Changelog

All notable changes to **Yggdrasil** will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

<!-- release-plz prepends new versions below this line. -->
