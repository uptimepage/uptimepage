#!/usr/bin/env bash
# Seed a demo tenant (org + components + 90d history + a resolved incident)
# into a running local stack, so the per-org status page has content.
#
# Idempotent: demo rows are tagged `seed-demo` and wiped before re-insert,
# so re-running gives the same state without duplicates. Postgres data is
# fully reset each run; ClickHouse history is only reset when RESET_CH=1
# (ALTER ... DELETE is an async heavy mutation — on a throwaway stack it's
# cheaper to just `down -v` and start fresh).
#
# Env overrides:
#   SLUG          org slug to seed onto       (default: default)
#   PG_CONTAINER  postgres container name     (default: uptimepage-postgres-1)
#   CH_CONTAINER  clickhouse container name   (default: uptimepage-clickhouse-1)
#   BASE_DOMAIN   for the printed URL         (default: lvh.me)
#   RESET_CH      1 = clear prior runs' CH rows first (default: 0)
#
# Requires the stack already up. SaaS mode → page at
# {slug}.{BASE_DOMAIN}; self-host → {BASE_DOMAIN}/status.
set -euo pipefail

SLUG="${SLUG:-default}"
PG_CONTAINER="${PG_CONTAINER:-uptimepage-postgres-1}"
CH_CONTAINER="${CH_CONTAINER:-uptimepage-clickhouse-1}"
BASE_DOMAIN="${BASE_DOMAIN:-lvh.me}"
RESET_CH="${RESET_CH:-0}"

pg() { docker exec -i "$PG_CONTAINER" psql -U monitor -d monitor -v ON_ERROR_STOP=1 "$@"; }
ch() { docker exec -i "$CH_CONTAINER" clickhouse-client "$@"; }
source "$(dirname "${BASH_SOURCE[0]}")/lib/seed-purge.sh"

echo "==> Postgres: org '$SLUG' + components + incident"
pg <<SQL
UPDATE organizations
   SET name = 'Acme Inc',
       public_display_name = 'Acme Status',
       public_about = 'Live status of Acme services.',
       public_brand_color = '#6d28d9',
       public_status_enabled = true
 WHERE slug = '${SLUG}';

-- Wipe prior demo rows (incident_updates cascade via incident FK).
DELETE FROM incidents
 WHERE org_id = (SELECT id FROM organizations WHERE slug='${SLUG}')
   AND target_id IN (
     SELECT id FROM targets
      WHERE org_id = (SELECT id FROM organizations WHERE slug='${SLUG}')
        AND tags @> ARRAY['seed-demo']);
DELETE FROM targets
 WHERE org_id = (SELECT id FROM organizations WHERE slug='${SLUG}')
   AND tags @> ARRAY['seed-demo'];

WITH org AS (SELECT id FROM organizations WHERE slug='${SLUG}')
INSERT INTO targets
  (org_id,name,check_spec,interval_secs,enabled,tags,public_status,public_name,public_group,public_sort_order)
SELECT org.id, t.name, s.spec::jsonb, 60, true, ARRAY['seed-demo'], true, t.pname, t.grp, t.so
FROM org, (VALUES
  ('demo-api','API','Core Services',0),
  ('demo-web','Website','Core Services',1),
  ('demo-db','Database','Infrastructure',0)
) AS t(name,pname,grp,so)
CROSS JOIN LATERAL (SELECT '{"type":"http","url":"https://example.com/","method":"GET","timeout":5000,"follow_redirects":true,"max_redirects":3,"expected_status":{"kind":"exact","value":200},"headers":{},"verify_tls":true}' AS spec) s;

