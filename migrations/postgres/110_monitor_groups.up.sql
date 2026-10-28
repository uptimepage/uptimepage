SET LOCAL lock_timeout = '5s';

-- Rust's str::trim set (Unicode White_Space), so a name trimmed by either side
-- is the same name. Zero-width characters are not White_Space and stay.
CREATE FUNCTION group_name_trim(name TEXT) RETURNS TEXT
    LANGUAGE sql IMMUTABLE STRICT PARALLEL SAFE
    RETURN btrim(name, E' \t\n\u000B\f\r\u0085                 　');

CREATE TABLE monitor_groups (
    id                     UUID PRIMARY KEY DEFAULT uuidv7(),
    org_id                 UUID NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    name                   TEXT NOT NULL CHECK (char_length(name) BETWEEN 1 AND 50 AND name = group_name_trim(name)),
    alert_as_one           BOOLEAN NOT NULL DEFAULT false,
    renotify_interval_secs INTEGER NOT NULL DEFAULT 3600 CHECK (renotify_interval_secs >= 0),
    alert_again            BOOLEAN NOT NULL DEFAULT true,
    tags                   TEXT[] NOT NULL DEFAULT '{}',
    escalation_policy_id   UUID REFERENCES escalation_policies(id) ON DELETE SET NULL,
    write_source           TEXT NOT NULL DEFAULT 'ui' CHECK (write_source IN ('ui', 'api', 'terraform')),
    created_at             TIMESTAMPTZ NOT NULL DEFAULT now(),
    created_by             UUID REFERENCES users(id) ON DELETE SET NULL,
    updated_at             TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_by             UUID REFERENCES users(id) ON DELETE SET NULL,
    UNIQUE (org_id, name)
);

-- assert_org_matches_parent rejects a NULL reference, so triggers on nullable
-- links are guarded and an ON DELETE SET NULL passes through.
CREATE TRIGGER trg_monitor_groups_policy_org_match
    BEFORE INSERT OR UPDATE OF escalation_policy_id, org_id ON monitor_groups
    FOR EACH ROW WHEN (NEW.escalation_policy_id IS NOT NULL)
    EXECUTE FUNCTION assert_org_matches_parent('escalation_policies', 'escalation_policy_id');
CREATE INDEX idx_monitor_groups_policy ON monitor_groups (escalation_policy_id) WHERE escalation_policy_id IS NOT NULL;
CREATE INDEX idx_monitor_groups_created_by ON monitor_groups (created_by) WHERE created_by IS NOT NULL;
CREATE INDEX idx_monitor_groups_updated_by ON monitor_groups (updated_by) WHERE updated_by IS NOT NULL;

CREATE TABLE group_channels (
    group_id   UUID NOT NULL REFERENCES monitor_groups(id) ON DELETE CASCADE,
    channel_id UUID NOT NULL REFERENCES notification_channels(id) ON DELETE CASCADE,
    org_id     UUID NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    position   INT  NOT NULL,
    PRIMARY KEY (group_id, channel_id)
);
CREATE INDEX group_channels_channel_idx ON group_channels (channel_id);
CREATE TRIGGER trg_group_channels_group_org_match
    BEFORE INSERT OR UPDATE OF group_id, org_id ON group_channels
    FOR EACH ROW EXECUTE FUNCTION assert_org_matches_parent('monitor_groups', 'group_id');
CREATE TRIGGER trg_group_channels_channel_org_match
    BEFORE INSERT OR UPDATE OF channel_id, org_id ON group_channels
    FOR EACH ROW EXECUTE FUNCTION assert_org_matches_parent('notification_channels', 'channel_id');

-- The API took "" and untrimmed names, the group list skipped "", and a bulk
-- move took any length: one group per trimmed name cut to the 50 a group
-- holds, and a blank one is no group.
ALTER TABLE targets ADD COLUMN group_id UUID REFERENCES monitor_groups(id) ON DELETE SET NULL;
UPDATE targets SET group_name = NULLIF(group_name_trim(left(group_name_trim(group_name), 50)), '')
    WHERE group_name IS NOT NULL
      AND group_name IS DISTINCT FROM NULLIF(group_name_trim(left(group_name_trim(group_name), 50)), '');
INSERT INTO monitor_groups (org_id, name, created_at, updated_at)
    SELECT org_id, group_name, min(created_at), min(created_at)
    FROM targets WHERE group_name IS NOT NULL GROUP BY org_id, group_name;
UPDATE targets t SET group_id = g.id
    FROM monitor_groups g WHERE g.org_id = t.org_id AND g.name = t.group_name;

