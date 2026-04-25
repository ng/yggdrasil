-- ADR 0016: extend tasks with execution metadata. Defaults preserve current
-- behavior (runnable=FALSE, approval_level=auto, no deadline).
ALTER TABLE tasks ADD COLUMN IF NOT EXISTS runnable             BOOLEAN     NOT NULL DEFAULT FALSE;
ALTER TABLE tasks ADD COLUMN IF NOT EXISTS current_attempt_id   UUID REFERENCES task_runs(run_id);
ALTER TABLE tasks ADD COLUMN IF NOT EXISTS max_attempts         INT         NOT NULL DEFAULT 3;
ALTER TABLE tasks ADD COLUMN IF NOT EXISTS timeout_ms           BIGINT;
ALTER TABLE tasks ADD COLUMN IF NOT EXISTS deadline_at          TIMESTAMPTZ;
ALTER TABLE tasks ADD COLUMN IF NOT EXISTS approval_level       TEXT        NOT NULL DEFAULT 'auto';
   -- 'auto' | 'approve_plan' | 'approve_completion'
ALTER TABLE tasks ADD COLUMN IF NOT EXISTS approved_at          TIMESTAMPTZ;
ALTER TABLE tasks ADD COLUMN IF NOT EXISTS approved_by_agent_id UUID REFERENCES agents(agent_id);
ALTER TABLE tasks ADD COLUMN IF NOT EXISTS parent_task_id       UUID REFERENCES tasks(task_id);
ALTER TABLE tasks ADD COLUMN IF NOT EXISTS input_spec           JSONB       NOT NULL DEFAULT '{}';
ALTER TABLE tasks ADD COLUMN IF NOT EXISTS output_spec          JSONB       NOT NULL DEFAULT '{}';
ALTER TABLE tasks ADD COLUMN IF NOT EXISTS agent_role           TEXT;
ALTER TABLE tasks ADD COLUMN IF NOT EXISTS required_locks       TEXT[]      NOT NULL DEFAULT '{}';
ALTER TABLE tasks ADD COLUMN IF NOT EXISTS result_blob_ref      TEXT;
ALTER TABLE tasks ADD COLUMN IF NOT EXISTS plan_strategy        TEXT;

-- Validate enums via CHECK constraints (less rigid than a TYPE; lets us add
-- values without ALTER TYPE migrations).
ALTER TABLE tasks ADD CONSTRAINT tasks_approval_level_chk
    CHECK (approval_level IN ('auto', 'approve_plan', 'approve_completion'));
ALTER TABLE tasks ADD CONSTRAINT tasks_agent_role_chk
    CHECK (agent_role IS NULL OR agent_role IN ('planner', 'executor', 'critic'));
ALTER TABLE tasks ADD CONSTRAINT tasks_plan_strategy_chk
    CHECK (plan_strategy IS NULL OR plan_strategy IN ('llm'));

-- Extend task_status with the dynamic-child + approval states.
ALTER TYPE task_status ADD VALUE IF NOT EXISTS 'awaiting_children';
ALTER TYPE task_status ADD VALUE IF NOT EXISTS 'awaiting_approval';
ALTER TYPE task_status ADD VALUE IF NOT EXISTS 'awaiting_review';

-- Runnable + unblocked = scheduler-eligible. Partial index keeps it cheap.
CREATE INDEX IF NOT EXISTS idx_tasks_runnable
    ON tasks (repo_id, priority, updated_at)
    WHERE runnable = TRUE AND status IN ('open', 'in_progress');

CREATE INDEX IF NOT EXISTS idx_tasks_parent
    ON tasks (parent_task_id)
    WHERE parent_task_id IS NOT NULL;
