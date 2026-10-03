#!/usr/bin/env bash
# Reproduce the monitor that made a customer delete their whole inventory: a
# host that drops roughly one probe in five from every region, in single
# checks that never confirm an incident.
#
#   flap-blocked   ~20% failures, isolated, all regions   → ~80% uptime,
#                                                           empty incidents tab,
#                                                           high flap count
#   flap-clean     same cadence, every check up           → the control row
#
# Failures are deliberately one check wide and phase-shifted per region, so
# neither gate that opens an incident is ever met: no region reaches
# `alert_confirmations` failures in a row, and no two regions fail together.
# That combination is the whole point — the monitor looks broken and pages
# nobody, which is what the product previously refused to explain.
#
# Env overrides:
#   SLUG          org slug to seed onto    (default: devorg)
#   PG_CONTAINER  postgres container name  (default: uptimepage-postgres-1)
#   CH_CONTAINER  clickhouse container     (default: uptimepage-clickhouse-1)
#   HOURS         history depth in hours   (default: 24)
#   FAIL_EVERY    1 in N checks fails      (default: 5)
#
# Idempotent: both monitors are tagged `seed-flap` and their Postgres rows and
# ClickHouse history are wiped before re-insert.
#
# Requires `just up-app` + `just dev-login` first.
set -euo pipefail

SLUG="${SLUG:-devorg}"
PG_CONTAINER="${PG_CONTAINER:-uptimepage-postgres-1}"
CH_CONTAINER="${CH_CONTAINER:-uptimepage-clickhouse-1}"
HOURS="${HOURS:-24}"
FAIL_EVERY="${FAIL_EVERY:-5}"

pg() { docker exec -i "$PG_CONTAINER" psql -U monitor -d monitor -v ON_ERROR_STOP=1 "$@"; }
ch() { docker exec -i "$CH_CONTAINER" clickhouse-client "$@"; }
source "$(dirname "${BASH_SOURCE[0]}")/lib/seed-purge.sh"

# A slug is only unique among live orgs, so a soft-deleted one can share it.
ORG=$(pg -tAc "SELECT id FROM organizations WHERE slug='${SLUG}' AND deleted_at IS NULL")
if [[ -z "$ORG" ]]; then
  echo "error: org '${SLUG}' missing — run 'just dev-login' (or SLUG=… just dev-login) first" >&2
  exit 1
fi

# A live agent probes these hosts for real and writes its own results over the
# pattern, which erases the shape this seed exists to produce.
if docker ps --format '{{.Names}}' | grep -q '^uptimepage-agent-'; then
  echo "error: dev-region agents are running — they will probe these monitors and" >&2
  echo "       overwrite the seeded history. Stop them first:" >&2
  echo "         just dev-regions-down" >&2
  exit 1
fi

