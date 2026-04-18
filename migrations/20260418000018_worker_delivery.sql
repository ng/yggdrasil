-- yggdrasil-54: delivery tracking on workers.
-- 'completed' only means Claude exited — doesn't say whether the branch
-- was pushed, a PR opened, or the branch merged. Ancillary columns
-- keep the state enum clean while exposing delivery status.

ALTER TABLE workers ADD COLUMN IF NOT EXISTS branch_pushed BOOLEAN NOT NULL DEFAULT false;
ALTER TABLE workers ADD COLUMN IF NOT EXISTS branch_merged BOOLEAN NOT NULL DEFAULT false;
ALTER TABLE workers ADD COLUMN IF NOT EXISTS pr_url        TEXT;
ALTER TABLE workers ADD COLUMN IF NOT EXISTS delivery_checked_at TIMESTAMPTZ;
