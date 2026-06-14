-- Pre-recovery git checkpoint (yggdrasil-115). Before the watcher destroys a
-- reaped agent's worktree, it commits the work-in-progress onto a
-- refs/ygg/recovery/<run_id> ref and stashes that commit's SHA here, so the
-- retry attempt can continue from it instead of resetting to the starting
-- commit on every overnight reap.
ALTER TABLE task_runs ADD COLUMN pre_recovery_commit TEXT;
