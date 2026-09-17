-- A heartbeat's interval is how often its silence is judged. Rows written by
-- the first heartbeat form carry the cadence rail's hidden value, hours apart,
-- so a missed ping was noticed at the next restart rather than the next tick.
-- Lower each to the cadence its window calls for: a tenth of period + grace,
-- between a minute and five, never under the plan's own floor.
UPDATE targets t
   SET interval_secs = c.cadence
  FROM (
       SELECT t.id,
              GREATEST(
                  LEAST(300, GREATEST(60, CEIL(
                      ((t.check_spec->>'period')::bigint
                       + COALESCE((t.check_spec->>'grace')::bigint, 0)) / 10000.0
                  ))),
                  COALESCE(p.min_check_interval_secs, 60)
              )::integer AS cadence
         FROM targets t
         JOIN organizations o ON o.id = t.org_id
         LEFT JOIN accounts a ON a.id = o.account_id
         LEFT JOIN plans p ON p.id = a.plan_id
        WHERE t.kind = 'heartbeat'
  ) c
 WHERE t.id = c.id
   AND t.interval_secs > c.cadence;
