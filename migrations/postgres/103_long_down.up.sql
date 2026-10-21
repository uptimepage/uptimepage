-- A monitor down from every region for days is checked at a slower rate until
-- it answers again. Applied when the work is handed out; `interval_secs` is
-- never rewritten, so clearing `slowed_at` is the whole restore.
ALTER TABLE targets ADD COLUMN slowed_at TIMESTAMPTZ;
-- When its owner was last told, so each slow episode mails once.
ALTER TABLE targets ADD COLUMN slowed_notified_at TIMESTAMPTZ;

CREATE INDEX idx_targets_slowed ON targets(slowed_at) WHERE slowed_at IS NOT NULL;

-- Every write that moves `updated_at` (an edit, a plan hold or release, its
-- owner leaving the org) puts a slowed monitor back on its own interval. The
-- slow pass leaves `updated_at` alone, so it never trips this.
CREATE FUNCTION targets_edit_clears_slowed() RETURNS TRIGGER AS $$
BEGIN
    NEW.slowed_at := NULL;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql
SET search_path = pg_catalog, public;

CREATE TRIGGER trg_targets_edit_clears_slowed
    BEFORE UPDATE ON targets
    FOR EACH ROW WHEN (OLD.slowed_at IS NOT NULL AND OLD.updated_at IS DISTINCT FROM NEW.updated_at)
    EXECUTE FUNCTION targets_edit_clears_slowed();
