-- Backfill empty user_id rows left by the multi-user migration.
-- The DEFAULT '' was correct for schema addition, but existing rows
-- need to be claimed by the current OS user. Since single-user
-- installations have exactly one identity, we resolve it from
-- current_user (the Postgres role), which matches whoami on typical
-- setups. Multi-user installations that already set YGG_USER will
-- have non-empty user_id on their rows and are unaffected.

DO $$
DECLARE
  uid TEXT := current_user;
BEGIN
  UPDATE agents    SET user_id = uid WHERE user_id = '';
  UPDATE repos     SET user_id = uid WHERE user_id = '';
  UPDATE tasks     SET user_id = uid WHERE user_id = '';
  UPDATE locks     SET user_id = uid WHERE user_id = '';
  UPDATE nodes     SET user_id = uid WHERE user_id = '';
  UPDATE sessions  SET user_id = uid WHERE user_id = '';
  UPDATE events    SET user_id = uid WHERE user_id = '';
  UPDATE memories  SET user_id = uid WHERE user_id = '';
  UPDATE workers   SET user_id = uid WHERE user_id = '';
  UPDATE learnings SET user_id = uid WHERE user_id = '';
END $$;
