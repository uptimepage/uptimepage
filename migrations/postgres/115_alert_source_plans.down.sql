DROP INDEX IF EXISTS idx_alert_sources_plan_hold;
ALTER TABLE plans DROP COLUMN IF EXISTS alert_events_per_minute;
ALTER TABLE plans DROP COLUMN IF EXISTS max_alert_sources;
