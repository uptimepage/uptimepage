SET LOCAL lock_timeout = '5s';

-- An episode that may page owes its first page from the statement that starts
-- it (an open, a declare with paging on, a reopen) until the transaction that
-- records that page, whatever its age. The sweep that finds a lost page reads
-- this instead of guessing from the paging log.
ALTER TABLE incidents ADD COLUMN IF NOT EXISTS page_owed_at TIMESTAMPTZ;

CREATE INDEX IF NOT EXISTS idx_incidents_page_owed ON incidents (page_owed_at)
    WHERE page_owed_at IS NOT NULL AND ended_at IS NULL;

-- Served the scan this replaces, and nothing else.
DROP INDEX IF EXISTS idx_incidents_reconcile;

-- Open incidents whose current episode shows no first page: no paging-log row
-- of that episode, bookkeeping rows included (a hold or "reached nobody" also
-- settles it), and no ladder armed in it. Only arming sets a policy, so a
-- never-reopened incident without one was never armed. A reopen keeps the last
-- episode's policy, so a reopened one counts as armed only with a timer
-- running: an escalated event proves nothing, being written before the arming
-- in a statement of its own. That marks some that armed and left no trace, a
-- policy with no steps or a ladder run out on empty rungs, whose first page
-- then runs again and reaches only what is reachable now.
WITH current AS (
    SELECT i.id,
           (SELECT count(*) FROM incident_events e
             WHERE e.incident_id = i.id AND e.org_id = i.org_id AND e.kind = 'reopened') AS episode,
           (SELECT max(e.occurred_at) FROM incident_events e
             WHERE e.incident_id = i.id AND e.org_id = i.org_id AND e.kind = 'reopened') AS reopened_at
      FROM incidents i
     WHERE i.ended_at IS NULL AND i.paging_enabled AND i.target_id IS NOT NULL
)
UPDATE incidents i
   SET page_owed_at = COALESCE(c.reopened_at, i.started_at)
  FROM current c
 WHERE i.id = c.id
   AND NOT EXISTS (
       SELECT 1 FROM incident_notifications n
        WHERE n.incident_id = i.id AND n.org_id = i.org_id AND n.episode = c.episode
   )
   AND CASE WHEN c.episode = 0 THEN i.escalation_policy_id IS NULL
            ELSE i.next_escalation_at IS NULL END;
