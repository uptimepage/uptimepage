CREATE INDEX idx_maintenance_live_ends_at
    ON maintenance_windows (ends_at)
    WHERE deleted_at IS NULL;
