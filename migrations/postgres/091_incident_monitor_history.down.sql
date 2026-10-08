ALTER TABLE incident_status_pages DROP COLUMN IF EXISTS component_name;

DELETE FROM incident_notifications WHERE reason = 'monitor_deleted';
ALTER TABLE incident_notifications
    DROP CONSTRAINT incident_notifications_reason_check,
    ADD CONSTRAINT incident_notifications_reason_check
        CHECK (reason IN ('opened','escalated','resolved','reopened','no_data','data_resumed','reminder'));

DROP TRIGGER IF EXISTS trg_targets_rename_incidents ON targets;
DROP FUNCTION IF EXISTS targets_rename_incidents();
DROP TRIGGER IF EXISTS trg_incidents_snapshot_monitor ON incidents;
DROP FUNCTION IF EXISTS incidents_snapshot_monitor();

DELETE FROM incident_events WHERE kind = 'monitor_deleted';
ALTER TABLE incident_events DROP CONSTRAINT incident_events_kind_check;
ALTER TABLE incident_events ADD CONSTRAINT incident_events_kind_check
    CHECK (kind IN (
      'triggered','acknowledged','assigned','unassigned',
      'escalated','notified','note','severity_changed','downtime_changed',
      'state_changed','resolved','reopened','published','unpublished',
      'postmortem_published','postmortem_unpublished'
    ));

-- The old schema cannot hold a monitor incident without its monitor.
DELETE FROM incidents WHERE origin = 'monitor' AND target_id IS NULL;

ALTER TABLE incidents
    DROP CONSTRAINT incidents_target_id_fkey,
    ADD CONSTRAINT incidents_target_id_fkey
        FOREIGN KEY (target_id) REFERENCES targets(id) ON DELETE CASCADE;

ALTER TABLE incidents DROP CONSTRAINT incident_monitor_named;
ALTER TABLE incidents ADD CONSTRAINT incident_monitor_has_target
    CHECK (origin = 'manual' OR target_id IS NOT NULL);

ALTER TABLE incidents
    DROP COLUMN IF EXISTS closed_by_monitor_delete,
    DROP COLUMN IF EXISTS target_kind,
    DROP COLUMN IF EXISTS target_name,
    DROP COLUMN IF EXISTS target_ref;
