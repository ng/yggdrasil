-- Keep agent_name as the cwd-basename identity; add persona as a separate
-- slot so (agent_name, persona) forms a compound key when a persona is set.
-- This preserves the "two parallel CC sessions in the same repo but with
-- different roles" case without squashing them into a "name:persona"
-- string that's painful to query.

ALTER TABLE agents ADD COLUMN IF NOT EXISTS persona TEXT;

-- Allow (name, NULL) plus (name, persona) to coexist as distinct identities.
-- The existing UNIQUE(agent_name) constraint is replaced by a composite one
-- that treats NULL persona as its own bucket — so legacy rows are fine,
-- and multiple personas under the same name are allowed.
ALTER TABLE agents DROP CONSTRAINT IF EXISTS agents_agent_name_key;
CREATE UNIQUE INDEX IF NOT EXISTS agents_name_persona_uk
    ON agents (agent_name, COALESCE(persona, ''));