-- Kept for builds that still read it, frozen at the copy above: a write to it
-- would land where nothing reads, so it is refused instead.
CREATE FUNCTION targets_group_name_frozen() RETURNS TRIGGER AS $$
BEGIN
    IF (TG_OP = 'INSERT' AND NEW.group_name IS NOT NULL)
       OR (TG_OP = 'UPDATE' AND NEW.group_name IS DISTINCT FROM OLD.group_name) THEN
        RAISE EXCEPTION 'targets.group_name is read-only: groups live in monitor_groups';
    END IF;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql
SET search_path = pg_catalog, public;
CREATE TRIGGER trg_targets_group_name_frozen
    BEFORE INSERT OR UPDATE OF group_name ON targets
    FOR EACH ROW EXECUTE FUNCTION targets_group_name_frozen();

CREATE TRIGGER trg_targets_group_org_match
    BEFORE INSERT OR UPDATE OF group_id, org_id ON targets
    FOR EACH ROW WHEN (NEW.group_id IS NOT NULL)
    EXECUTE FUNCTION assert_org_matches_parent('monitor_groups', 'group_id');
CREATE INDEX idx_targets_group ON targets (org_id, group_id) WHERE group_id IS NOT NULL;

ALTER TABLE incidents DROP CONSTRAINT incidents_origin_check;
ALTER TABLE incidents ADD CONSTRAINT incidents_origin_check
    CHECK (origin IN ('monitor', 'manual', 'group'));

ALTER TABLE incidents DROP CONSTRAINT incident_monitor_named;
ALTER TABLE incidents ADD CONSTRAINT incident_monitor_named CHECK (
    (target_id IS NULL OR (target_ref = target_id AND target_name IS NOT NULL))
    AND ((target_ref IS NULL) = (target_name IS NULL))
    AND (origin IN ('manual', 'group') OR target_ref IS NOT NULL)
);

ALTER TABLE incidents
    ADD COLUMN group_id               UUID REFERENCES monitor_groups(id) ON DELETE SET NULL,
    ADD COLUMN group_ref              UUID,
    ADD COLUMN group_name             TEXT,
    ADD COLUMN routed_group_id        UUID REFERENCES monitor_groups(id) ON DELETE SET NULL,
    ADD COLUMN group_incident_id      UUID REFERENCES incidents(id) ON DELETE SET NULL,
    ADD COLUMN closed_by_group_delete BOOLEAN NOT NULL DEFAULT false,
    ADD COLUMN detached_at            TIMESTAMPTZ,
    ADD COLUMN member_seq             INTEGER NOT NULL DEFAULT 0,
    ADD COLUMN member_notice_seq      INTEGER NOT NULL DEFAULT 0,
    ADD COLUMN last_member_notice_at  TIMESTAMPTZ,
    ADD COLUMN group_member_seq       INTEGER,
    ADD CONSTRAINT incident_member_notice_bounded CHECK (member_notice_seq BETWEEN 0 AND member_seq),
    ADD CONSTRAINT incident_not_its_own_group CHECK (group_incident_id IS DISTINCT FROM id),
    ADD CONSTRAINT incident_member_is_monitor CHECK (
        (routed_group_id IS NULL AND group_incident_id IS NULL) OR origin = 'monitor'),
    -- The group survives on a group incident as group_ref and group_name after
    -- the group is deleted, as target_ref does for a monitor.
    ADD CONSTRAINT incident_group_named CHECK (
        (group_id IS NULL OR group_ref = group_id)
        AND ((group_ref IS NULL) = (group_name IS NULL))
        AND ((origin = 'group') = (group_ref IS NOT NULL))
        AND (origin <> 'group' OR target_id IS NULL));

-- FOR SHARE waits out a rename in flight, whose own update cannot see this row.
CREATE FUNCTION incidents_snapshot_group() RETURNS TRIGGER AS $$
DECLARE
    group_org UUID;
BEGIN
    IF NEW.group_id IS NULL THEN
        RETURN NEW;
    END IF;
    SELECT org_id, name INTO group_org, NEW.group_name
    FROM monitor_groups WHERE id = NEW.group_id FOR SHARE;
    IF group_org IS NULL OR group_org <> NEW.org_id THEN
        RAISE EXCEPTION 'group_id % belongs to org % but the incident is under org %',
            NEW.group_id, group_org, NEW.org_id
            USING ERRCODE = '23503', CONSTRAINT = 'incidents_group_id_fkey';
    END IF;
    NEW.group_ref := NEW.group_id;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql
