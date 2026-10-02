-- The state an operator last set on a manual-kind target. No row means it has
-- never been set and reads as up since the target was created.
CREATE TABLE manual_monitors (
    target_id UUID PRIMARY KEY REFERENCES targets(id) ON DELETE CASCADE,
    org_id    UUID NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    status    TEXT NOT NULL CHECK (status IN ('up', 'degraded', 'down')),
    note      TEXT CHECK (char_length(note) <= 200),
    set_at    TIMESTAMPTZ NOT NULL,
    set_by    UUID REFERENCES users(id) ON DELETE SET NULL
);

CREATE TRIGGER trg_manual_monitors_target_org
    BEFORE INSERT OR UPDATE OF target_id, org_id ON manual_monitors
    FOR EACH ROW EXECUTE FUNCTION assert_target_in_same_org();

-- Org deletion cascades through org_id.
CREATE INDEX idx_manual_monitors_org ON manual_monitors(org_id);

-- The scheduler lists manual targets on every refresh.
CREATE INDEX idx_targets_manual ON targets(id) WHERE kind = 'manual';
