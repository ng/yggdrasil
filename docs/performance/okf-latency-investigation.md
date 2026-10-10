# OKF hook latency investigation

The latest two bracketed runs meet the local latency target; historical failures remain below for comparison. The selected-UUID batching repeat
measured SQL p95 279.39 ms and OKF p95 387.96 ms: **108.57 ms added**, against
50 ms allowed. It uses 10,000 documents, 20 concurrent clients and 100 warm
samples per mode. See `okf-hooks-2026-10-09-selected-uuid-batch-repeat.json`.
Earlier passing samples do not supersede this repeat.

## Existing phase evidence

The separate `okf-hooks-2026-10-09-kind-index-phases.json` run measured 72.60 ms
added p95. Its per-phase samples yield the following milliseconds:

| Phase | Median | p95 |
| --- | ---: | ---: |
| edit_repository | 67.86 | 89.26 |
| edit_rules | 105.46 | 141.42 |
| index_inventory | 48.65 | 66.51 |
| index_read | 5.52 | 17.48 |
| index_validate | 11.10 | 33.30 |
| edit_revalidation | 22.89 | 36.38 |
| edit_receipt | 23.80 | 37.62 |
| hook_usage | 5.51 | 14.71 |

These are historical observations, not measurements of the current head.
Phases nest and percentile samples need not come from the same request; do not
sum these values or subtract them from end-to-end p95 to predict an improvement.

## Next measurement

`injection::for_edit` resolves the repository before selecting rules.
`Context::repo` calls `GitIdentity::discover`, which starts two sequential Git
processes: `rev-parse --path-format=absolute --git-common-dir` and
`config --get remote.origin.url`. The repository phase also includes path
canonicalization and registry resolution. Measure these separately in the real
release-CLI fixture before choosing a replacement or cache. The present profile
does not establish which part dominates or how much is avoidable relative to SQL.

Directory inventory remains another candidate, but previous kind-specific indexes
and selected-UUID batching have not reliably met the gate. Preserve fresh global
UUID ambiguity detection, linked-worktree identity, origin alias conflicts,
selected-document byte revalidation and approval invalidation. A timestamp-only
membership cache is not adequate evidence of authoritative membership.

Any optimization must rerun `sql_relative_edit_hook_p95` in release mode at the
original workload, retaining ordered output and usage-accounting checks. Keep
cold database startup and shared-remote latency separate from this local warm gate.

## Identity phase measurement (2026-10-10)

With only opt-in timing instrumentation added, the unchanged release-CLI workload
passed once: SQL p95 351.20 ms, OKF p95 362.45 ms, added p95 **11.26 ms**.
Raw samples: `okf-hooks-2026-10-10-identity-phases.json`. This diagnostic pass
is not evidence of a performance fix: SQL itself was slower than the previous
failed repeat, and no optimization was made. Repeat without diagnostics before
claiming qualification; retain the earlier failure as evidence of variance.

| Identity phase | Median ms | p95 ms |
| --- | ---: | ---: |
| Repository total | 114.54 | 166.02 |
| Git common directory | 61.83 | 96.41 |
| Git origin | 46.99 | 90.97 |
| Canonicalize directory | 0.04 | 0.17 |
| Parse origin | 0.00 | 0.00 |
| Registry resolution | 0.09 | 0.24 |

This attributes the repository phase primarily to the two Git subprocesses under
concurrency. It does not measure a safe alternative. Any attempt to avoid an
origin lookup must still detect origin/common-directory mapping conflicts and
must preserve missing-origin and linked-worktree behavior.

Validation of the instrumentation: 590 tests passed, 29 opt-in tests ignored
across 93 result groups, using a fresh disposable database and serial execution;
`cargo check --all-targets`, `cargo fmt --check` and `git diff --check` passed.
An initial parallel run failed four scheduler tests; a serial retry against that
reused database failed two approval tests. All 32 integration tests passed in the
fresh database. These earlier failures are fixture-contamination evidence and
must not be represented as successful runs.

## Uninstrumented repeats at `ba7aff6`

Two consecutive release-CLI runs with phase diagnostics disabled passed the
original 10,000-document, 20-client, 100-sample-per-mode workload:

| Run | SQL p95 ms | OKF p95 ms | Added p95 ms |
| --- | ---: | ---: | ---: |
| `okf-hooks-2026-10-10-identity-uninstrumented.json` | 346.66 | 285.15 | -61.51 |
| `okf-hooks-2026-10-10-identity-uninstrumented-repeat.json` | 283.11 | 287.97 | 4.86 |

Both preserve the expected ordered output and all 2,400 usage observations.
These are passing local qualification samples. They do not establish that the
historical 108.57 ms failure was fixed: only diagnostic instrumentation changed,
and the SQL p95 differs considerably even between these two runs. The harness
measures SQL before OKF, so time-varying machine load can affect the comparison.
A future stability investigation should counterbalance measurement order while
preserving the same workload and backend correctness checks, rather than accept
only favorable runs or weaken the 50 ms threshold.

## Bracketed baseline

The release-CLI fixture now measures SQL → OKF → SQL, each with 20 warm-up
requests and 100 measured requests. Each request uses a fresh session and checks
the same 20 ordered rules. The gate uses the **larger** added p95 from the two
SQL comparisons, preserving the 50 ms limit. SQL usage totals must reach 240
applications per matching rule across both phases; OKF must still record 2,400
observations. Transitions are fixture-only and do not qualify production rollback.

The first uninstrumented run passes: SQL-before p95 240.79 ms, OKF 253.11 ms,
SQL-after 235.50 ms, worst added p95 **17.61 ms**. Raw samples are in
`okf-hooks-2026-10-10-bracketed.json`. This guards against a slow initial baseline
masking a regression; it does not eliminate all machine-load variance or explain
the historical failure.

The idle-build repeat also passes: SQL-before p95 231.95 ms, OKF 247.97 ms,
SQL-after 239.97 ms, worst added p95 **16.01 ms**. See
`okf-hooks-2026-10-10-bracketed-repeat.json`. Both bracketed runs preserve output
and usage-accounting invariants. This meets the local measured target for these
two runs; it does not claim a production optimization or universal latency bound.

Validation after the fixture change: **590 passed, 29 opt-in ignored, 93 result
groups** against a fresh disposable database, plus `cargo check --all-targets`,
`cargo fmt --check` and `git diff --check`. Both bracketed measurements ran in
release mode with diagnostics disabled and no concurrent local Cargo build.