SET search_path = pg_catalog, public;
CREATE TRIGGER trg_incidents_snapshot_group
    BEFORE INSERT OR UPDATE OF group_id, org_id ON incidents
    FOR EACH ROW EXECUTE FUNCTION incidents_snapshot_group();

CREATE FUNCTION monitor_groups_rename_incidents() RETURNS TRIGGER AS $$
BEGIN
    UPDATE incidents SET group_name = NEW.name
    WHERE org_id = NEW.org_id AND group_id = NEW.id;
    RETURN NULL;
END;
$$ LANGUAGE plpgsql
SET search_path = pg_catalog, public;
CREATE TRIGGER trg_monitor_groups_rename_incidents
    AFTER UPDATE OF name ON monitor_groups
    FOR EACH ROW WHEN (OLD.name IS DISTINCT FROM NEW.name)
    EXECUTE FUNCTION monitor_groups_rename_incidents();

CREATE TRIGGER trg_incidents_routed_group_org_match
    BEFORE INSERT OR UPDATE OF routed_group_id, org_id ON incidents
    FOR EACH ROW WHEN (NEW.routed_group_id IS NOT NULL)
    EXECUTE FUNCTION assert_org_matches_parent('monitor_groups', 'routed_group_id');

CREATE FUNCTION incidents_member_points_at_group() RETURNS TRIGGER AS $$
DECLARE
    parent_org UUID;
    parent_origin TEXT;
BEGIN
    SELECT org_id, origin INTO parent_org, parent_origin
    FROM incidents WHERE id = NEW.group_incident_id;
    IF parent_org IS NULL OR parent_org <> NEW.org_id OR parent_origin <> 'group' THEN
        RAISE EXCEPTION 'incident % cannot join % (org %, origin %)',
            NEW.id, NEW.group_incident_id, parent_org, parent_origin;
    END IF;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql
SET search_path = pg_catalog, public;
CREATE TRIGGER trg_incidents_member_points_at_group
    BEFORE INSERT OR UPDATE OF group_incident_id, org_id ON incidents
    FOR EACH ROW WHEN (NEW.group_incident_id IS NOT NULL)
    EXECUTE FUNCTION incidents_member_points_at_group();

CREATE UNIQUE INDEX idx_incidents_group_open
    ON incidents (org_id, group_id) WHERE ended_at IS NULL AND origin = 'group';
CREATE INDEX idx_incidents_group ON incidents (org_id, group_id) WHERE group_id IS NOT NULL;
CREATE INDEX idx_incidents_routed_group ON incidents (org_id, routed_group_id) WHERE routed_group_id IS NOT NULL;
CREATE INDEX idx_incidents_group_members ON incidents (group_incident_id) WHERE group_incident_id IS NOT NULL;
CREATE UNIQUE INDEX idx_incidents_group_member_seq
    ON incidents (group_incident_id, group_member_seq) WHERE group_incident_id IS NOT NULL;

-- Constraint keeps its generated name; the drift test looks it up by name.
ALTER TABLE incident_events DROP CONSTRAINT incident_events_kind_check;
ALTER TABLE incident_events ADD CONSTRAINT incident_events_kind_check
    CHECK (kind IN (
      'triggered','acknowledged','assigned','unassigned',
      'escalated','notified','note','severity_changed','downtime_changed',
      'state_changed','resolved','reopened','published','unpublished',
      'postmortem_published','postmortem_unpublished','monitor_deleted',
      'member_down','member_up','group_deleted','detached'
    ));

ALTER TABLE incident_notifications
    DROP CONSTRAINT incident_notifications_reason_check,
    ADD CONSTRAINT incident_notifications_reason_check
        CHECK (reason IN ('opened','escalated','resolved','reopened','no_data','data_resumed',
                          'reminder','monitor_deleted','member_down','group_deleted')),
    ADD COLUMN member_from_seq     INTEGER,
    ADD COLUMN member_to_seq       INTEGER,
    ADD COLUMN member_incident_ids UUID[],
    ADD CONSTRAINT incident_notifications_member_range CHECK (
        (member_from_seq IS NULL) = (member_to_seq IS NULL)
        AND (member_from_seq IS NULL OR member_from_seq BETWEEN 1 AND member_to_seq));
-- A replayed digest inserts nothing.
CREATE UNIQUE INDEX idx_incident_notifications_member_range
    ON incident_notifications (incident_id, reason, member_to_seq, channel_id)
    WHERE member_to_seq IS NOT NULL;

