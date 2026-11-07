SET LOCAL lock_timeout = '5s';

-- A tool that sends alerts in (Alertmanager, Grafana, Datadog, any webhook) and
-- the route its pages take. Token discipline is the heartbeat one: the SHA-256
-- of the secret is the lookup key, a sealed copy re-displays the URL, and a
-- superseded token is accepted until it expires but never shown again.
CREATE TABLE alert_sources (
    id                      UUID PRIMARY KEY DEFAULT uuidv7(),
    org_id                  UUID NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    name                    TEXT NOT NULL CHECK (char_length(name) BETWEEN 1 AND 100 AND name = group_name_trim(name)),
    kind                    TEXT NOT NULL CHECK (kind IN ('generic', 'alertmanager', 'grafana', 'datadog')),
    enabled                 BOOLEAN NOT NULL DEFAULT true,
    plan_hold_at            TIMESTAMPTZ,
    token_hash              TEXT NOT NULL UNIQUE,
    token_enc               TEXT NOT NULL,
    prev_token_hash         TEXT,
    prev_token_expires_at   TIMESTAMPTZ,
    prev_token_last_used_at TIMESTAMPTZ,
    token_rotated_at        TIMESTAMPTZ,
    signing_secret_enc      TEXT,
    grouping                TEXT NOT NULL DEFAULT 'group' CHECK (grouping IN ('group', 'alert')),
    escalation_policy_id    UUID REFERENCES escalation_policies(id) ON DELETE SET NULL,
    notify_recovery         BOOLEAN NOT NULL DEFAULT true,
    renotify_interval_secs  INTEGER NOT NULL DEFAULT 3600 CHECK (renotify_interval_secs >= 0),
    last_event_at           TIMESTAMPTZ,
    write_source            TEXT NOT NULL DEFAULT 'ui' CHECK (write_source IN ('ui', 'api', 'terraform')),
    created_at              TIMESTAMPTZ NOT NULL DEFAULT now(),
    created_by              UUID REFERENCES users(id) ON DELETE SET NULL,
    updated_at              TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_by              UUID REFERENCES users(id) ON DELETE SET NULL,
    UNIQUE (org_id, name),
    -- A previous hash with no expiry would be accepted forever.
    CONSTRAINT alert_sources_prev_token_shape
        CHECK ((prev_token_hash IS NULL) = (prev_token_expires_at IS NULL)
               AND (prev_token_last_used_at IS NULL OR prev_token_hash IS NOT NULL))
);
CREATE UNIQUE INDEX idx_alert_sources_prev_token
    ON alert_sources (prev_token_hash) WHERE prev_token_hash IS NOT NULL;
CREATE TRIGGER trg_alert_sources_policy_org_match
    BEFORE INSERT OR UPDATE OF escalation_policy_id, org_id ON alert_sources
    FOR EACH ROW WHEN (NEW.escalation_policy_id IS NOT NULL)
    EXECUTE FUNCTION assert_org_matches_parent('escalation_policies', 'escalation_policy_id');
CREATE INDEX idx_alert_sources_policy ON alert_sources (escalation_policy_id) WHERE escalation_policy_id IS NOT NULL;
CREATE INDEX idx_alert_sources_created_by ON alert_sources (created_by) WHERE created_by IS NOT NULL;
CREATE INDEX idx_alert_sources_updated_by ON alert_sources (updated_by) WHERE updated_by IS NOT NULL;

CREATE TABLE alert_source_channels (
    source_id  UUID NOT NULL REFERENCES alert_sources(id) ON DELETE CASCADE,
    channel_id UUID NOT NULL REFERENCES notification_channels(id) ON DELETE CASCADE,
    org_id     UUID NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    position   INT  NOT NULL,
    PRIMARY KEY (source_id, channel_id)
);
CREATE INDEX alert_source_channels_channel_idx ON alert_source_channels (channel_id);
CREATE TRIGGER trg_alert_source_channels_source_org_match
    BEFORE INSERT OR UPDATE OF source_id, org_id ON alert_source_channels
    FOR EACH ROW EXECUTE FUNCTION assert_org_matches_parent('alert_sources', 'source_id');
CREATE TRIGGER trg_alert_source_channels_channel_org_match
    BEFORE INSERT OR UPDATE OF channel_id, org_id ON alert_source_channels
    FOR EACH ROW EXECUTE FUNCTION assert_org_matches_parent('notification_channels', 'channel_id');

ALTER TABLE incidents DROP CONSTRAINT incidents_origin_check;
ALTER TABLE incidents ADD CONSTRAINT incidents_origin_check
    CHECK (origin IN ('monitor', 'manual', 'group', 'alert'));

