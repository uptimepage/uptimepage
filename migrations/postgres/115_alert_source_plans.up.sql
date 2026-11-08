-- Alert sources come with Team. The event budget is the same on every plan:
-- a held source's resolves must still close.
ALTER TABLE plans
    ADD COLUMN max_alert_sources       INTEGER NOT NULL DEFAULT 0 CHECK (max_alert_sources >= 0),
    ADD COLUMN alert_events_per_minute INTEGER NOT NULL DEFAULT 1200 CHECK (alert_events_per_minute > 0);
UPDATE plans SET max_alert_sources = 5, updated_at = now() WHERE id = 'team';

CREATE INDEX idx_alert_sources_plan_hold ON alert_sources (org_id) WHERE plan_hold_at IS NOT NULL;
