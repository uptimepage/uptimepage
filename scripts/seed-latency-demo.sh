#!/usr/bin/env bash
# Seed ONE dense, phase-rich monitor with 30 days of ClickHouse history — for
# eyeballing the monitor-detail latency + breakdown charts across every range
# (1h / 24h / 7d / 30d). Verifies issue #29: switching range must re-scale the
# x-axis and reshape the series, and the breakdown chart needs per-phase data.
#
# Shape:
#   * one 'up' sample every 5 min for 30d (~8640 points) — dense at 1h, still
#     spans 30d so each range looks materially different
#   * dns/connect/tls/ttfb phase timings on every 'up' row → breakdown chart
#     renders all five bands (DNS/Connect/TLS/Server/Processing)
#   * periodic ttfb + processing spikes so p50 / p95 / p99 visibly separate
#   * a slow latency ramp (older = faster) so 30d shows a trend a 24h view hides
#   * sparse 'down' samples (phases NULL) so the line breaks and a TCP-style
#     all-NULL-phase path is exercised
#
# Idempotent: the target is tagged `seed-latency` and its rows are wiped (PG +
# CH) before re-insert, along with the CH rows of any monitor the org no longer has.
#
# Env overrides:
#   SLUG          org slug to seed onto       (default: devorg)
#   PG_CONTAINER  postgres container name     (default: uptimepage-postgres-1)
#   CH_CONTAINER  clickhouse container name   (default: uptimepage-clickhouse-1)
#   BASE_DOMAIN   for the printed URL         (default: lvh.me)
#
# Requires `just up-app` + `just dev-login` first (org must already exist) and
# a ClickHouse whose check_results_1m matview carries the per-phase avgState
# columns (i.e. a DB created after the issue-#29 migration change — a clean
# `just down-clean && just up-app`).
set -euo pipefail

SLUG="${SLUG:-devorg}"
PG_CONTAINER="${PG_CONTAINER:-uptimepage-postgres-1}"
CH_CONTAINER="${CH_CONTAINER:-uptimepage-clickhouse-1}"
BASE_DOMAIN="${BASE_DOMAIN:-lvh.me}"

pg() { docker exec -i "$PG_CONTAINER" psql -U monitor -d monitor -v ON_ERROR_STOP=1 "$@"; }
ch() { docker exec -i "$CH_CONTAINER" clickhouse-client "$@"; }
source "$(dirname "${BASH_SOURCE[0]}")/lib/seed-purge.sh"

if ! pg -tAc "SELECT 1 FROM organizations WHERE slug='${SLUG}'" | grep -q 1; then
  echo "error: org '${SLUG}' missing — run 'just dev-login' (or SLUG=… just dev-login) first" >&2
  exit 1
fi

ORG=$(pg -tAc "SELECT id FROM organizations WHERE slug='${SLUG}';")
: "${ORG:?org id query returned empty}"

echo "==> Postgres: (re)create the lat-demo monitors (dense 30d + short 30min)"
pg <<SQL
DELETE FROM targets
 WHERE org_id = '${ORG}' AND tags @> ARRAY['seed-latency'];

INSERT INTO targets
  (org_id, name, check_spec, interval_secs, enabled, tags)
VALUES
  ('${ORG}', 'lat-demo',
   '{"type":"http","url":"https://example.com/","method":"GET","timeout":5000,"follow_redirects":true,"max_redirects":3,"expected_status":{"kind":"exact","value":200},"headers":{},"verify_tls":true}'::jsonb,
   300, false, ARRAY['seed-latency']),
  ('${ORG}', 'lat-demo-short',
   '{"type":"http","url":"https://example.com/","method":"GET","timeout":5000,"follow_redirects":true,"max_redirects":3,"expected_status":{"kind":"exact","value":200},"headers":{},"verify_tls":true}'::jsonb,
   60, false, ARRAY['seed-latency']);
SQL

