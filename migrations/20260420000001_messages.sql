-- Messaging bus: inbox pattern on the events table.
-- 'message' is an event kind; recipient_agent_id is the addressed agent;
-- agents.message_cursor tracks each agent's last-seen event.id so the
-- inbox = SELECT FROM events WHERE event_kind='message' AND recipient = $me AND id > $cursor.
-- Events remain the bus; the inbox is a filter + cursor.

ALTER TYPE event_kind ADD VALUE IF NOT EXISTS 'message';

ALTER TABLE events ADD COLUMN IF NOT EXISTS recipient_agent_id UUID REFERENCES agents(agent_id);

-- Partial index: only pays for itself on message events, which are rare.
CREATE INDEX IF NOT EXISTS idx_events_recipient_unread
    ON events (recipient_agent_id, created_at DESC)
    WHERE event_kind = 'message';

ALTER TABLE agents ADD COLUMN IF NOT EXISTS message_cursor TIMESTAMPTZ DEFAULT '1970-01-01'::timestamptz;
