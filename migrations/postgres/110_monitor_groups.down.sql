SET LOCAL lock_timeout = '5s';

DROP TABLE group_silence_members;
DROP TABLE group_silence_episodes;
DROP TABLE status_page_sections;

DROP INDEX idx_incident_notifications_member_range;
DELETE FROM incident_notifications WHERE reason IN ('member_down', 'group_deleted');
ALTER TABLE incident_notifications
    DROP CONSTRAINT incident_notifications_member_range,
    DROP COLUMN member_incident_ids,
    DROP COLUMN member_to_seq,
    DROP COLUMN member_from_seq,
    DROP CONSTRAINT incident_notifications_reason_check,
    ADD CONSTRAINT incident_notifications_reason_check
        CHECK (reason IN ('opened','escalated','resolved','reopened','no_data','data_resumed',
                          'reminder','monitor_deleted'));

-- The old schema has no group incidents. Their members keep their own rows.
DELETE FROM incidents WHERE origin = 'group';
DELETE FROM incident_events WHERE kind IN ('member_down', 'member_up', 'group_deleted', 'detached');
ALTER TABLE incident_events DROP CONSTRAINT incident_events_kind_check;
ALTER TABLE incident_events ADD CONSTRAINT incident_events_kind_check
    CHECK (kind IN (
      'triggered','acknowledged','assigned','unassigned',
      'escalated','notified','note','severity_changed','downtime_changed',
      'state_changed','resolved','reopened','published','unpublished',
      'postmortem_published','postmortem_unpublished','monitor_deleted'
    ));

DROP TRIGGER trg_incidents_member_points_at_group ON incidents;
DROP FUNCTION incidents_member_points_at_group();
DROP TRIGGER trg_incidents_routed_group_org_match ON incidents;
DROP TRIGGER trg_monitor_groups_rename_incidents ON monitor_groups;
DROP FUNCTION monitor_groups_rename_incidents();
DROP TRIGGER trg_incidents_snapshot_group ON incidents;
DROP FUNCTION incidents_snapshot_group();

ALTER TABLE incidents
    DROP CONSTRAINT incident_group_named,
    DROP CONSTRAINT incident_member_is_monitor,
    DROP CONSTRAINT incident_not_its_own_group,
    DROP CONSTRAINT incident_member_notice_bounded,
    DROP COLUMN group_member_seq,
    DROP COLUMN last_member_notice_at,
    DROP COLUMN member_notice_seq,
    DROP COLUMN member_seq,
    DROP COLUMN detached_at,
    DROP COLUMN closed_by_group_delete,
    DROP COLUMN group_incident_id,
    DROP COLUMN routed_group_id,
    DROP COLUMN group_name,
    DROP COLUMN group_ref,
    DROP COLUMN group_id;

ALTER TABLE incidents DROP CONSTRAINT incident_monitor_named;
ALTER TABLE incidents ADD CONSTRAINT incident_monitor_named CHECK (
    (target_id IS NULL OR (target_ref = target_id AND target_name IS NOT NULL))
    AND ((target_ref IS NULL) = (target_name IS NULL))
    AND (origin = 'manual' OR target_ref IS NOT NULL)
);
ALTER TABLE incidents DROP CONSTRAINT incidents_origin_check;
ALTER TABLE incidents ADD CONSTRAINT incidents_origin_check
    CHECK (origin IN ('monitor', 'manual'));

-- Back to the label: the name each monitor's group has now. The guard goes
-- first; it refuses the rebuild.
DROP TRIGGER trg_targets_group_name_frozen ON targets;
DROP FUNCTION targets_group_name_frozen();
UPDATE targets t
   SET group_name = g.name
  FROM (SELECT t2.id, mg.name
          FROM targets t2 LEFT JOIN monitor_groups mg ON mg.id = t2.group_id) g
 WHERE g.id = t.id AND t.group_name IS DISTINCT FROM g.name;
DROP TRIGGER trg_targets_group_org_match ON targets;
DROP INDEX idx_targets_group;
ALTER TABLE targets DROP COLUMN group_id;

DROP TABLE group_channels;
DROP TABLE monitor_groups;
DROP FUNCTION group_name_trim(TEXT);