ALTER TABLE incidents DROP CONSTRAINT incident_monitor_named;
ALTER TABLE incidents ADD CONSTRAINT incident_monitor_named CHECK (
    (target_id IS NULL OR (target_ref = target_id AND target_name IS NOT NULL))
    AND ((target_ref IS NULL) = (target_name IS NULL))
    AND (origin IN ('manual', 'group', 'alert') OR target_ref IS NOT NULL)
);

-- The source survives on its incidents as alert_source_ref and
-- alert_source_name after it is deleted, as group_ref does for a group.
ALTER TABLE incidents
    ADD COLUMN alert_source_id   UUID REFERENCES alert_sources(id) ON DELETE SET NULL,
    ADD COLUMN alert_source_ref  UUID,
    ADD COLUMN alert_source_name TEXT,
    ADD COLUMN dedup_key         TEXT,
    ADD CONSTRAINT incident_alert_named CHECK (
        (alert_source_id IS NULL OR alert_source_ref = alert_source_id)
        AND ((alert_source_ref IS NULL) = (alert_source_name IS NULL))
        AND ((origin = 'alert') = (alert_source_ref IS NOT NULL))
        AND ((origin = 'alert') = (dedup_key IS NOT NULL))
        AND (origin <> 'alert' OR target_ref IS NULL));

-- FOR SHARE waits out a rename in flight, whose own update cannot see this row.
CREATE FUNCTION incidents_snapshot_alert_source() RETURNS TRIGGER AS $$
DECLARE
    source_org UUID;
BEGIN
    IF NEW.alert_source_id IS NULL THEN
        RETURN NEW;
    END IF;
    SELECT org_id, name INTO source_org, NEW.alert_source_name
    FROM alert_sources WHERE id = NEW.alert_source_id FOR SHARE;
    IF source_org IS NULL OR source_org <> NEW.org_id THEN
        RAISE EXCEPTION 'alert_source_id % belongs to org % but the incident is under org %',
            NEW.alert_source_id, source_org, NEW.org_id
            USING ERRCODE = '23503', CONSTRAINT = 'incidents_alert_source_id_fkey';
    END IF;
    NEW.alert_source_ref := NEW.alert_source_id;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql
SET search_path = pg_catalog, public;
CREATE TRIGGER trg_incidents_snapshot_alert_source
    BEFORE INSERT OR UPDATE OF alert_source_id, org_id ON incidents
    FOR EACH ROW EXECUTE FUNCTION incidents_snapshot_alert_source();

CREATE FUNCTION alert_sources_rename_incidents() RETURNS TRIGGER AS $$
BEGIN
    UPDATE incidents SET alert_source_name = NEW.name
    WHERE org_id = NEW.org_id AND alert_source_id = NEW.id;
    RETURN NULL;
END;
$$ LANGUAGE plpgsql
SET search_path = pg_catalog, public;
CREATE TRIGGER trg_alert_sources_rename_incidents
    AFTER UPDATE OF name ON alert_sources
    FOR EACH ROW WHEN (OLD.name IS DISTINCT FROM NEW.name)
    EXECUTE FUNCTION alert_sources_rename_incidents();

-- One open incident per source and key, which two concurrent firings race on.
CREATE UNIQUE INDEX idx_incidents_org_open_alert
    ON incidents (org_id, alert_source_id, dedup_key)
    WHERE ended_at IS NULL AND origin = 'alert';
-- The last incident of a key, for flap damping and stale resolves; by ref so
-- it still answers once the source is gone.
CREATE INDEX idx_incidents_alert_key_recent
    ON incidents (org_id, alert_source_ref, dedup_key, started_at DESC)
    WHERE origin = 'alert';
CREATE INDEX idx_incidents_alert_source
    ON incidents (alert_source_id) WHERE alert_source_id IS NOT NULL;

-- What changes on every repeat stays off the row the engine locks.
CREATE TABLE incident_alert_state (
    incident_id   UUID PRIMARY KEY REFERENCES incidents(id) ON DELETE CASCADE,
    org_id        UUID NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    last_event_at TIMESTAMPTZ NOT NULL,
    repeat_count  INTEGER NOT NULL DEFAULT 0 CHECK (repeat_count >= 0),
    firing_count  INTEGER NOT NULL DEFAULT 1 CHECK (firing_count >= 0),
    -- The sender's clock: the latest firing seen, NULL when it sent none.
    fired_at      TIMESTAMPTZ,
    labels        JSONB NOT NULL DEFAULT '{}',
    annotations   JSONB NOT NULL DEFAULT '{}',
    links         JSONB NOT NULL DEFAULT '[]'
);
CREATE TRIGGER trg_incident_alert_state_incident_org_match
    BEFORE INSERT OR UPDATE OF incident_id, org_id ON incident_alert_state
    FOR EACH ROW EXECUTE FUNCTION assert_org_matches_parent('incidents', 'incident_id');
