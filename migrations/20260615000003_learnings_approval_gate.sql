-- ADR 0017: approval gate on learnings. A learning is `pending` or `active`;
-- only `active` is ever surfaced (surface_for_files / list filter status='active').
-- `DEFAULT 'active'` preserves today's behavior exactly — `ygg learn create`
-- still produces an immediately-firing rule, and every existing row stays active.
-- `source` records how the learning entered the corpus: hand-written (`manual`)
-- or agent-proposed (`proposed`). Vocabulary mirrors ADR 0016's task-approval
-- columns (approved_at / approved_by) so the two gates read the same.
ALTER TABLE learnings ADD COLUMN status TEXT NOT NULL DEFAULT 'active'
    CHECK (status IN ('pending', 'active'));
ALTER TABLE learnings ADD COLUMN source TEXT NOT NULL DEFAULT 'manual'
    CHECK (source IN ('manual', 'proposed'));
ALTER TABLE learnings ADD COLUMN approved_at TIMESTAMPTZ;
ALTER TABLE learnings ADD COLUMN approved_by UUID REFERENCES agents(agent_id);

-- Partial index: the pending queue is the only hot lookup the gate adds.
CREATE INDEX idx_learnings_status ON learnings (status) WHERE status = 'pending';