mapfile -t REGIONS < <(pg -tAc "SELECT id FROM regions WHERE enabled ORDER BY id LIMIT 3")
if (( ${#REGIONS[@]} == 0 )); then
  echo "error: no enabled regions — bring the stack up first" >&2
  exit 1
fi
echo "    regions: ${REGIONS[*]}"
REGION_LIST=$(printf "'%s'," "${REGIONS[@]}"); REGION_LIST="${REGION_LIST%,}"

echo "==> Postgres: (re)create the flapping monitor and its control"
pg <<SQL
DELETE FROM targets WHERE org_id = '${ORG}' AND tags @> ARRAY['seed-flap'];

INSERT INTO targets (org_id, name, check_spec, interval_secs, enabled, tags, group_name,
                     alert_confirmations, region_policy)
VALUES
  ('${ORG}', 'Blocked API (seeded)',
   '{"type":"http","url":"https://blocked-api.example.com/health","method":"GET",
     "headers":{},"body":null,"timeout":10000,"follow_redirects":false,"max_redirects":0,
     "expected_status":{"kind":"exact","value":200},"expected_body_contains":null,
     "verify_tls":true,"basic_auth":null,"bearer_token":null}'::jsonb,
   60, true, ARRAY['seed-flap','flap-blocked'], 'Seeded', 2, '"majority"'::jsonb),
  ('${ORG}', 'Healthy twin (seeded)',
   '{"type":"http","url":"https://healthy-twin.example.com/health","method":"GET",
     "headers":{},"body":null,"timeout":10000,"follow_redirects":false,"max_redirects":0,
     "expected_status":{"kind":"exact","value":200},"expected_body_contains":null,
     "verify_tls":true,"basic_auth":null,"bearer_token":null}'::jsonb,
   60, true, ARRAY['seed-flap','flap-clean'], 'Seeded', 2, '"majority"'::jsonb);

-- Only the regions this seed writes history for: a monitor assigned to a
-- region with no data reads as a coverage gap, which is a different story.
INSERT INTO target_regions (target_id, region)
SELECT t.id, r.id FROM targets t CROSS JOIN regions r
 WHERE t.org_id = '${ORG}' AND t.tags @> ARRAY['seed-flap'] AND r.enabled
   AND r.id IN (${REGION_LIST})
ON CONFLICT DO NOTHING;
SQL

# A missing check_spec field does not fail here, it 500s the monitor list later:
# one undecodable row fails the whole page. Compare against a spec the app wrote.
MISSING=$(pg -tAc "
WITH good AS (
  SELECT jsonb_object_keys(check_spec) AS k FROM targets
   WHERE check_spec->>'type' = 'http' AND check_spec ? 'follow_redirects'
     AND NOT tags @> ARRAY['seed-flap'] LIMIT 1
), seeded AS (
  SELECT id, check_spec FROM targets WHERE org_id = '${ORG}' AND tags @> ARRAY['seed-flap']
)
SELECT string_agg(DISTINCT g.k, ', ')
  FROM seeded s CROSS JOIN good g WHERE NOT (s.check_spec ? g.k)")
if [[ -n "$MISSING" ]]; then
  echo "error: seeded check_spec is missing field(s): ${MISSING}" >&2
  echo "       the monitor list will 500 on them — add the field(s) to this script" >&2
  exit 1
fi

id_of() { pg -tAc "SELECT id FROM targets WHERE org_id='${ORG}' AND tags @> ARRAY['$1']"; }
BLOCKED=$(id_of flap-blocked)
CLEAN=$(id_of flap-clean)

echo "==> ClickHouse: wipe rows for these monitors and for any prior run"
reset_seeded_history "$ORG" seed-flap

SAMPLES=$(( HOURS * 60 ))

# `number` is minutes-ago. The phase shift keeps two regions from failing on the
# same minute, which would reach quorum and open an incident.
phase=0
for region in "${REGIONS[@]}"; do
  # Mirrors what the real case produced: a timeout from most paths, and an
  # outright unroutable one from the region that had no route at all.
  if (( phase == 2 )); then err="network unreachable"; else err="connect timeout"; fi
  echo "==> ClickHouse: ${SAMPLES} checks for ${region} (1 in ${FAIL_EVERY} fails: ${err})"
  ch -mn <<SQL
INSERT INTO monitor.check_results
  (org_id,target_id,region,timestamp,agent_id,status,duration_ms,dns_ms,connect_ms,tls_ms,ttfb_ms,response_code,error)
SELECT
  toUUID('${ORG}'), toUUID('${BLOCKED}'), '${region}',
  now() - toIntervalMinute(number),
  'seed-flap',
  if((number + ${phase}) % ${FAIL_EVERY} = 0, 'error', 'up'),
  if((number + ${phase}) % ${FAIL_EVERY} = 0, 5000, toUInt32(210 + (number % 90))),
  toUInt16(6 + (number % 5)),
  if((number + ${phase}) % ${FAIL_EVERY} = 0, NULL, toUInt16(230 + (number % 60))),
  if((number + ${phase}) % ${FAIL_EVERY} = 0, NULL, toUInt16(40 + (number % 25))),
  if((number + ${phase}) % ${FAIL_EVERY} = 0, NULL, toUInt16(90 + (number % 70))),
  if((number + ${phase}) % ${FAIL_EVERY} = 0, NULL, 200),
  if((number + ${phase}) % ${FAIL_EVERY} = 0, '${err}', '')
FROM numbers(${SAMPLES});

INSERT INTO monitor.check_results
  (org_id,target_id,region,timestamp,agent_id,status,duration_ms,dns_ms,connect_ms,tls_ms,ttfb_ms,response_code,error)
SELECT
  toUUID('${ORG}'), toUUID('${CLEAN}'), '${region}',
  now() - toIntervalMinute(number),
  'seed-flap',
  'up',
  toUInt32(180 + (number % 40)),
  toUInt16(5 + (number % 4)),
  toUInt16(60 + (number % 20)),
  toUInt16(35 + (number % 15)),
  toUInt16(70 + (number % 30)),
  200,
  ''
FROM numbers(${SAMPLES});
SQL
  phase=$(( phase + 1 ))
done

FAILED=$(ch -q "SELECT count() FROM monitor.check_results
                 WHERE target_id = toUUID('${BLOCKED}') AND status != 'up'")
TOTAL=$(ch -q "SELECT count() FROM monitor.check_results WHERE target_id = toUUID('${BLOCKED}')")

cat <<EOF

Seeded. ${FAILED} of ${TOTAL} checks failed on the blocked monitor, none of them
consecutive, so no incident opens and the incidents tab stays empty.

  monitor:   /targets/${BLOCKED}
  incidents: /targets/${BLOCKED}/incidents
  control:   /targets/${CLEAN}

Look for the flap count per region on the monitor page, and the explanation
under the empty table on the incidents tab.
EOF