resolve_tid() {
  local name="$1"
  local tid
  tid=$(pg -tAc \
    "SELECT id FROM targets WHERE org_id='${ORG}' AND name='${name}' AND tags @> ARRAY['seed-latency'] LIMIT 1;")
  if [[ ${#tid} -ne 36 ]]; then
    echo "error: ${name} id='${tid}' — expected 36-char UUID" >&2
    exit 1
  fi
  printf '%s' "$tid"
}
TID=$(resolve_tid lat-demo)
TID_SHORT=$(resolve_tid lat-demo-short)

echo "==> ClickHouse: wipe rows for these monitors and for any prior run"
reset_seeded_history "$ORG" seed-latency

echo "==> ClickHouse: 30d dense history (~8640 'up' samples, 5-min cadence, with phases)"
# number = samples-ago; number=0 is "now". A 5-min step over 30 days needs
# 30*24*12 = 8640 rows. Phases compose the total; periodic +spikes push the
# tail percentiles apart, and a (8640-number)-keyed term ramps recent latency
# up so the 30d view shows a slope the 24h view can't.
ch -mn <<SQL
INSERT INTO monitor.check_results
  (org_id,target_id,timestamp,status,duration_ms,dns_ms,connect_ms,tls_ms,ttfb_ms,response_code)
SELECT
  toUUID('${ORG}'),
  toUUID('${TID}'),
  now() - toIntervalMinute(number * 5),
  'up',
  toUInt32(dns + con + tls + ttfb + app),
  toUInt16(dns), toUInt16(con), toUInt16(tls), toUInt16(ttfb),
  200
FROM
(
  SELECT
    number,
    8 + (number % 7)                                          AS dns,
    15 + (number % 11)                                        AS con,
    25 + (number % 17)                                        AS tls,
    40 + (number % 60)
       + intDiv(8640 - number, 288)                           -- recent-latency ramp (0..30ms)
       + if(number % 37 = 0, 220, 0)                          -- p95 spikes
       AS ttfb,
    15 + (number % 25)
       + if(number % 173 = 0, 600, 0)                         -- rare p99 spikes
       AS app
  FROM numbers(8640)
)
WHERE number % 211 != 0;  -- holes become 'down' rows below

-- Sparse outages: phases left NULL (defaults), so the line breaks over the
-- gap and the all-NULL-phase → 0 path is exercised on a real query.
INSERT INTO monitor.check_results
  (org_id,target_id,timestamp,status,duration_ms,response_code,error)
SELECT
  toUUID('${ORG}'), toUUID('${TID}'),
  now() - toIntervalMinute(number * 5),
  'down', 0, 503, 'connection refused'
FROM numbers(8640)
WHERE number % 211 = 0;
SQL

echo "==> ClickHouse: 30min short history (~30 'up' samples, 1-min cadence, with phases)"
# Less than the smallest (1h) range: only the most recent ~30 min carries data.
# At 1h the series fills the right half and the x-axis still spans a full hour;
# at 24h/7d/30d it collapses to a recent sliver — the "new monitor" case from #29.
ch -mn <<SQL
INSERT INTO monitor.check_results
  (org_id,target_id,timestamp,status,duration_ms,dns_ms,connect_ms,tls_ms,ttfb_ms,response_code)
SELECT
  toUUID('${ORG}'),
  toUUID('${TID_SHORT}'),
  now() - toIntervalMinute(number),
  'up',
  toUInt32(dns + con + tls + ttfb + app),
  toUInt16(dns), toUInt16(con), toUInt16(tls), toUInt16(ttfb),
  200
FROM
(
  SELECT
    number,
    8 + (number % 7)                       AS dns,
    15 + (number % 11)                      AS con,
    25 + (number % 17)                      AS tls,
    40 + (number % 30) + if(number % 9 = 0, 180, 0) AS ttfb,
    15 + (number % 20)                      AS app
  FROM numbers(30)
);
SQL

count_rows() {
  ch -mn -q "SELECT count() FROM monitor.check_results WHERE org_id=toUUID('${ORG}') AND target_id=toUUID('$1')"
}
echo "==> done:"
echo "    lat-demo       ($(count_rows "${TID}") rows, 30d)   http://${SLUG}.${BASE_DOMAIN}:8080/targets/${TID}"
echo "    lat-demo-short ($(count_rows "${TID_SHORT}") rows, 30min) http://${SLUG}.${BASE_DOMAIN}:8080/targets/${TID_SHORT}"
echo "    (self-host: drop the '${SLUG}.' subdomain.)"
echo "    lat-demo: flip 1h → 24h → 7d → 30d, axis + series change each time."
echo "    lat-demo-short: data < 1h — fills only the recent end of every range."
