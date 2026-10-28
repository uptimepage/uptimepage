#!/usr/bin/env bash
# Seed certificate, DNS and domain monitors covering every state the check
# facts card renders. The running app probes them for real, so the card shows
# what the workers read, not hand-written JSON.
#
#   facts-tls-ok       tls_cert github.com                  up, two regions, one cert
#   facts-tls-warn     tls_cert cloudflare.com, warn 365d   degraded
#   facts-tls-expired  tls_cert expired.badssl.com          down, expired years ago
#   facts-dns-split    dns A cloudflare.com                 up, regions read different answers
#   facts-dns-cname    dns A www.github.com                 up, answered via a CNAME
#   facts-dns-set      dns A cloudflare.com, exact set      down, missing + unexpected
#   facts-dns-miss     dns TXT google.com, contains         down, no answer holds the value
#   facts-dns-empty    dns AAAA github.com                  down, no records
#   facts-domain-ok    domain github.com                    up, registrar
#   facts-domain-warn  domain cloudflare.com, warn 4000d    degraded
#
# The app probes only LIVE_REGION. The two-region monitors also sit on REGION2,
# which nothing probes while the dev-region agents are down. Once the live
# reading lands it is copied there: unchanged for the cert, so both regions fold
# into one group, and with other answers for the split.
#
# Env overrides:
#   SLUG          org slug to seed onto     (default: devorg)
#   PG_CONTAINER  postgres container name   (default: uptimepage-postgres-1)
#   CH_CONTAINER  clickhouse container      (default: uptimepage-clickhouse-1)
#   LIVE_REGION   region the app probes     (default: default)
#   REGION2       second, unprobed region   (default: eu-helsinki)
#   WAIT_SECS     wait for live readings    (default: 120)
#   BASE          app URL for the links     (default: http://app.lvh.me:8080)
#
# Idempotent: monitors tagged `seed-facts` and their ClickHouse history are
# wiped before re-insert.
#
# Requires the app running (`just run` or `just up-app`) + `just dev-login`.
set -euo pipefail

SLUG="${SLUG:-devorg}"
PG_CONTAINER="${PG_CONTAINER:-uptimepage-postgres-1}"
CH_CONTAINER="${CH_CONTAINER:-uptimepage-clickhouse-1}"
LIVE_REGION="${LIVE_REGION:-default}"
REGION2="${REGION2:-eu-helsinki}"
WAIT_SECS="${WAIT_SECS:-120}"
BASE="${BASE:-http://app.lvh.me:8080}"
TAG=seed-facts

pg() { docker exec -i "$PG_CONTAINER" psql -U monitor -d monitor -v ON_ERROR_STOP=1 "$@"; }
ch() { docker exec -i "$CH_CONTAINER" clickhouse-client "$@"; }
source "$(dirname "${BASH_SOURCE[0]}")/lib/seed-purge.sh"

ORG=$(pg -tAc "SELECT id FROM organizations WHERE slug='${SLUG}' AND deleted_at IS NULL")
if [[ -z "$ORG" ]]; then
  echo "error: org '${SLUG}' missing, run 'just dev-login' (or SLUG=… just dev-login) first" >&2
  exit 1
fi

if ! command -v jq >/dev/null; then
  echo "error: jq is required (brew install jq)" >&2
  exit 1
fi

# A live agent in REGION2 would probe the copied reading away. Captured first:
# under pipefail, grep -q quitting early can fail the pipe and skip this.
CONTAINERS=$(docker ps --format '{{.Names}}')
if grep -q '^uptimepage-agent-' <<<"$CONTAINERS"; then
  echo "error: dev-region agents are running and would overwrite the ${REGION2} readings." >&2
  echo "       Stop them first: just dev-regions-down" >&2
  exit 1
fi

for region in "$LIVE_REGION" "$REGION2"; do
  if [[ -z $(pg -tAc "SELECT 1 FROM regions WHERE id='${region}' AND enabled") ]]; then
    echo "error: region '${region}' is missing or disabled" >&2
    exit 1
  fi
done

