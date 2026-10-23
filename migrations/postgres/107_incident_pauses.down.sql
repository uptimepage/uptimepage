ALTER TABLE incidents
    DROP COLUMN IF EXISTS paused_until,
    DROP COLUMN IF EXISTS paused_from,
    DROP COLUMN IF EXISTS paused_since;
