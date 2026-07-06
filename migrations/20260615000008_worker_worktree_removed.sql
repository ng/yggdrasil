-- Track worktree teardown separately from window reaping. A completed +
-- pushed + unmerged worker gets its tmux window reaped early (window_reaped)
-- but its worktree deliberately kept for in-place PR iteration. If we reused
-- window_reaped to exclude the row, a branch that merges LATER would never
-- re-enter cleanup and its worktree would leak. worktree_removed marks the
-- second, later step so the row stays eligible for worktree teardown until it
-- actually runs.
ALTER TABLE workers ADD COLUMN worktree_removed BOOLEAN NOT NULL DEFAULT false;
