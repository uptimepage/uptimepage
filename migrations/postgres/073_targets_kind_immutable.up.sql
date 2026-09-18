-- Fail the boot rather than queue every monitor write behind the trigger install.
SET LOCAL lock_timeout = '5s';

-- A monitor keeps its id across an edit, so a check of another kind would
-- pile a second kind's results onto the first one's history. The API refuses
-- it first; this holds the same line for every other writer. `kind` is
-- generated from check_spec and not yet recomputed in a BEFORE trigger, so
-- the incoming spec's own tag is what gets compared.
CREATE OR REPLACE FUNCTION reject_target_kind_change() RETURNS TRIGGER AS $$
BEGIN
    IF NEW.check_spec->>'type' IS DISTINCT FROM OLD.kind THEN
        RAISE EXCEPTION 'targets.kind is immutable (attempted % -> %)',
            OLD.kind, NEW.check_spec->>'type'
            USING ERRCODE = '23514', CONSTRAINT = 'targets_kind_immutable';
    END IF;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql
SET search_path = pg_catalog, public;

CREATE TRIGGER trg_targets_kind_immutable
    BEFORE UPDATE OF check_spec ON targets
    FOR EACH ROW EXECUTE FUNCTION reject_target_kind_change();
