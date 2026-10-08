-- Fail the boot rather than queue every incident write and page behind the
-- change.
SET LOCAL lock_timeout = '5s';

-- The notice that tells responders an incident ended is requested through an
-- in-process signal after the close commits, so a restart or a full signal
-- queue used to lose it for good. Every close now records here, in the same
-- statement, that the notice is owed; a reopen withdraws it. A value at or
-- before now is owed and free to take. A later one is leased by whoever is
-- sending it, and falls due again if that sender dies. Cleared once sent.
-- Rows closed before this column existed stay NULL and owe nothing.
ALTER TABLE incidents ADD COLUMN closing_notice_at TIMESTAMPTZ;

CREATE INDEX idx_incidents_closing_notice
    ON incidents (closing_notice_at)
    WHERE closing_notice_at IS NOT NULL;

-- The episode (how many times the incident had reopened) each page was sent
-- for. A duplicate open signal is recognised by a page already sent for the
-- current episode, so a close whose notice never went out still lets the
-- reopen after it page. Telling episodes apart by time instead would count a
-- page the last episode was still sending when the incident reopened.
ALTER TABLE incident_notifications ADD COLUMN episode BIGINT NOT NULL DEFAULT 0;

-- Pages already sent belong to the episode that was current when each was
-- recorded. Only incidents that have reopened have any but the first.
UPDATE incident_notifications n
SET episode = (
    SELECT count(*) FROM incident_events e
    WHERE e.incident_id = n.incident_id AND e.org_id = n.org_id
      AND e.kind = 'reopened' AND e.occurred_at <= n.created_at
)
WHERE EXISTS (
    SELECT 1 FROM incident_events e
    WHERE e.incident_id = n.incident_id AND e.org_id = n.org_id AND e.kind = 'reopened'
);
