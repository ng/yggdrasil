-- yggdrasil-38: task relevance + link kinds.
--
-- relevance is a 0..100 score (default 50). It doesn't change task
-- priority — priority is a coarse bucket, relevance is a fine-grained
-- nudge for retrieval and display ordering. Raising a task's relevance
-- is the cheap "this is more load-bearing than I first thought" gesture.

ALTER TABLE tasks ADD COLUMN IF NOT EXISTS relevance INT NOT NULL DEFAULT 50;
ALTER TABLE tasks ADD CONSTRAINT tasks_relevance_range
    CHECK (relevance BETWEEN 0 AND 100);

-- Non-blocker relationships between tasks. Separate from task_deps because
-- task_deps is a strict "can't proceed until" edge; these are informational.
CREATE TYPE task_link_kind AS ENUM (
    'see_also',
    'superseded_by',
    'duplicate_of',
    'related'
);

CREATE TABLE task_links (
    task_id       UUID NOT NULL REFERENCES tasks(task_id) ON DELETE CASCADE,
    target_id     UUID NOT NULL REFERENCES tasks(task_id) ON DELETE CASCADE,
    kind          task_link_kind NOT NULL,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (task_id, target_id, kind)
);

CREATE INDEX idx_task_links_task   ON task_links (task_id);
CREATE INDEX idx_task_links_target ON task_links (target_id);
