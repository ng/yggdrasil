-- Partial index for the Chat panel's all_messages query.
-- Filters on event_kind = 'message' with ORDER BY created_at DESC.
CREATE INDEX IF NOT EXISTS idx_events_message_created
    ON events (created_at DESC)
    WHERE event_kind = 'message';
