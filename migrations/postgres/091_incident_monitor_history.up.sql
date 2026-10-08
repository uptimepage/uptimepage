-- Fail the boot rather than queue every incident and monitor write behind the change.
SET LOCAL lock_timeout = '5s';

-- An incident outlives its monitor. Deleting a monitor used to cascade its
-- incidents, their timeline and the alerts sent for them out of existence, so
-- nobody could say afterwards what was paged or for how long. The incident now
-- carries the monitor's id, name and kind, and the link to the monitor is
-- cleared instead of the incident being dropped. target_ref keeps the id after
-- the link is gone, so two deleted monitors that shared a name stay apart.
-- closed_by_monitor_delete marks an incident the delete closed: nothing was
-- resolved, so it stays out of the resolution metrics.
ALTER TABLE incidents
    ADD COLUMN target_ref UUID,
    ADD COLUMN target_name TEXT,
    ADD COLUMN target_kind TEXT,
    ADD COLUMN closed_by_monitor_delete BOOLEAN NOT NULL DEFAULT false;

UPDATE incidents i
SET target_ref = t.id, target_name = t.name, target_kind = t.kind
FROM targets t
WHERE t.id = i.target_id;

-- Every writer goes through this, so no insert path can leave the name out or
-- point an incident at another org's monitor. Clearing target_id (the monitor
-- was deleted) keeps the name already on the row. FOR SHARE waits out a rename
-- in flight, whose own update cannot see this uncommitted row.
CREATE OR REPLACE FUNCTION incidents_snapshot_monitor() RETURNS TRIGGER AS $$
DECLARE
    monitor_org UUID;
BEGIN
    IF NEW.target_id IS NULL THEN
        RETURN NEW;
    END IF;
    SELECT org_id, name, kind INTO monitor_org, NEW.target_name, NEW.target_kind
    FROM targets WHERE id = NEW.target_id FOR SHARE;
    IF monitor_org IS NULL OR monitor_org <> NEW.org_id THEN
        RAISE EXCEPTION 'target_id % belongs to org % but the incident is under org %',
            NEW.target_id, monitor_org, NEW.org_id
            USING ERRCODE = '23503', CONSTRAINT = 'incidents_target_id_fkey';
    END IF;
    NEW.target_ref := NEW.target_id;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql
SET search_path = pg_catalog, public;

CREATE TRIGGER trg_incidents_snapshot_monitor
    BEFORE INSERT OR UPDATE OF target_id ON incidents
    FOR EACH ROW EXECUTE FUNCTION incidents_snapshot_monitor();

-- A rename shows on the monitor's incidents, as it did when the name was read
-- through the join.
CREATE OR REPLACE FUNCTION targets_rename_incidents() RETURNS TRIGGER AS $$
BEGIN
    UPDATE incidents SET target_name = NEW.name
    WHERE org_id = NEW.org_id AND target_id = NEW.id;
    RETURN NULL;
END;
$$ LANGUAGE plpgsql
SET search_path = pg_catalog, public;

CREATE TRIGGER trg_targets_rename_incidents
    AFTER UPDATE OF name ON targets
    FOR EACH ROW WHEN (OLD.name IS DISTINCT FROM NEW.name)
    EXECUTE FUNCTION targets_rename_incidents();

-- A monitor-origin incident no longer needs a live monitor, but it always
-- knows which monitor it was. An org purge clears target_id before it deletes
-- the incidents, so this has to hold with the link gone.
ALTER TABLE incidents DROP CONSTRAINT incident_monitor_has_target;
ALTER TABLE incidents ADD CONSTRAINT incident_monitor_named CHECK (
    (target_id IS NULL OR (target_ref = target_id AND target_name IS NOT NULL))
    AND ((target_ref IS NULL) = (target_name IS NULL))
    AND (origin = 'manual' OR target_ref IS NOT NULL)
);

ALTER TABLE incidents
    DROP CONSTRAINT incidents_target_id_fkey,
    ADD CONSTRAINT incidents_target_id_fkey
        FOREIGN KEY (target_id) REFERENCES targets(id) ON DELETE SET NULL;

-- Constraint keeps its generated name; the drift test looks it up by name.
ALTER TABLE incident_events DROP CONSTRAINT incident_events_kind_check;
ALTER TABLE incident_events ADD CONSTRAINT incident_events_kind_check
    CHECK (kind IN (
      'triggered','acknowledged','assigned','unassigned',
      'escalated','notified','note','severity_changed','downtime_changed',
      'state_changed','resolved','reopened','published','unpublished',
      'postmortem_published','postmortem_unpublished','monitor_deleted'
    ));

-- Closing a monitor's incident when the monitor is deleted tells the responders
-- it paged, in its own words: nothing recovered.
ALTER TABLE incident_notifications
    DROP CONSTRAINT incident_notifications_reason_check,
    ADD CONSTRAINT incident_notifications_reason_check
        CHECK (reason IN ('opened','escalated','resolved','reopened','no_data','data_resumed',
                          'reminder','monitor_deleted'));

-- A public incident whose monitor is deleted stays on the pages that showed it,
-- under the name each page gave the monitor.
ALTER TABLE incident_status_pages ADD COLUMN component_name TEXT;