echo "==> Postgres: drop the previous run's monitors"
pg -qc "DELETE FROM targets WHERE org_id = '${ORG}' AND tags @> ARRAY['${TAG}']"

# Before the insert: the app reads a new monitor within seconds, and the reset
# would delete that reading along with every other row of an unknown id.
echo "==> ClickHouse: wipe rows of monitors that no longer exist"
reset_seeded_history "$ORG" "$TAG"

echo "==> Postgres: create the monitors"
pg -q <<SQL
INSERT INTO monitor_groups (org_id, name) VALUES ('${ORG}', 'Check facts (seeded)') ON CONFLICT (org_id, name) DO NOTHING;
INSERT INTO targets (org_id, name, check_spec, interval_secs, tags, group_id, alert_confirmations)
SELECT '${ORG}', name, spec::jsonb, secs, ARRAY['${TAG}', tag], (SELECT id FROM monitor_groups WHERE org_id = '${ORG}' AND name = 'Check facts (seeded)'), confirmations
FROM (VALUES
  ('facts-tls-ok', 'github.com certificate', 3600, 1,
   '{"type":"tls_cert","host":"github.com","port":443,"server_name":null,"warn_days":30,"critical_days":7,"timeout":5000,"ip_version":"any"}'),
  ('facts-tls-warn', 'cloudflare.com certificate, wide warning', 3600, 1,
   '{"type":"tls_cert","host":"cloudflare.com","port":443,"server_name":null,"warn_days":365,"critical_days":7,"timeout":5000,"ip_version":"any"}'),
  ('facts-tls-expired', 'expired.badssl.com certificate', 3600, 1,
   '{"type":"tls_cert","host":"expired.badssl.com","port":443,"server_name":null,"warn_days":30,"critical_days":7,"timeout":5000,"ip_version":"any"}'),
  ('facts-dns-split', 'cloudflare.com A', 60, 2,
   '{"type":"dns","domain":"cloudflare.com","record_type":"A","resolver":"1.1.1.1","expected_contains":null,"expected_records":null,"timeout":3000}'),
  ('facts-dns-cname', 'www.github.com A', 60, 2,
   '{"type":"dns","domain":"www.github.com","record_type":"A","resolver":"1.1.1.1","expected_contains":null,"expected_records":null,"timeout":3000}'),
  ('facts-dns-set', 'cloudflare.com A, exact set', 60, 2,
   '{"type":"dns","domain":"cloudflare.com","record_type":"A","resolver":"1.1.1.1","expected_contains":null,"expected_records":["104.16.132.229","192.0.2.10"],"timeout":3000}'),
  ('facts-dns-miss', 'google.com TXT holds a value', 60, 2,
   '{"type":"dns","domain":"google.com","record_type":"TXT","resolver":"1.1.1.1","expected_contains":"v=spf9","expected_records":null,"timeout":3000}'),
  ('facts-dns-empty', 'github.com AAAA', 60, 2,
   '{"type":"dns","domain":"github.com","record_type":"AAAA","resolver":"1.1.1.1","expected_contains":null,"expected_records":null,"timeout":3000}'),
  ('facts-domain-ok', 'github.com registration', 43200, 1,
   '{"type":"domain_expiry","domain":"github.com","warn_days":60,"critical_days":14,"timeout":10000}'),
  ('facts-domain-warn', 'cloudflare.com registration, wide warning', 43200, 1,
   '{"type":"domain_expiry","domain":"cloudflare.com","warn_days":4000,"critical_days":14,"timeout":10000}')
) AS seed(tag, name, secs, confirmations, spec);

INSERT INTO target_regions (target_id, region)
SELECT id, '${LIVE_REGION}' FROM targets WHERE org_id = '${ORG}' AND tags @> ARRAY['${TAG}']
UNION ALL
SELECT id, '${REGION2}' FROM targets
 WHERE org_id = '${ORG}' AND tags && ARRAY['facts-tls-ok', 'facts-dns-split']
ON CONFLICT DO NOTHING;
SQL

id_of() { pg -tAc "SELECT id FROM targets WHERE org_id='${ORG}' AND tags @> ARRAY['$1']"; }

