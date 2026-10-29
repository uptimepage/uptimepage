SET LOCAL lock_timeout = '5s';

-- Back to the shape after 110: the label rebuilt from each monitor's current
-- group, then frozen again. The guard comes last; it refuses the rebuild.
ALTER TABLE targets ADD COLUMN group_name TEXT;

UPDATE targets t
   SET group_name = g.name
  FROM monitor_groups g
 WHERE g.id = t.group_id;

CREATE INDEX idx_targets_org_group
    ON targets(org_id, group_name) WHERE group_name IS NOT NULL;

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
