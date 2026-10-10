# OKF hook latency investigation

The release gate remains **failed**. The latest selected-UUID batching repeat
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
