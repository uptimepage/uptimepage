-- Everyone who acknowledged an incident, once per person per episode. The
-- episode is the incident's reopen count when they acknowledged, so a reopen
-- starts an empty list and the earlier ones stay as history. The first row of
-- an episode holds the credit mirrored on incidents.acknowledged_by.
CREATE TABLE incident_acknowledgements (
    id              UUID PRIMARY KEY DEFAULT uuidv7(),
    org_id          UUID NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    incident_id     UUID NOT NULL REFERENCES incidents(id) ON DELETE CASCADE,
    episode         BIGINT NOT NULL CHECK (episode >= 0),
    actor_type      TEXT NOT NULL CONSTRAINT incident_acknowledgements_actor_type_check
                        CHECK (actor_type IN ('system','user','mcp','link')),
    actor_id        UUID REFERENCES users(id) ON DELETE SET NULL,
    -- Named nobody when it landed. Fixed at insert: a deleted account nulls
    -- actor_id later, and its row must not turn into an anonymous one.
    anonymous       BOOLEAN NOT NULL CHECK (NOT anonymous OR actor_id IS NULL),
    -- Taken at insert, under the incident lock, so times run in the order
    -- the acknowledgements landed rather than when each transaction began.
    acknowledged_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp()
);

-- One row per member per episode, whether they acted on the web or through
-- MCP. One that named nobody counts once per kind. A deleted account's row
-- loses its id but was never anonymous, so it falls outside both and the
-- deletion never collides with anything.
CREATE UNIQUE INDEX uq_incident_acknowledgements_member
    ON incident_acknowledgements (incident_id, episode, actor_id)
    WHERE actor_id IS NOT NULL;
CREATE UNIQUE INDEX uq_incident_acknowledgements_anonymous
    ON incident_acknowledgements (incident_id, episode, actor_type)
    WHERE anonymous;

-- Serves the per-episode reads and the cascade from a deleted incident.
CREATE INDEX idx_incident_acknowledgements_incident
    ON incident_acknowledgements (incident_id, episode);
-- Serves the SET NULL when an account is deleted.
CREATE INDEX idx_incident_acknowledgements_actor
    ON incident_acknowledgements (actor_id)
    WHERE actor_id IS NOT NULL;

CREATE TRIGGER trg_incident_acknowledgements_org_match
    BEFORE INSERT OR UPDATE OF incident_id, org_id ON incident_acknowledgements
    FOR EACH ROW EXECUTE FUNCTION assert_org_matches_parent('incidents', 'incident_id');
