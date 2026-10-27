CREATE INDEX IF NOT EXISTS idx_incidents_reconcile
    ON incidents (started_at)
    WHERE state = 'triggered' AND escalation_policy_id IS NULL AND next_escalation_at IS NULL;
DROP INDEX IF EXISTS idx_incidents_page_owed;
ALTER TABLE incidents DROP COLUMN IF EXISTS page_owed_at;