CREATE TABLE status_page_sections (
    id             UUID PRIMARY KEY DEFAULT uuidv7(),
    org_id         UUID NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    status_page_id UUID NOT NULL REFERENCES status_pages(id) ON DELETE CASCADE,
    group_id       UUID NOT NULL REFERENCES monitor_groups(id) ON DELETE CASCADE,
    heading        TEXT CHECK (heading IS NULL OR char_length(heading) BETWEEN 1 AND 50),
    created_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    created_by     UUID REFERENCES users(id) ON DELETE SET NULL,
    updated_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_by     UUID REFERENCES users(id) ON DELETE SET NULL,
    UNIQUE (status_page_id, group_id)
);
CREATE TRIGGER trg_status_page_sections_page_org_match
    BEFORE INSERT OR UPDATE OF status_page_id, org_id ON status_page_sections
    FOR EACH ROW EXECUTE FUNCTION assert_org_matches_parent('status_pages', 'status_page_id');
CREATE TRIGGER trg_status_page_sections_group_org_match
    BEFORE INSERT OR UPDATE OF group_id, org_id ON status_page_sections
    FOR EACH ROW EXECUTE FUNCTION assert_org_matches_parent('monitor_groups', 'group_id');
CREATE INDEX idx_status_page_sections_group ON status_page_sections (org_id, group_id);
CREATE INDEX idx_status_page_sections_created_by ON status_page_sections (created_by) WHERE created_by IS NOT NULL;
CREATE INDEX idx_status_page_sections_updated_by ON status_page_sections (updated_by) WHERE updated_by IS NOT NULL;

-- One "monitoring stopped" notice per group that alerts as one, whatever number
-- of its members go silent; its id keys the notice the way a monitor's id keys
-- its own. Members keep their own rows, so one coming back and going silent
-- again does not lose the episode it was in.
CREATE TABLE group_silence_episodes (
    id                   UUID PRIMARY KEY DEFAULT uuidv7(),
    org_id               UUID NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    group_id             UUID REFERENCES monitor_groups(id) ON DELETE SET NULL,
    group_name           TEXT NOT NULL,
    channel_ids          UUID[] NOT NULL DEFAULT '{}',
    started_at           TIMESTAMPTZ NOT NULL DEFAULT now(),
    ended_at             TIMESTAMPTZ,
    notified_at          TIMESTAMPTZ,
    resolved_notified_at TIMESTAMPTZ
);
CREATE TRIGGER trg_group_silence_episodes_group_org_match
    BEFORE INSERT OR UPDATE OF group_id, org_id ON group_silence_episodes
    FOR EACH ROW WHEN (NEW.group_id IS NOT NULL)
    EXECUTE FUNCTION assert_org_matches_parent('monitor_groups', 'group_id');
CREATE UNIQUE INDEX idx_group_silence_episodes_open
    ON group_silence_episodes (group_id) WHERE ended_at IS NULL;
CREATE INDEX idx_group_silence_episodes_unended
    ON group_silence_episodes (org_id) WHERE ended_at IS NULL;
CREATE INDEX idx_group_silence_episodes_group
    ON group_silence_episodes (group_id) WHERE group_id IS NOT NULL;

CREATE TABLE group_silence_members (
    id           UUID PRIMARY KEY DEFAULT uuidv7(),
    episode_id   UUID NOT NULL REFERENCES group_silence_episodes(id) ON DELETE CASCADE,
    org_id       UUID NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    target_id    UUID REFERENCES targets(id) ON DELETE SET NULL,
    target_name  TEXT NOT NULL,
    silent_since TIMESTAMPTZ NOT NULL,
    resumed_at   TIMESTAMPTZ
);
CREATE TRIGGER trg_group_silence_members_episode_org_match
    BEFORE INSERT OR UPDATE OF episode_id, org_id ON group_silence_members
    FOR EACH ROW EXECUTE FUNCTION assert_org_matches_parent('group_silence_episodes', 'episode_id');
CREATE TRIGGER trg_group_silence_members_target_org_match
    BEFORE INSERT OR UPDATE OF target_id, org_id ON group_silence_members
    FOR EACH ROW WHEN (NEW.target_id IS NOT NULL)
    EXECUTE FUNCTION assert_org_matches_parent('targets', 'target_id');
CREATE UNIQUE INDEX idx_group_silence_members_open
    ON group_silence_members (target_id) WHERE resumed_at IS NULL;
CREATE INDEX idx_group_silence_members_episode
    ON group_silence_members (episode_id) WHERE resumed_at IS NULL;
CREATE INDEX idx_group_silence_members_target
    ON group_silence_members (target_id) WHERE target_id IS NOT NULL;
