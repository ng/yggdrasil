-- Singleton heartbeat row that time-throttles the opportunistic watcher tick
-- baked into the PreToolUse hook. Every agent tool/ygg call would otherwise be
-- a candidate to run a full maintenance tick (tmux probes + git checks) when no
-- dashboard or `ygg watcher` daemon is up; the atomic claim on last_tick_at
-- lets exactly one caller per interval win, across all sessions, so the
-- watcher-of-last-resort self-heals the fleet without stampeding.
CREATE TABLE watcher_heartbeat (
    id           BOOLEAN PRIMARY KEY DEFAULT true,
    last_tick_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT watcher_heartbeat_singleton CHECK (id)
);
INSERT INTO watcher_heartbeat (id) VALUES (true);
