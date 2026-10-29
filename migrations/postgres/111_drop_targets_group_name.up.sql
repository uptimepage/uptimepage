SET LOCAL lock_timeout = '5s';

-- Groups have lived in monitor_groups since 110; the frozen label was kept
-- only for builds older than that. Its index goes with the column.
DROP TRIGGER trg_targets_group_name_frozen ON targets;
DROP FUNCTION targets_group_name_frozen();
ALTER TABLE targets DROP COLUMN group_name;
