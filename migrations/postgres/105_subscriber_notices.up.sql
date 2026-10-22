-- Subscriber mail moves from one row per (subscriber, update), worked out for
-- every subscriber on every tick, to notices: one per thing a page's
-- subscribers should hear, fanned out once into deliveries. Monitor incidents
-- that fail together on a page form one outage, told once when it starts and
-- once when it ends.

-- What the old log sent, or was sending, to some subscriber of each page.
-- Read before the table goes, so the seed below marks only that as told and
-- an update posted moments before the deploy still goes out.
CREATE TEMP TABLE told ON COMMIT DROP AS
SELECT DISTINCT s.status_page_id, d.org_id, d.event_kind, d.event_ref, d.phase
  FROM status_page_subscriber_deliveries d
  JOIN status_page_subscribers s ON s.id = d.subscriber_id AND s.org_id = d.org_id;

DROP TABLE status_page_subscriber_deliveries;

CREATE TABLE status_page_outages (
    id             UUID PRIMARY KEY DEFAULT uuidv7(),
    org_id         UUID NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    status_page_id UUID NOT NULL REFERENCES status_pages(id) ON DELETE CASCADE,
    -- When the page first said something about its first member.
    opened_at      TIMESTAMPTZ NOT NULL,
    -- The earliest member's start, for the mails.
    started_at     TIMESTAMPTZ NOT NULL,
    -- Set while every member is closed: the latest of their closes.
    recovered_at   TIMESTAMPTZ,
    resolved_at    TIMESTAMPTZ,
    -- Its started mail went out, or its start was told some other way.
    started_told   BOOLEAN NOT NULL DEFAULT false,
    created_at     TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX idx_sp_outages_open ON status_page_outages (status_page_id)
    WHERE resolved_at IS NULL;
CREATE INDEX idx_sp_outages_resolved ON status_page_outages (resolved_at)
    WHERE resolved_at IS NOT NULL;

CREATE TRIGGER trg_sp_outages_org_match
    BEFORE INSERT OR UPDATE OF status_page_id, org_id ON status_page_outages
    FOR EACH ROW EXECUTE FUNCTION assert_org_matches_parent('status_pages', 'status_page_id');

CREATE TABLE status_page_outage_members (
    outage_id      UUID NOT NULL REFERENCES status_page_outages(id) ON DELETE CASCADE,
    org_id         UUID NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    status_page_id UUID NOT NULL,
    incident_id    UUID NOT NULL REFERENCES incidents(id) ON DELETE CASCADE,
    target_id      UUID NOT NULL,
    component_name TEXT NOT NULL,
    joined_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    -- When it recovered while still on the page; stays NULL for one the page
    -- stopped showing while it was down.
    ended_at       TIMESTAMPTZ,
    closed_at      TIMESTAMPTZ,
    PRIMARY KEY (outage_id, incident_id)
);

-- An open incident sits in one open membership per page.
CREATE UNIQUE INDEX idx_sp_outage_members_open
    ON status_page_outage_members (status_page_id, incident_id) WHERE closed_at IS NULL;
CREATE INDEX idx_sp_outage_members_incident
    ON status_page_outage_members (status_page_id, incident_id);

CREATE TRIGGER trg_sp_outage_members_org_match
    BEFORE INSERT OR UPDATE OF outage_id, org_id ON status_page_outage_members
    FOR EACH ROW EXECUTE FUNCTION assert_org_matches_parent('status_page_outages', 'outage_id');

CREATE TABLE status_page_notices (
    id             UUID PRIMARY KEY DEFAULT uuidv7(),
    org_id         UUID NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    status_page_id UUID NOT NULL REFERENCES status_pages(id) ON DELETE CASCADE,
    kind           TEXT NOT NULL CHECK (kind IN ('incident_update', 'outage_started',
                                                 'outage_resolved', 'maintenance',
                                                 'maintenance_update')),
    -- The update, outage or window the notice is about; no FK because it
    -- points at several tables.
    source_ref     UUID NOT NULL,
    phase          TEXT NOT NULL DEFAULT '',
    channels       TEXT[] NOT NULL,
    -- Everything the mail or POST says, so every subscriber gets the same
    -- words and a retry renders the same bytes.
    payload        JSONB NOT NULL,
    fanned_out_at  TIMESTAMPTZ,
    created_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (status_page_id, kind, source_ref, phase)
);

CREATE INDEX idx_sp_notices_unfanned ON status_page_notices (created_at)
    WHERE fanned_out_at IS NULL;
CREATE INDEX idx_sp_notices_created ON status_page_notices (created_at);

CREATE TRIGGER trg_sp_notices_org_match
    BEFORE INSERT OR UPDATE OF status_page_id, org_id ON status_page_notices
    FOR EACH ROW EXECUTE FUNCTION assert_org_matches_parent('status_pages', 'status_page_id');

CREATE TABLE status_page_subscriber_deliveries (
    id              UUID PRIMARY KEY DEFAULT uuidv7(),
    org_id          UUID NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    notice_id       UUID NOT NULL REFERENCES status_page_notices(id) ON DELETE CASCADE,
    subscriber_id   UUID NOT NULL REFERENCES status_page_subscribers(id) ON DELETE CASCADE,
    channel         TEXT NOT NULL CHECK (channel IN ('email', 'webhook')),
    status          TEXT NOT NULL DEFAULT 'queued'
                    CHECK (status IN ('queued', 'sending', 'sent', 'failed', 'skipped', 'expired')),
    attempts        INT NOT NULL DEFAULT 0,
    next_attempt_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    lease_until     TIMESTAMPTZ,
    -- Who holds a `sending` row; every renewal and outcome write matches it.
    claim_token     UUID,
    -- Set while a stored batch holds the row.
    batch_key       TEXT,
    error           TEXT,
    sent_at         TIMESTAMPTZ,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (notice_id, subscriber_id)
);

CREATE INDEX idx_sp_deliveries_due ON status_page_subscriber_deliveries (channel, next_attempt_at)
    WHERE status = 'queued';
CREATE INDEX idx_sp_deliveries_held ON status_page_subscriber_deliveries (claim_token)
    WHERE status = 'sending';
CREATE INDEX idx_sp_deliveries_subscriber ON status_page_subscriber_deliveries (subscriber_id);

CREATE TRIGGER trg_sp_subscriber_deliveries_org_match
    BEFORE INSERT OR UPDATE OF subscriber_id, org_id ON status_page_subscriber_deliveries
    FOR EACH ROW EXECUTE FUNCTION assert_org_matches_parent('status_page_subscribers', 'subscriber_id');

CREATE TABLE status_page_delivery_batches (
    key          TEXT PRIMARY KEY,
    org_id       UUID NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    notice_id    UUID NOT NULL REFERENCES status_page_notices(id) ON DELETE CASCADE,
    request      BYTEA NOT NULL,
    -- Position i of the request is delivery_ids[i].
    delivery_ids UUID[] NOT NULL,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TRIGGER trg_sp_delivery_batches_org_match
    BEFORE INSERT OR UPDATE OF notice_id, org_id ON status_page_delivery_batches
    FOR EACH ROW EXECUTE FUNCTION assert_org_matches_parent('status_page_notices', 'notice_id');

-- The planner reads recent updates and recently ended monitor incidents across
-- every org.
CREATE INDEX idx_incident_updates_posted ON incident_updates (posted_at);
CREATE INDEX idx_incidents_monitor_ended ON incidents (ended_at)
    WHERE origin = 'monitor' AND ended_at IS NOT NULL;

-- What subscribers were already told is marked handled, so the first planner
-- pass sends nothing twice. Anything older than the old dispatcher's few
-- seconds of lag was settled by it, sent or deliberately not (no subscriber
-- yet, not public), and counts as told; only what was posted in the last two
-- minutes and never reached the old log is left for the planner. A superset of
-- the pages an event shows on is fine: a handled notice sends nothing.
CREATE TEMP TABLE settled ON COMMIT DROP AS SELECT now() - interval '2 minutes' AS before;

INSERT INTO status_page_notices (org_id, status_page_id, kind, source_ref, phase, channels,
                                 payload, fanned_out_at)
SELECT org_id, status_page_id, event_kind, event_ref, phase, ARRAY['email', 'webhook'],
       '{}'::jsonb, now()
  FROM told
UNION
SELECT u.org_id, p.status_page_id, 'incident_update', u.id, '', ARRAY['email', 'webhook'],
       '{}'::jsonb, now()
  FROM incident_updates u
  JOIN incidents i ON i.id = u.incident_id AND i.org_id = u.org_id
  JOIN LATERAL (
      SELECT c.status_page_id FROM status_page_components c
       WHERE c.org_id = i.org_id AND c.target_id = i.target_id
      UNION
      SELECT isp.status_page_id FROM incident_status_pages isp
       WHERE isp.org_id = i.org_id AND isp.incident_id = i.id
  ) p ON true
 WHERE u.posted_at >= now() - interval '24 hours'
   AND u.posted_at < (SELECT before FROM settled)
UNION
SELECT mw.org_id, p.status_page_id, 'maintenance', mw.id, ph.phase, ARRAY['email', 'webhook'],
       '{}'::jsonb, now()
  FROM maintenance_windows mw
  JOIN LATERAL (
      SELECT c.status_page_id FROM maintenance_window_components mwc
        JOIN status_page_components c ON c.org_id = mwc.org_id AND c.target_id = mwc.target_id
       WHERE mwc.org_id = mw.org_id AND mwc.maintenance_id = mw.id
      UNION
      SELECT c.status_page_id FROM maintenance_window_departures d
        JOIN status_page_components c ON c.org_id = d.org_id AND c.target_id = d.target_id
       WHERE d.org_id = mw.org_id AND d.maintenance_id = mw.id
      UNION
      SELECT kept.status_page_id FROM maintenance_status_pages kept
       WHERE kept.org_id = mw.org_id AND kept.maintenance_id = mw.id
  ) p ON true
  JOIN LATERAL (
      SELECT 'scheduled' AS phase
       WHERE mw.created_at >= now() - interval '24 hours'
         AND mw.created_at < (SELECT before FROM settled)
      UNION ALL
      SELECT 'completed'
       WHERE mw.ends_at >= now() - interval '24 hours'
         AND mw.ends_at < (SELECT before FROM settled)
  ) ph ON true
UNION
SELECT u.org_id, p.status_page_id, 'maintenance_update', u.id, '', ARRAY['email', 'webhook'],
       '{}'::jsonb, now()
  FROM maintenance_updates u
  JOIN LATERAL (
      SELECT c.status_page_id FROM maintenance_window_components mwc
        JOIN status_page_components c ON c.org_id = mwc.org_id AND c.target_id = mwc.target_id
       WHERE mwc.org_id = u.org_id AND mwc.maintenance_id = u.maintenance_id
      UNION
      SELECT c.status_page_id FROM maintenance_window_departures d
        JOIN status_page_components c ON c.org_id = d.org_id AND c.target_id = d.target_id
       WHERE d.org_id = u.org_id AND d.maintenance_id = u.maintenance_id
      UNION
      SELECT kept.status_page_id FROM maintenance_status_pages kept
       WHERE kept.org_id = u.org_id AND kept.maintenance_id = u.maintenance_id
  ) p ON true
 WHERE u.posted_at >= now() - interval '24 hours'
   AND u.posted_at < (SELECT before FROM settled)
ON CONFLICT (status_page_id, kind, source_ref, phase) DO NOTHING;

-- Monitor incidents subscribers already heard about, by the same rule. An
-- open one whose start was settled becomes its own outage with its start
-- told on every page that shows it, so its resolved mail still goes out; one
-- whose start is still in flight is left for the planner, which announces it.
-- One closed inside the lookback with nothing in flight becomes a resolved
-- outage with its resolved notice handled; otherwise the planner sends its
-- summary.
CREATE TEMP TABLE seed_outages ON COMMIT DROP AS
SELECT uuidv7() AS outage_id, i.org_id, c.status_page_id, i.id AS incident_id, i.target_id,
       COALESCE(NULLIF(c.public_name, ''), t.name) AS component_name,
       COALESCE((SELECT min(u.posted_at) FROM incident_updates u
                  WHERE u.incident_id = i.id AND u.org_id = i.org_id), i.created_at) AS opened_at,
       i.started_at, i.resolved_at, i.ended_at,
       NOT EXISTS (SELECT 1 FROM incident_updates u
                    WHERE u.incident_id = i.id AND u.org_id = i.org_id
                      AND u.posted_at >= (SELECT before FROM settled)
                      AND NOT EXISTS (SELECT 1 FROM told
                                       WHERE told.event_kind = 'incident_update'
                                         AND told.event_ref = u.id
                                         AND told.status_page_id = c.status_page_id)) AS nothing_in_flight
  FROM incidents i
  JOIN status_page_components c ON c.org_id = i.org_id AND c.target_id = i.target_id
  JOIN targets t ON t.id = i.target_id AND t.org_id = i.org_id
 WHERE i.origin = 'monitor'
   AND (i.ended_at IS NULL OR i.ended_at >= now() - interval '24 hours')
   AND (i.visibility = 'public'
        OR EXISTS (SELECT 1 FROM incident_events e
                    WHERE e.incident_id = i.id AND e.org_id = i.org_id AND e.kind = 'published'));

DELETE FROM seed_outages WHERE NOT nothing_in_flight;

INSERT INTO status_page_outages (id, org_id, status_page_id, opened_at, started_at, recovered_at,
                                 resolved_at, started_told)
SELECT outage_id, org_id, status_page_id, opened_at, started_at,
       CASE WHEN ended_at IS NULL THEN NULL ELSE COALESCE(resolved_at, ended_at) END,
       CASE WHEN ended_at IS NULL THEN NULL ELSE now() END,
       true
  FROM seed_outages;

INSERT INTO status_page_outage_members (outage_id, org_id, status_page_id, incident_id, target_id,
                                        component_name, ended_at, closed_at)
SELECT outage_id, org_id, status_page_id, incident_id, target_id, component_name, ended_at,
       CASE WHEN ended_at IS NULL THEN NULL ELSE COALESCE(resolved_at, ended_at) END
  FROM seed_outages;

INSERT INTO status_page_notices (org_id, status_page_id, kind, source_ref, phase, channels,
                                 payload, fanned_out_at)
SELECT org_id, status_page_id, 'outage_resolved', outage_id, '', ARRAY['email'], '{}'::jsonb, now()
  FROM seed_outages
 WHERE ended_at IS NOT NULL;
