-- yggdrasil-180: record recency of learning application alongside the
-- existing applied_count counter. Lets `ygg learn list` triage active vs
-- stale learnings (last-used). NULL = never applied.
ALTER TABLE learnings ADD COLUMN last_applied_at TIMESTAMPTZ;
