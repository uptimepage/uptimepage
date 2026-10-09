ALTER TABLE incidents
    DROP COLUMN IF EXISTS recovered_until,
    DROP COLUMN IF EXISTS recovered_from,
    DROP COLUMN IF EXISTS recovering_since;
ALTER TABLE targets DROP COLUMN IF EXISTS recovery_period_secs;
