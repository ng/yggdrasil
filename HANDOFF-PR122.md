# Yggdrasil PR #122 handoff

## Goal status

**Resumed 2026-10-11** via `/goal complete HANDOFF-PR122.md`. Operator decision
the same day: **drop the macOS signing/notarization gate and the 14-day dogfood
window** (single-operator deployment); cut over this machine now; then implement
M7 immediately. All items below are done as of 2026-10-12; CI is green at `8d70c0b` (standard 38213926103, native 38213926106). PR #122 is ready to merge.

## Workspace

- Primary repository: `/Users/ng/Documents/GitHub/yggdrasil`
- Worktree: `/Users/ng/Documents/GitHub/yggdrasil/.codex/worktrees/embedded-pg-okf-plan`
- Branch: `docs/embedded-pg-okf-plan`
- Last implementation HEAD: `ca5038f`, pushed. Later commits are docs/ledger only.
- Worktree was clean and up to date before creating this handoff.
- Agent: `YGG_AGENT_NAME=embedded-pg-okf-plan`
- Detailed local handoff: `/tmp/ygg-pr122-current-handoff.txt`
- Release ledger: `docs/managed-postgres-okf-status.md`
- Tracking task: `yggdrasil-5` (closed with M7); follow-up `yggdrasil-7` largely moot after machinery removal.

Read `CLAUDE.md` and `AGENTS.md`; use Yggdrasil coordination and locks. Preserve
unrelated changes and worktrees. Revalidate current local and remote state before
acting after an explicit resume.

## Implemented

- Managed PostgreSQL packaging, lifecycle, ownership, recovery, configuration,
  external connections, backups/restores, patch upgrades and major-version moves.
- Authoritative OKF storage, identities, approvals, offline knowledge commands,
  retrieval, indexing, durable writes and shared Git transport.
- Private and shared-fleet migration, cancellation, abort, current-data SQL
  rollback, reverse cancellation and explicit descendant reconciliation.
- Public `ygg knowledge fleet` CLI with exact request SHA pins and resumable journals.
- External↔managed deployment-move tests preserving identities, knowledge,
  task claims, later writes and original sources.
- Native CI coverage and corrected real disk-full recovery fixture.
- Separate database lifecycle and authenticated loopback-SSH latency measurements.
- Documentation and raw qualification evidence committed to PR #122.

## Verified evidence

- Full repository suite: **623 passed, zero failed, 47 ignored across 94 groups**;
  disposable UTF-8 PostgreSQL cleaned up successfully. This suite covered
  `80b453f`; later additions were tests, scripts, CI and documentation, with
  relevant checks run separately.
- All **23 native migration tests** passed.
- Both external↔managed move tests passed.
- All-target, formatting and whitespace checks passed for the application changes;
  subsequent new test targets were checked separately.
- Standard and all three native platform jobs passed at `6d49d51`:
  standard run `38053885195`; native run `38053885160`.
- Nine bundle regression tests and offline/online candidate smokes passed.
- Authenticated SSH benchmark: all 22 writer/reader pairs passed.
- Local warm-hook benchmark: 17.61/16.01 ms worst added p95, below 50 ms.
  Historical variance and measurement limitations remain documented.

Useful local evidence:

- `/tmp/ygg-fleet-cli-full.log`
- `/tmp/ygg-fleet-cli-native-full.log`
- `/tmp/ygg-deployment-move2.log`
- `/tmp/ygg-fleet-cli-check-final.log`
- `/tmp/ygg-lifecycle-bundle-tests-final.log`
- `/tmp/ygg-shared-ssh-perf.log`

Raw committed measurements:

- `docs/performance/managed-lifecycle-2026-10-10.json`
- `docs/performance/shared-ssh-latency-2026-10-10.json`
- `docs/performance/okf-latency-investigation.md`

## Recent commits

- `80b453f`: public fleet CLI and recovery tests/docs.
- `17ad26b`: native CI trigger for nested CLI modules.
- `6d49d51`: external↔managed deployment qualification.
- `83b6955`, `0c4fe45`, `964c188`: verification and release-ledger updates.
- `a593cde`: separate packaged database lifecycle timing.
- `ca5038f`: authenticated shared-SSH benchmark and raw results.

## Remaining work after explicit resume

1. **Done 2026-10-11.** Final CI results for `ca5038f`:
   - Standard `38055774671`: success (PG16/PG18 tests, check, clippy, rustfmt).
   - Native `38055774673`: success on Linux, Apple Silicon and Intel macOS.
   Recorded in `docs/managed-postgres-okf-status.md` ("Latest head
   qualification"); "Next release work" item 1 is marked done.
2. **Done.** No failures to address. No tests were rerun.
3. **Dropped** (operator decision 2026-10-11): no Developer ID signing or
   notarization. Published-artifact smoke on Linux remains a release item.
4. **Done.** Deployment target is this machine's external Homebrew PostgreSQL 18
   (`~/.config/ygg/.env`), 28 repos, owner `ng`. Client audit clean. Plan and
   mapping in `~/.local/share/ygg-okf/`.
5. **Done.** Cutover at generation 19 (401 OKF documents) after a successful
   rollback rehearsal (generation 5). Details in the ledger's "Operator cutover".
   Backups: `~/.local/share/ygg-backups/2026-10-11-{pre,post}-okf`.
6. **Dropped** (operator decision): no 14-day window.
7. **Done 2026-10-12.** M7: removal migration, SQL repository and cutover
   machinery removal, `ygg init`/`ygg knowledge init` bootstrap, auto-binding,
   docs; full suite 571 passed / 0 failed; applied to this machine; backup
   restore rehearsal passed. See ledger "M7 verification".

## Missing inputs

None. Both prior questions were answered on 2026-10-11: signing is not needed,
and the dogfood target is this machine.

## Operational boundaries

- No operator data was migrated and no release was published.
- Preserve the existing Homebrew PostgreSQL installation, especially
  `/opt/homebrew/var/postgresql@18`, and unrelated worktrees.
- Use disposable databases for tests. Never restart a live test or CI job merely
  because observation timed out; inspect its authoritative state first.
- Do not claim fixture results prove WAN/provider behavior, real deployment,
  signing qualification or elapsed dogfood time.
- Four superseded CI runs were deliberately cancelled: `38055499019`,
  `38055313404`, `38055194504`, `38054227159`. Preserve the latest runs.
- The first October 11 resume check was interrupted when the user paused the
  goal. The second (this one) completed read-only CI inspection; see item 1.
- The docs-only handoff commit `aea8575` also triggered CI; its standard run
  `38173420624` passed and native run `38173420682` was in progress at the time
  of writing. It changes no source and needs no action unless it fails.
