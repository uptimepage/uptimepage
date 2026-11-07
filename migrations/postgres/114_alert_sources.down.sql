SET LOCAL lock_timeout = '5s';

DROP TABLE incident_alert_state;

-- The old schema has no alert incidents.
DELETE FROM incidents WHERE origin = 'alert';

DROP INDEX idx_incidents_alert_source;
DROP INDEX idx_incidents_alert_key_recent;
DROP INDEX idx_incidents_org_open_alert;

DROP TRIGGER trg_alert_sources_rename_incidents ON alert_sources;
DROP FUNCTION alert_sources_rename_incidents();
DROP TRIGGER trg_incidents_snapshot_alert_source ON incidents;
DROP FUNCTION incidents_snapshot_alert_source();

ALTER TABLE incidents
    DROP CONSTRAINT incident_alert_named,
    DROP COLUMN dedup_key,
    DROP COLUMN alert_source_name,
    DROP COLUMN alert_source_ref,
    DROP COLUMN alert_source_id;

ALTER TABLE incidents DROP CONSTRAINT incident_monitor_named;
ALTER TABLE incidents ADD CONSTRAINT incident_monitor_named CHECK (
    (target_id IS NULL OR (target_ref = target_id AND target_name IS NOT NULL))
    AND ((target_ref IS NULL) = (target_name IS NULL))
    AND (origin IN ('manual', 'group') OR target_ref IS NOT NULL)
);

ALTER TABLE incidents DROP CONSTRAINT incidents_origin_check;
ALTER TABLE incidents ADD CONSTRAINT incidents_origin_check
    CHECK (origin IN ('monitor', 'manual', 'group'));

DROP TABLE alert_source_channels;
DROP TABLE alert_sources;
