SET LOCAL lock_timeout = '5s';

-- When a monitor-opened incident's monitor was paused. Nobody checked it then,
-- so none of it is downtime, though the incident stays open across the pause.
-- `paused_since` is the pause still running; a resume moves it into the
-- paired arrays.
ALTER TABLE incidents
    ADD COLUMN IF NOT EXISTS paused_since TIMESTAMPTZ,
    ADD COLUMN IF NOT EXISTS paused_from TIMESTAMPTZ[] NOT NULL DEFAULT '{}',
    ADD COLUMN IF NOT EXISTS paused_until TIMESTAMPTZ[] NOT NULL DEFAULT '{}';

-- Incidents on a monitor paused now date from the audit row of the pause still
-- running: one after the incident began and after the monitor's last resume.
-- An open one without that row (purged, or a bulk pause too large to name
-- every monitor) dates from the monitor's last edit, which a pause also
-- stamps; that can only date the pause late, never early. One resolved since
-- counts only a pause logged before it closed.
WITH running AS (
    SELECT i.id, GREATEST(i.started_at, COALESCE(p.at, t.updated_at)) AS since
      FROM incidents i
      JOIN targets t ON t.id = i.target_id AND t.org_id = i.org_id
     CROSS JOIN LATERAL (
          SELECT max(a.occurred_at) AS at FROM org_audit_log a
           WHERE a.org_id = t.org_id AND a.action = 'target.paused'
             AND a.metadata->'targets' @> jsonb_build_array(jsonb_build_object('id', t.id))
             AND a.occurred_at >= i.started_at
             AND a.occurred_at > COALESCE(
                 (SELECT max(r.occurred_at) FROM org_audit_log r
                   WHERE r.org_id = t.org_id AND r.action = 'target.resumed'
                     AND r.metadata->'targets' @> jsonb_build_array(jsonb_build_object('id', t.id))),
                 '-infinity')) p
     WHERE NOT t.enabled AND i.origin = 'monitor'
       AND (i.ended_at IS NULL OR p.at < i.ended_at)
)
UPDATE incidents i SET paused_since = r.since FROM running r WHERE i.id = r.id;

-- Pauses already resumed inside an incident, open or closed: each pause in the
-- audit log after the incident began, until the monitor's next resume logged
-- before its next pause. A pause with no such resume is either the one
-- running, dated above, or one whose resume the log lost, which stays
-- downtime.
UPDATE incidents i
   SET paused_from = s.froms, paused_until = s.untils
  FROM (
      SELECT x.id,
             array_agg(p.occurred_at ORDER BY p.occurred_at) AS froms,
             array_agg(r.resumed_at ORDER BY p.occurred_at) AS untils
        FROM incidents x
        JOIN org_audit_log p
          ON p.org_id = x.org_id AND p.action = 'target.paused'
         AND p.metadata->'targets' @> jsonb_build_array(jsonb_build_object('id', x.target_id))
         AND p.occurred_at >= x.started_at
         AND p.occurred_at < COALESCE(x.ended_at, 'infinity')
       CROSS JOIN LATERAL (
            SELECT min(a.occurred_at) AS resumed_at FROM org_audit_log a
             WHERE a.org_id = x.org_id AND a.action = 'target.resumed'
               AND a.metadata->'targets' @> jsonb_build_array(jsonb_build_object('id', x.target_id))
               AND a.occurred_at > p.occurred_at
               AND a.occurred_at < COALESCE(
                   (SELECT min(n.occurred_at) FROM org_audit_log n
                     WHERE n.org_id = x.org_id AND n.action = 'target.paused'
                       AND n.metadata->'targets' @> jsonb_build_array(jsonb_build_object('id', x.target_id))
                       AND n.occurred_at > p.occurred_at),
                   'infinity')) r
       WHERE x.origin = 'monitor' AND x.target_id IS NOT NULL AND r.resumed_at IS NOT NULL
       GROUP BY x.id) s
 WHERE i.id = s.id;
