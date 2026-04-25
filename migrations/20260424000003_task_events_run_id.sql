-- ADR 0016: attribute task_events to a specific run. Adds new event kinds
-- that the scheduler emits as it advances state.

ALTER TABLE task_events ADD COLUMN IF NOT EXISTS run_id UUID REFERENCES task_runs(run_id);
CREATE INDEX IF NOT EXISTS idx_task_events_run ON task_events (run_id) WHERE run_id IS NOT NULL;

-- New top-level event kinds emitted by the scheduler. ADD VALUE IF NOT EXISTS
-- is idempotent.
ALTER TYPE event_kind ADD VALUE IF NOT EXISTS 'run_scheduled';
ALTER TYPE event_kind ADD VALUE IF NOT EXISTS 'run_claimed';
ALTER TYPE event_kind ADD VALUE IF NOT EXISTS 'run_terminal';
ALTER TYPE event_kind ADD VALUE IF NOT EXISTS 'run_retry';
ALTER TYPE event_kind ADD VALUE IF NOT EXISTS 'scheduler_tick';
ALTER TYPE event_kind ADD VALUE IF NOT EXISTS 'scheduler_error';