IDS=$(pg -tAc "SELECT string_agg(format('toUUID(%L)', id), ',') FROM targets
                WHERE org_id = '${ORG}' AND tags @> ARRAY['${TAG}']")
TOTAL=$(pg -tAc "SELECT count(*) FROM targets WHERE org_id = '${ORG}' AND tags @> ARRAY['${TAG}']")

echo "==> Waiting up to ${WAIT_SECS}s for the app to read all ${TOTAL} monitors in ${LIVE_REGION}"
deadline=$(( SECONDS + WAIT_SECS ))
while :; do
  read_count=$(ch -q "SELECT uniqExact(target_id) FROM monitor.check_results
                       WHERE org_id = toUUID('${ORG}') AND region = '${LIVE_REGION}'
                         AND target_id IN (${IDS})")
  printf '\r    %s/%s read' "$read_count" "$TOTAL"
  (( read_count >= TOTAL )) && break
  if (( SECONDS >= deadline )); then
    echo
    echo "warn: not every monitor was read in time; re-run with a bigger WAIT_SECS" >&2
    break
  fi
  sleep 5
done
echo

# Copies the newest live reading into REGION2, its details passed through FILTER.
mirror() {
  local tag="$1" filter="$2" id details
  id=$(id_of "$tag")
  details=$(ch -q "SELECT details FROM monitor.check_results
                    WHERE target_id = toUUID('${id}') AND region = '${LIVE_REGION}'
                      AND details IS NOT NULL
                    ORDER BY timestamp DESC LIMIT 1 FORMAT TSVRaw")
  if [[ -z "$details" ]]; then
    echo "    skip ${tag}: no live reading to copy yet"
    return
  fi
  local json
  json=$(jq -c "$filter" <<<"$details")
  # Query parameters are read in escaped format, so JSON backslashes are doubled.
  ch --param_details="${json//\\/\\\\}" -q "
    INSERT INTO monitor.check_results
    SELECT * REPLACE ('${REGION2}' AS region, '${TAG}' AS agent_id,
                      timestamp + 7 AS timestamp, now() AS ingested_at,
                      duration_ms + 38 AS duration_ms, {details:String} AS details)
    -- Filtered in a subquery: in the outer WHERE these names are the aliases.
    FROM (SELECT * FROM monitor.check_results
          WHERE target_id = toUUID('${id}') AND region = '${LIVE_REGION}' AND details IS NOT NULL
          ORDER BY timestamp DESC LIMIT 1)" </dev/null
  local copied
  copied=$(ch -q "SELECT count() FROM monitor.check_results
                   WHERE target_id = toUUID('${id}') AND region = '${REGION2}'" </dev/null)
  if (( copied == 0 )); then
    echo "error: ${tag}: copy to ${REGION2} wrote nothing" >&2
    exit 1
  fi
  echo "    ${tag}: copied to ${REGION2}"
}

echo "==> ClickHouse: second-region readings"
mirror facts-tls-ok '.'
mirror facts-dns-split '.answers = ["198.51.100.7", "198.51.100.8"]'

echo
echo "Seeded onto ${SLUG}. Newest reading per monitor in ${LIVE_REGION}:"
echo
# Read the list up front: `docker exec -i` inside the loop would eat its stdin.
mapfile -t SEEDED < <(pg -tAc "SELECT tags[2], id, plan_hold_at IS NOT NULL FROM targets
                                WHERE org_id = '${ORG}' AND tags @> ARRAY['${TAG}'] ORDER BY tags[2]")
for row in "${SEEDED[@]}"; do
  IFS='|' read -r tag id held <<<"$row"
  reading=$(ch -q "SELECT concat(toString(status), if(details IS NULL, ', no details', ''))
                    FROM monitor.check_results
                    WHERE target_id = toUUID('${id}') AND region = '${LIVE_REGION}'
                    ORDER BY timestamp DESC LIMIT 1")
  # The plan cap parks the newest monitors first, and a parked one is never read.
  [[ "$held" == t ]] && reading="held by the plan cap"
  printf '  %-18s %-24s %s/targets/%s\n' "$tag" "${reading:-not read yet}" "$BASE" "$id"
done
