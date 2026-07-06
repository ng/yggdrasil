-- One-shot marker set once the watcher has reaped a cleanable worker's tmux
-- window (and, when fully delivered, its worktree). Without it, list_cleanable
-- keeps re-selecting the same already-cleaned rows every tick — re-killing a
-- dead window and starving the LIMIT budget for genuinely-new cleanable
-- workers. list_cleanable filters on window_reaped = false so each row drains
-- after one pass.
ALTER TABLE workers ADD COLUMN window_reaped BOOLEAN NOT NULL DEFAULT false;
