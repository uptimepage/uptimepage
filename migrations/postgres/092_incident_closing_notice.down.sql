ALTER TABLE incident_notifications DROP COLUMN IF EXISTS episode;
DROP INDEX IF EXISTS idx_incidents_closing_notice;
ALTER TABLE incidents DROP COLUMN IF EXISTS closing_notice_at;
