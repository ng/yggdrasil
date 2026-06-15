-- yggdrasil-183: thematic agent names. A task may carry an `agent_slug` — a
-- short, human-meaningful name the creating agent picks for the worker that
-- will run it (e.g. "oauth-refresh"). The scheduler uses it as the spawned
-- agent's name prefix instead of the generic `ygg-<prefix>-<seq>` form, and
-- spawn exports it as YGG_AGENT_NAME so hooks stop inferring identity from the
-- (often stale) worktree directory basename. NULL = fall back to the old
-- scheme. Sanitized at the CLI boundary to [a-z0-9-].
ALTER TABLE tasks ADD COLUMN agent_slug TEXT;
