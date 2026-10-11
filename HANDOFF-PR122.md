# Yggdrasil PR #122 handoff

## Goal status

**Resumed 2026-10-11** via `/goal complete HANDOFF-PR122.md`. The full goal is
**still not complete**: implement
`/Users/ng/.codex/plans/yggdrasil/embedded-postgres-okf.md` using existing
[PR #122](https://github.com/ng/yggdrasil/pull/122). Remaining items 1 and 2
below are done; items 3 through 7 are **blocked on user inputs and elapsed
time** (see "Missing inputs").

## Workspace

- Primary repository: `/Users/ng/Documents/GitHub/yggdrasil`
- Worktree: `/Users/ng/Documents/GitHub/yggdrasil/.codex/worktrees/embedded-pg-okf-plan`
- Branch: `docs/embedded-pg-okf-plan`
- Last implementation HEAD: `ca5038f`, pushed. Later commits are docs/ledger only.
- Worktree was clean and up to date before creating this handoff.
- Agent: `YGG_AGENT_NAME=embedded-pg-okf-plan`
- Detailed local handoff: `/tmp/ygg-pr122-current-handoff.txt`
- Release ledger: `docs/managed-postgres-okf-status.md`
- Tracking task: `yggdrasil-5`, not closed.

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
3. Complete published-artifact and positive macOS signing/notarization/quarantine
   qualification. Candidate bundles do not satisfy released-artifact gates.
4. Establish the actual deployment target, compatible-client inventory,
   writer quiescence, identity mappings and recoverable backups.
5. Perform actual deployment and current-data rollback rehearsal.
6. Record **14 days of real dogfooding**.
7. Only after those prerequisites, implement M7: forward legacy-table removal,
   SQL knowledge repository removal, default OKF/installer changes, and final
   restore/coordination verification.

## Missing inputs

- Which Developer ID signing/notarization setup to use.
- Which installation/profile should receive the compatibility release and dogfood.

These were asked asynchronously but no answer was received. Re-checked
2026-10-11 on resume: still only an Apple Development identity locally, zero
Developer ID Application identities, and `gh secret list` / `gh variable list`
return nothing. Do not treat that identity as Developer ID distribution signing.
Request setup names or locations, not secret values.

Items 3–5 cannot start without these answers; item 6 additionally needs 14
elapsed days after deployment; item 7 is gated on all of them. No fixture or
local evidence substitutes for any of these gates.

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
