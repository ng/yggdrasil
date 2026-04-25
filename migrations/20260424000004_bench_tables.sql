-- ADR 0016 + docs/eval-benchmarks.md: tables for `ygg bench`. Separate from
-- ygg eval (which is a dashboard over retrieval-pipeline events on the
-- ADR 0015 deprecation track).

CREATE TABLE bench_runs (
    run_id        UUID PRIMARY KEY DEFAULT uuid_generate_v4(),
    scenario      TEXT NOT NULL,
    baseline      TEXT NOT NULL,           -- 'vanilla-single' | 'vanilla-tmux' | 'ygg-N'
    parallelism   INT  NOT NULL,
    model         TEXT NOT NULL,
    harness_sha   TEXT NOT NULL,
    seed          BIGINT,
    started_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    ended_at      TIMESTAMPTZ,
    passed        BOOLEAN,
    notes         TEXT,
    CHECK (parallelism >= 1)
);

CREATE INDEX idx_bench_runs_scenario ON bench_runs (scenario, baseline, started_at DESC);
CREATE INDEX idx_bench_runs_started  ON bench_runs (started_at DESC);

CREATE TABLE bench_task_results (
    run_id        UUID NOT NULL REFERENCES bench_runs(run_id) ON DELETE CASCADE,
    task_idx      INT  NOT NULL,
    passed        BOOLEAN NOT NULL,
    wall_clock_s  INT NOT NULL,
    tokens_in     BIGINT,
    tokens_out    BIGINT,
    tokens_cache  BIGINT,
    usd           NUMERIC,
    reopened      BOOLEAN NOT NULL DEFAULT FALSE,
    PRIMARY KEY (run_id, task_idx)
);

CREATE TABLE bench_metrics (
    run_id     UUID NOT NULL REFERENCES bench_runs(run_id) ON DELETE CASCADE,
    metric     TEXT NOT NULL,
    value      DOUBLE PRECISION NOT NULL,
    PRIMARY KEY (run_id, metric)
);
