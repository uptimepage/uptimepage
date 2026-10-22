DROP INDEX IF EXISTS idx_incidents_monitor_ended;
DROP INDEX IF EXISTS idx_incident_updates_posted;
DROP TABLE IF EXISTS status_page_delivery_batches;
DROP TABLE IF EXISTS status_page_subscriber_deliveries;
DROP TABLE IF EXISTS status_page_notices;
DROP TABLE IF EXISTS status_page_outage_members;
DROP TABLE IF EXISTS status_page_outages;

CREATE TABLE status_page_subscriber_deliveries (
    id            UUID PRIMARY KEY DEFAULT uuidv7(),
    subscriber_id UUID NOT NULL REFERENCES status_page_subscribers(id) ON DELETE CASCADE,
    org_id        UUID NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    event_kind    TEXT NOT NULL CHECK (event_kind IN ('incident_update', 'maintenance', 'maintenance_update')),
    event_ref     UUID NOT NULL,
    phase         TEXT NOT NULL DEFAULT '',
    status        TEXT NOT NULL CHECK (status IN ('queued', 'sent', 'failed')),
    attempts      INT NOT NULL DEFAULT 0,
    error         TEXT,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    sent_at       TIMESTAMPTZ,
    UNIQUE (subscriber_id, event_kind, event_ref, phase)
);

CREATE INDEX idx_sp_subscriber_deliveries_org
    ON status_page_subscriber_deliveries (org_id, created_at);

CREATE TRIGGER trg_sp_subscriber_deliveries_org_match
    BEFORE INSERT OR UPDATE OF subscriber_id, org_id ON status_page_subscriber_deliveries
    FOR EACH ROW EXECUTE FUNCTION assert_org_matches_parent('status_page_subscribers', 'subscriber_id');
