-- Add new event_kind values for task / remember lifecycle so ygg logs --follow
-- surfaces task movement alongside the existing conversation/lock activity.
ALTER TYPE event_kind ADD VALUE IF NOT EXISTS 'task_created';
ALTER TYPE event_kind ADD VALUE IF NOT EXISTS 'task_status_changed';
ALTER TYPE event_kind ADD VALUE IF NOT EXISTS 'remembered';
