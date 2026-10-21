DROP TRIGGER IF EXISTS trg_targets_edit_clears_slowed ON targets;
DROP FUNCTION IF EXISTS targets_edit_clears_slowed();
DROP INDEX IF EXISTS idx_targets_slowed;

ALTER TABLE targets DROP COLUMN IF EXISTS slowed_notified_at;
ALTER TABLE targets DROP COLUMN IF EXISTS slowed_at;