WITH org AS (SELECT id oid FROM organizations WHERE slug='${SLUG}'),
tgt AS (SELECT id tid FROM targets WHERE org_id=(SELECT oid FROM org) AND public_name='Database'),
ins AS (
  INSERT INTO incidents
    (org_id,target_id,started_at,ended_at,severity,status_at_start,check_count,error_sample,public_title,public_description,duration_secs)
  SELECT (SELECT oid FROM org),(SELECT tid FROM tgt),
         now()-interval '5 day', now()-interval '4 day',
         'major','down',40,'connection refused',
         'Database connectivity outage',
         'Primary database refused connections for ~24h. Failover restored service; no data loss.',
         86400
  RETURNING id, org_id
)
INSERT INTO incident_updates (org_id,incident_id,posted_at,phase,message)
SELECT i.org_id, i.id, v.posted_at, v.phase, v.message
FROM ins i, (VALUES
  ((now()-interval '5 day')                  ,'investigating','Investigating elevated database connection errors.'),
  ((now()-interval '5 day'+interval '20 min'),'identified'   ,'Root cause identified: primary DB node refusing connections.'),
  ((now()-interval '4 day'-interval '40 min'),'monitoring'   ,'Failover completed; monitoring recovery.'),
  ((now()-interval '4 day')                  ,'resolved'     ,'Service fully restored. Incident resolved.')
) AS v(posted_at,phase,message);
SQL

ORG=$(pg -Atc "SELECT id FROM organizations WHERE slug='${SLUG}';")
read -r API WEB DB < <(pg -Atc \
  "SELECT string_agg(id::text,' ' ORDER BY public_group,public_sort_order)
     FROM targets WHERE org_id='${ORG}' AND tags @> ARRAY['seed-demo'];")

if [ "$RESET_CH" = "1" ]; then
  echo "==> ClickHouse: wipe rows for the demo monitors and any prior run"
  reset_seeded_history "$ORG" seed-demo
fi

echo "==> ClickHouse: 90d history (API/Web all-up, DB 2d major outage)"
ch -mn <<SQL
INSERT INTO monitor.check_results (org_id,target_id,timestamp,status,duration_ms,response_code)
SELECT toUUID('${ORG}'),toUUID('${API}'),now()-toIntervalDay(number)-toIntervalMinute(number%17),'up',90+(number%40),200 FROM numbers(90);
INSERT INTO monitor.check_results (org_id,target_id,timestamp,status,duration_ms,response_code)
SELECT toUUID('${ORG}'),toUUID('${API}'),now()-toIntervalMinute(number*2),'up',95+(number%30),200 FROM numbers(12);
INSERT INTO monitor.check_results (org_id,target_id,timestamp,status,duration_ms,response_code)
SELECT toUUID('${ORG}'),toUUID('${WEB}'),now()-toIntervalDay(number)-toIntervalMinute(number%23),'up',110+(number%60),200 FROM numbers(90);
INSERT INTO monitor.check_results (org_id,target_id,timestamp,status,duration_ms,response_code)
SELECT toUUID('${ORG}'),toUUID('${WEB}'),now()-toIntervalMinute(number*2),'up',120+(number%25),200 FROM numbers(12);
INSERT INTO monitor.check_results (org_id,target_id,timestamp,status,duration_ms,response_code)
SELECT toUUID('${ORG}'),toUUID('${DB}'),now()-toIntervalDay(number)-toIntervalMinute(number%13),'up',60+(number%20),200 FROM numbers(90) WHERE number NOT IN (4,5);
INSERT INTO monitor.check_results (org_id,target_id,timestamp,status,duration_ms,response_code,error)
SELECT toUUID('${ORG}'),toUUID('${DB}'),now()-toIntervalDay(4)-toIntervalMinute(number*3),'down',0,503,'connection refused' FROM numbers(20);
INSERT INTO monitor.check_results (org_id,target_id,timestamp,status,duration_ms,response_code,error)
SELECT toUUID('${ORG}'),toUUID('${DB}'),now()-toIntervalDay(5)-toIntervalMinute(number*3),'down',0,503,'connection refused' FROM numbers(20);
INSERT INTO monitor.check_results (org_id,target_id,timestamp,status,duration_ms,response_code)
SELECT toUUID('${ORG}'),toUUID('${DB}'),now()-toIntervalMinute(number*2),'up',65+(number%15),200 FROM numbers(12);
SQL

echo
echo "Seeded org '${SLUG}' (id ${ORG})."
echo "SaaS:      https://${SLUG}.${BASE_DOMAIN}/"
echo "Self-host: https://app.${BASE_DOMAIN}/status"
echo "(page cache TTL ~10s; wait a moment before the first load)"
