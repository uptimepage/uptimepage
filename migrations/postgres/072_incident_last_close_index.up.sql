-- Fail the boot rather than queue every reader behind the index build.
SET LOCAL lock_timeout = '5s';

-- The incident writer asks, per monitor, when its last monitor-origin
-- incident closed, so it can ignore results older than that resolution.
CREATE INDEX idx_incidents_last_close
    ON incidents (org_id, target_id, ended_at DESC)
    WHERE origin = 'monitor' AND ended_at IS NOT NULL;
